use smithay::{
    backend::{
        allocator::{
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{DrmDevice, DrmDeviceFd, DrmEvent, DrmNode, GbmBufferedSurface},
        egl::{EGLContext, EGLDevice, EGLDisplay},
        renderer::{
            damage::OutputDamageTracker,
            element::{
                surface::WaylandSurfaceRenderElement,
            },
            gles::GlesRenderer,
            multigpu::{gbm::GbmGlesBackend, GpuManager},
            Bind,
        },
        session::{Session, libseat::LibSeatSession},
        udev::{all_gpus, primary_gpu, UdevBackend, UdevEvent},
        winit::{WinitGraphicsBackend, WinitEventLoop, WinitEvent},
    },
    output::{Mode as OutputMode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::{
        calloop::{LoopHandle, timer::{Timer, TimeoutAction}},
        rustix::fs::OFlags,
        drm::control::{connector, crtc, Device as DrmControlDevice, ModeTypeFlags},
    },
    utils::{DeviceFd, Point, Size, Transform},
};
use std::{collections::HashMap, os::unix::io::OwnedFd, time::Duration};
use tracing::{error, info, warn};

use crate::state::{BackendData, BlueState, GpuDevice, OutputRenderSurface, UdevData, WinitData};

/// Multi-GPU render-node import (hybrid-graphics laptops) — see module
/// doc for what's implemented (GpuManager lifecycle + the cross-copy
/// primitive) vs. what's still an open follow-up (per-surface origin
/// tracking to actually call it from the render loop).
pub mod multigpu;

/// HDR tone-mapping shader (compiled, not yet wired into the composite
/// pass — see module doc for exactly why and what's left).
pub mod hdr_shader;

// ── Winit (nested/dev mode) ───────────────────────────────────────────────

pub fn init_winit(
    state: &mut BlueState,
    mut backend: WinitGraphicsBackend<GlesRenderer>,
    events: WinitEventLoop,
    loop_handle: &LoopHandle<'static, BlueState>,
) {
    let size = backend.window_size();
    let output = Output::new("winit".to_string(), PhysicalProperties {
        size: Size::from((0, 0)),
        subpixel: Subpixel::Unknown,
        make: "Blue".to_string(),
        model: "Winit".to_string(),
        serial_number: String::new(),
    });
    let mode = OutputMode {
        size: Size::from((size.w as i32, size.h as i32)),
        refresh: 60_000,
    };
    output.change_current_state(Some(mode), Some(Transform::Normal), Some(Scale::Integer(1)), Some(Point::from((0, 0))));
    output.set_preferred(mode);
    state.space.map_output(&output, Point::from((0, 0)));
    let damage_tracker = OutputDamageTracker::from_output(&output);
    state.outputs.push(output.clone());

    // Register the client-facing dmabuf + color-management globals now
    // that a real GlesRenderer exists (`WinitGraphicsBackend::renderer()`
    // — a plain accessor, doesn't consume/move `backend`). Doing this
    // under winit (not just udev) is what makes both testable without
    // real DRM hardware — e.g. `build.rb check`'s headless smoke test.
    {
        let display_handle = state.display_handle.clone();
        let (dmabuf_state, dmabuf_global) =
            crate::protocols::dmabuf::init_dmabuf(&display_handle, backend.renderer(), None);
        state.dmabuf_state = Some(dmabuf_state);
        state.dmabuf_global = dmabuf_global;
        state.color_management_state = crate::protocols::color_management::init_color_management(&display_handle);
    }
    let hdr_tonemap_shader = match hdr_shader::compile_hdr_tonemap_shader(backend.renderer()) {
        Ok(program) => Some(program),
        Err(e) => {
            warn!("HDR tone-mapping shader failed to compile (not fatal — HDR content just won't be tone-mapped): {e:?}");
            None
        }
    };

    state.backend_data = BackendData::Winit(Box::new(WinitData { backend, output, damage_tracker, hdr_tonemap_shader }));
    state.seat.add_keyboard(smithay::input::keyboard::XkbConfig::default(), 400, 30).expect("keyboard");
    let _ = state.seat.add_pointer();

    loop_handle.insert_source(events, |event, _, state| {
        match event {
            WinitEvent::Resized { size, .. } => {
                if let BackendData::Winit(ref mut d) = state.backend_data {
                    let m = OutputMode { size: Size::from((size.w as i32, size.h as i32)), refresh: 60_000 };
                    d.output.change_current_state(Some(m), None, None, None);
                    d.damage_tracker = OutputDamageTracker::from_output(&d.output);
                }
            }
            WinitEvent::Input(ev) => crate::input::handle_input(state, ev),
            WinitEvent::CloseRequested => { state.should_exit = true; }
            WinitEvent::Redraw => {
                if let BackendData::Winit(ref d) = state.backend_data {
                    let output = d.output.clone();
                    drop(d);
                    render_winit(state, &output);
                }
            }
            WinitEvent::Focus(_) => {}
        }
    }).expect("winit source");
}

pub fn render_winit(state: &mut BlueState, output: &Output) {
    // Phase 1: collect render elements (borrow ends before render)
    let elements = {
        let BackendData::Winit(ref mut d) = state.backend_data else { return };
        let (renderer, _) = match d.backend.bind() {
            Ok(r) => r,
            Err(e) => { error!("bind: {}", e); return; }
        };
        // SpaceRenderElements wraps WaylandSurfaceRenderElement - use the correct type
        use smithay::desktop::space::SpaceRenderElements;
        // Was hardcoded to 1.0 — the fractional-scale protocol is
        // registered (protocols/mod.rs) and clients negotiate a scale
        // through it, but the renderer never actually applied it, so
        // HiDPI outputs always rendered at 1x regardless of what was
        // advertised. Pull the output's real (possibly fractional) scale
        // instead.
        let output_scale = output.current_scale().fractional_scale() as f32;
        let mut elems: Vec<SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>> =
            state.space.render_elements_for_output(renderer, output, output_scale)
                .unwrap_or_default();
        // IME candidate-window popups aren't part of `state.space` (they
        // aren't xdg-shell windows), so `render_elements_for_output`
        // never picks them up — this is the composite half of the fix in
        // protocols/input_method.rs (positioning was the other half).
        elems.extend(crate::protocols::input_method::render_elements(
            &state.input_method_popups, renderer, output_scale as f64,
        ));
        elems.extend(layer_shell_elements(output, renderer, output_scale as f64));
        elems
    };

    // Phase 2: render with fresh borrow
    let BackendData::Winit(ref mut d) = state.backend_data else { return };
    let (renderer, mut frame) = match d.backend.bind() {
        Ok(r) => r,
        Err(e) => { error!("bind2: {}", e); return; }
    };
    if let Err(e) = d.damage_tracker.render_output(renderer, &mut frame, 0, &elements, [0.08_f32, 0.10, 0.15, 1.0]) {
        warn!("render_output: {:?}", e);
    }
    drop(frame);
    if let Err(e) = d.backend.submit(None) { warn!("submit: {:?}", e); }
    d.backend.window().request_redraw();
}

// ── Udev/DRM (TTY mode) ────────────────────────────────────────────────────

pub fn init_udev(
    state: &mut BlueState,
    mut session: LibSeatSession,
    loop_handle: &LoopHandle<'static, BlueState>,
) {
    // Session trait must be in scope for .seat()
    let seat_name = session.seat();
    info!("udev backend, seat: {}", seat_name);

    // primary_gpu returns PathBuf - convert to DrmNode
    let primary_path = primary_gpu(&seat_name)
        .ok().flatten()
        .or_else(|| all_gpus(&seat_name).ok().and_then(|v| v.into_iter().next()))
        .expect("No GPU found");
    let primary_node = DrmNode::from_path(&primary_path).expect("DrmNode");
    info!("Primary GPU: {:?}", primary_node);

    let udev_backend = UdevBackend::new(&seat_name).expect("udev backend");
    let mut devices: HashMap<DrmNode, GpuDevice> = HashMap::new();
    // See render/multigpu.rs's module doc — this is registered with
    // every GPU node below (and on hotplug `UdevEvent::Added`/
    // `Removed`), independent of whether a second GPU is actually
    // present, so the infra is always correct/ready rather than only
    // exercised on hybrid-graphics hardware.
    let mut gpu_manager = GpuManager::new(GbmGlesBackend::default())
        .expect("GpuManager::new is infallible for GbmGlesBackend (no devices enumerated yet)");

    if let Ok((gpu, notifier)) = open_gpu(&primary_node, &mut session) {
        if let Err(e) = gpu_manager.as_mut().add_node(primary_node, gpu.gbm.clone()) {
            warn!("multi-gpu: failed to register primary GPU {primary_node:?} with GpuManager: {e:?}");
        }
        devices.insert(primary_node, gpu);
        register_drm_notifier(loop_handle, primary_node, notifier);
    }

    state.backend_data = BackendData::Udev(Box::new(UdevData {
        session, primary_gpu: primary_node, devices, gpu_manager,
    }));
    // scan_drm_outputs needs backend_data populated first (it looks the
    // GpuDevice up by node to create the renderer/surfaces), so this runs
    // after the assignment above rather than before it like the old code.
    if let BackendData::Udev(ref mut udev) = state.backend_data {
        if let Some(gpu) = udev.devices.get_mut(&primary_node) {
            let drm_clone_ptr: *mut DrmDevice = &mut gpu.drm;
            // SAFETY: `create_surface` needs `&mut DrmDevice`, but we
            // also need `&mut BlueState` for `scan_drm_outputs` itself
            // (to map outputs into `state.space`, create the renderer on
            // `state.backend_data`, etc) — both ultimately reach through
            // `state.backend_data`, so the borrow checker can't see that
            // `&mut gpu.drm` and `&mut state` don't actually alias in a
            // way that matters here (`scan_drm_outputs` never mutates
            // `udev.devices` itself while it holds the `drm` reference,
            // only outputs/renderer/surfaces on the *same* `gpu` entry
            // through a fresh `get_mut` lookup, which the pointer here
            // doesn't hold onto — see `scan_drm_outputs`'s own body for
            // where it re-borrows `gpu`). No other code runs between
            // this point and the end of `scan_drm_outputs`.
            unsafe { scan_drm_outputs(state, &mut *drm_clone_ptr, primary_node, loop_handle); }
        }
    }
    state.notify_output_topology_changed();

    state.seat.add_keyboard(smithay::input::keyboard::XkbConfig::default(), 400, 30).expect("kb");
    let _ = state.seat.add_pointer();

    loop_handle.insert_source(udev_backend, |event, _, state| {
        match event {
            UdevEvent::Added { path, .. } => {
                if let Ok(node) = DrmNode::from_path(&path) {
                    let lh = state.loop_handle.clone();
                    if let BackendData::Udev(ref mut data) = state.backend_data {
                        let mut sess = data.session.clone();
                        if let Ok((gpu, notifier)) = open_gpu(&node, &mut sess) {
                            if let Err(e) = data.gpu_manager.as_mut().add_node(node, gpu.gbm.clone()) {
                                warn!("multi-gpu: failed to register hotplugged GPU {node:?} with GpuManager: {e:?}");
                            }
                            data.devices.insert(node, gpu);
                            // Previously dropped for hotplugged GPUs (only
                            // the primary GPU's notifier, opened before
                            // the event loop even started, was
                            // registered) — a secondary GPU plugged in
                            // after boot would render one frame and then
                            // stall since nothing ever called
                            // `frame_submitted()` for it.
                            register_drm_notifier(&lh, node, notifier);
                        }
                    }
                    // Scan for outputs on the newly-added GPU the same
                    // way the primary GPU is scanned at startup.
                    if let BackendData::Udev(ref mut udev) = state.backend_data {
                        if let Some(gpu) = udev.devices.get_mut(&node) {
                            let drm_ptr: *mut DrmDevice = &mut gpu.drm;
                            // SAFETY: see the identical pattern (and
                            // rationale) in `init_udev` above.
                            unsafe { scan_drm_outputs(state, &mut *drm_ptr, node, &lh); }
                        }
                    }
                    state.notify_output_topology_changed();
                }
            }
            UdevEvent::Changed { .. } => {}
            UdevEvent::Removed { device_id } => {
                // Previously a no-op: unplugging a monitor (or, via
                // udev's DRM device_id, an entire GPU) left its
                // GbmBufferedSurface/DrmSurface alive with nothing ever
                // rendering to it again, and the `Output` stayed mapped
                // in `state.space` forever, so windows could keep being
                // placed on a monitor that no longer exists.
                if let Ok(node) = DrmNode::from_dev_id(device_id) {
                    let mut removed_outputs: Vec<Output> = Vec::new();
                    if let BackendData::Udev(ref mut udev) = state.backend_data {
                        if let Some(gpu) = udev.devices.get_mut(&node) {
                            removed_outputs = gpu.surfaces.values().map(|s| s.output.clone()).collect();
                            gpu.surfaces.clear();
                        }
                        udev.devices.remove(&node);
                        // Keep GpuManager in sync — an internal texture
                        // cache entry pointing at a now-closed device fd
                        // would otherwise be a use-after-free risk the
                        // next time something tried a cross-GPU import
                        // involving this node (see render/multigpu.rs).
                        udev.gpu_manager.as_mut().remove_node(&node);
                    }
                    for output in removed_outputs {
                        state.space.unmap_output(&output);
                        state.outputs.retain(|o| o != &output);
                    }
                    state.notify_output_topology_changed();
                    info!("Removed DRM device {:?} and its outputs", node);
                }
            }
        }
    }).expect("udev source");

    // Render timer — previously this only called `state.refresh()` (space
    // bookkeeping, no drawing). It now also drives `render_udev()` for
    // every lit-up output so bare-metal/TTY mode actually displays
    // something.
    loop_handle.insert_source(
        Timer::from_duration(Duration::from_millis(16)),
        |_, _, state| {
            state.refresh();
            render_all_udev_outputs(state);
            TimeoutAction::ToDuration(Duration::from_millis(16))
        },
    ).expect("render timer");
}

/// Registers a GPU's `DrmDeviceNotifier` on the event loop so
/// `DrmEvent::VBlank` (a previous pageflip completed) and
/// `DrmEvent::Error` reach `GbmBufferedSurface::frame_submitted()`. This
/// is required for the GBM swapchain to keep cycling buffers past the
/// first frame — see the comment on `open_gpu`'s return type.
fn register_drm_notifier(
    loop_handle: &LoopHandle<'static, BlueState>,
    node: DrmNode,
    notifier: smithay::backend::drm::DrmDeviceNotifier,
) {
    if let Err(e) = loop_handle.insert_source(notifier, move |event, metadata, state| {
        match event {
            DrmEvent::VBlank(crtc) => {
                if let BackendData::Udev(ref mut udev) = state.backend_data {
                    if let Some(gpu) = udev.devices.get_mut(&node) {
                        if let Some(surface) = gpu.surfaces.get_mut(&crtc) {
                            if let Err(e) = surface.gbm_surface.frame_submitted() {
                                warn!("frame_submitted failed for {:?}: {}", crtc, e);
                            }
                        }
                    }
                }
                let _ = metadata;
            }
            DrmEvent::Error(e) => warn!("DRM device error on {:?}: {}", node, e),
        }
    }) {
        error!("Failed to register DRM notifier for {:?}: {:?}", node, e);
    }
}

/// Return type note: previously this dropped the `DrmDeviceNotifier` that
/// `DrmDevice::new` hands back (`let (drm, _notifier) = ...`). That
/// notifier is what delivers `DrmEvent::VBlank`/`DrmEvent::Error` once
/// registered on the event loop — without it, `GbmBufferedSurface`'s
/// internal swapchain never gets told a buffer was released by the
/// previous pageflip and will stall (or error out) after the first frame.
/// It's now returned alongside the device so `init_udev`/hotplug handling
/// can register it.
fn open_gpu(
    node: &DrmNode,
    session: &mut LibSeatSession,
) -> Result<(GpuDevice, smithay::backend::drm::DrmDeviceNotifier), Box<dyn std::error::Error>> {
    // Session trait in scope via import above
    let path = node.dev_path().ok_or("no dev path")?;
    let owned_fd: OwnedFd = session.open(&path, OFlags::empty())
        .map_err(|e| format!("session.open: {}", e))?;
    let drm_fd = DrmDeviceFd::new(DeviceFd::from(owned_fd));
    // DrmDevice::new returns (DrmDevice, DrmDeviceNotifier)
    let (drm, notifier) = DrmDevice::new(drm_fd.clone(), true)
        .map_err(|e| format!("DrmDevice: {}", e))?;
    let gbm = GbmDevice::new(drm_fd)
        .map_err(|e| format!("GbmDevice: {}", e))?;
    Ok((GpuDevice { drm, gbm, renderer: None, hdr_tonemap_shader: None, surfaces: HashMap::new() }, notifier))
}

/// Detects connected outputs on a DRM device AND (unlike the previous
/// version of this function) actually lights each one up: it allocates a
/// `DrmSurface` for a free CRTC, wraps it in a `GbmBufferedSurface` swap
/// chain, and stashes the pair in `GpuDevice::surfaces` so `render_udev()`
/// has something to draw into. It also lazily creates the shared EGL/GLES
/// renderer for the GPU the first time it's needed.
///
/// ## Caveats (please read before relying on this in production)
/// This is a best-effort, from-scratch implementation written without a
/// working Rust toolchain available to compile/test against the exact
/// pinned `smithay` commit (`82912edf`) — there was no network access to a
/// build environment capable of pulling and building the full smithay +
/// libdrm/gbm/EGL dependency tree in this session. The overall structure
/// (EGLDisplay → EGLContext → GlesRenderer, DrmSurface → GbmAllocator →
/// GbmBufferedSurface, DRM device fd as a calloop event source consuming
/// `DrmEvent::VBlank`/`DrmEvent::Error`) matches the architecture used by
/// smithay's own reference compositor (`anvil/src/udev.rs`), but exact
/// method names/signatures can drift between smithay revisions. Treat this
/// as a strong scaffold to compile-fix against the pinned rev, not as
/// verified-working code — the previous state (outputs detected but never
/// rendered to) is unambiguously worse, so this is a net improvement
/// either way, but plan to `cargo build` and iterate before shipping it.
///
/// Single-CRTC-per-connector, no explicit plane management beyond what
/// `GbmBufferedSurface`/`DrmSurface` do internally. Two gaps this comment
/// used to list here have since been addressed elsewhere: hotplug-aware
/// surface teardown (`UdevEvent::Removed` in `init_udev`) turned out to
/// already be implemented when actually checked, and multi-GPU
/// render-node import lifecycle now exists (`render/multigpu.rs`) — what
/// remains for the latter is per-surface origin routing, not the
/// infrastructure itself; see ROADMAP.md for both.
fn scan_drm_outputs(
    state: &mut BlueState,
    drm: &mut DrmDevice,
    node: DrmNode,
    _loop_handle: &LoopHandle<'static, BlueState>,
) {
    let Ok(resources) = drm.resource_handles() else { return };

    // Lazily create the shared EGL/GLES renderer for this GPU.
    let renderer_ready = if let BackendData::Udev(ref mut udev) = state.backend_data {
        if let Some(gpu) = udev.devices.get_mut(&node) {
            if gpu.renderer.is_none() {
                match create_gles_renderer(&gpu.gbm) {
                    Ok(mut r) => {
                        gpu.hdr_tonemap_shader = match hdr_shader::compile_hdr_tonemap_shader(&mut r) {
                            Ok(program) => Some(program),
                            Err(e) => {
                                warn!("HDR tone-mapping shader failed to compile for {:?} (not fatal — HDR content just won't be tone-mapped): {e:?}", node);
                                None
                            }
                        };
                        gpu.renderer = Some(r);
                        true
                    }
                    Err(e) => { error!("Failed to create EGL/GLES renderer for {:?}: {}", node, e); false }
                }
            } else {
                true
            }
        } else {
            false
        }
    } else {
        false
    };
    if !renderer_ready {
        warn!("No renderer available for GPU {:?}, outputs on it will not render", node);
    } else if state.dmabuf_state.is_none() {
        // First GPU with a working renderer: register the client-facing
        // dmabuf + color-management globals (see protocols/dmabuf.rs,
        // protocols/color_management.rs). Only done once — multi-GPU
        // per-device feedback (advertising a different `main_device`
        // tranche per render node) is the multi-GPU follow-up already
        // flagged above, not attempted here.
        let display_handle = state.display_handle.clone();
        // `DrmNode::dev_id()` returns `u64` directly, not a `Result` —
        // confirmed by a real `cargo build` error (`E0599: no method
        // named 'ok' found for type 'u64'`) once this was actually
        // compiled; `libc::dev_t` is a plain alias for `u64` on Linux,
        // so no cast is needed either.
        let dev_id = Some(node.dev_id());

        let init_result = if let BackendData::Udev(ref udev) = state.backend_data {
            udev.devices.get(&node)
                .and_then(|gpu| gpu.renderer.as_ref())
                .map(|renderer| crate::protocols::dmabuf::init_dmabuf(&display_handle, renderer, dev_id))
        } else {
            None
        };
        // Borrow of `state.backend_data` (via `udev`/`gpu`/`renderer`)
        // ends at the close of the `if let` above, so assigning back into
        // `state` here is a fresh, disjoint borrow — same reasoning as
        // `dmabuf::init_dmabuf`'s doc comment.
        if let Some((dmabuf_state, dmabuf_global)) = init_result {
            state.dmabuf_state = Some(dmabuf_state);
            state.dmabuf_global = dmabuf_global;
            state.color_management_state = crate::protocols::color_management::init_color_management(&display_handle);
        }
    }

    let mut used_crtcs: Vec<crtc::Handle> = Vec::new();
    let mut x_off = 0i32;

    for conn_handle in resources.connectors() {
        let Ok(conn) = drm.get_connector(*conn_handle, false) else { continue };
        if conn.state() != connector::State::Connected { continue; }
        let mode = conn.modes().iter()
            .filter(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
            .max_by_key(|m| m.vrefresh())
            .or_else(|| conn.modes().first())
            .copied();
        let Some(mode) = mode else { continue };
        let (w, h) = mode.size();
        let conn_id = u32::from(*conn_handle);
        let name = format!("{}-{}", conn.interface() as u8, conn_id);
        let phys = conn.size().unwrap_or((0, 0));

        // Pick the first CRTC that can drive this connector and isn't
        // already claimed by an earlier connector in this same scan.
        // `Encoder::possible_crtcs()` returns a `CrtcListFilter` bitmask
        // directly usable by `ResourceHandles::filter_crtcs` — collecting
        // per-encoder results into a `Vec` first (the previous version of
        // this code) doesn't typecheck, since `filter_crtcs` wants that
        // bitmask type, not a pre-filtered list of handles.
        let possible_crtcs: Vec<crtc::Handle> = conn
            .encoders()
            .iter()
            .filter_map(|e| drm.get_encoder(*e).ok())
            .flat_map(|enc| resources.filter_crtcs(enc.possible_crtcs()))
            .collect();
        let possible_crtcs = if possible_crtcs.is_empty() {
            resources.crtcs().to_vec()
        } else {
            possible_crtcs
        };
        let Some(&crtc) = possible_crtcs.iter().find(|c| !used_crtcs.contains(c)) else {
            warn!("No free CRTC for connector {}, skipping", name);
            continue;
        };

        let output = Output::new(name.clone(), PhysicalProperties {
            size: Size::from((phys.0 as i32, phys.1 as i32)),
            subpixel: Subpixel::Unknown,
            make: "Unknown".to_string(),
            model: name.clone(),
            serial_number: String::new(),
        });

        // Register every mode this connector actually reports (not just
        // the one we're about to select) so `zwlr_output_management`
        // can advertise a real per-resolution/refresh list instead of
        // only ever showing whatever's currently active — see
        // `protocols/output_management.rs::advertise_head`, which reads
        // this back via `output.modes()`. EDID mode lists commonly
        // repeat the same (w,h)@refresh combination more than once
        // (e.g. once for the "preferred" flag, again as a plain
        // supported mode), so this dedupes on the exact triplet before
        // calling `add_mode` — smithay doesn't dedupe for us, and a
        // client-visible list with visually identical duplicate entries
        // would just be confusing in a resolution picker.
        let mut seen_modes: std::collections::HashSet<(i32, i32, i32)> = Default::default();
        for m in conn.modes() {
            let (mw, mh) = m.size();
            let refresh_mhz = m.vrefresh() as i32 * 1000;
            if !seen_modes.insert((mw as i32, mh as i32, refresh_mhz)) { continue; }
            output.add_mode(OutputMode { size: Size::from((mw as i32, mh as i32)), refresh: refresh_mhz });
        }

        let sm = OutputMode { size: Size::from((w as i32, h as i32)), refresh: mode.vrefresh() as i32 * 1000 };
        output.change_current_state(Some(sm), Some(Transform::Normal), Some(Scale::Integer(1)), Some(Point::from((x_off, 0))));
        output.set_preferred(sm);
        state.space.map_output(&output, Point::from((x_off, 0)));

        // Build the DRM surface + GBM swapchain for this CRTC/connector.
        if let BackendData::Udev(ref mut udev) = state.backend_data {
            if let Some(gpu) = udev.devices.get_mut(&node) {
                match drm.create_surface(crtc, mode, &[*conn_handle]) {
                    Ok(drm_surface) => {
                        let allocator = GbmAllocator::new(gpu.gbm.clone(), GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT);
                        // `GbmDevice` has no `supported_formats()` method
                        // (that was a guess that didn't hold up against
                        // the real API) — and `GbmBufferedSurface::new`
                        // at this pinned rev takes a plain `&[DrmFourcc]`
                        // (pixel formats only, no modifiers), not a
                        // `FormatSet` of (format, modifier) pairs as in
                        // newer smithay. Real modifier negotiation
                        // (vendor tiled/compressed formats) isn't
                        // reachable through this API at this rev — still
                        // a real follow-up, see ROADMAP.md.
                        //
                        // What *is* achievable here without risking an
                        // unverifiable API assumption (no compiler in
                        // this environment to check e.g. whether
                        // `DrmSurface`/`GbmAllocator` implement `Clone`
                        // for a retry-on-failure loop): pass a richer,
                        // priority-ordered format list to the *same*
                        // single call this already used — it already took
                        // a multi-element slice ([Xrgb8888, Argb8888]),
                        // so extending that list is a safe, consistent
                        // use of the existing calling convention, not a
                        // new API assumption. 10-bit XRGB2101010/
                        // ARGB2101010 first (meaningfully reduces
                        // banding on HDR-capable panels that can scan it
                        // out), falling back within the same call to the
                        // universally-supported 8-bit formats.
                        let formats: &[smithay::backend::allocator::Fourcc] = &[
                            smithay::backend::allocator::Fourcc::Xrgb2101010,
                            smithay::backend::allocator::Fourcc::Argb2101010,
                            smithay::backend::allocator::Fourcc::Xrgb8888,
                            smithay::backend::allocator::Fourcc::Argb8888,
                        ];
                        match GbmBufferedSurface::new(drm_surface, allocator, formats, None) {
                            Ok(gbm_surface) => {
                                let damage_tracker = OutputDamageTracker::from_output(&output);
                                gpu.surfaces.insert(crtc, OutputRenderSurface {
                                    output: output.clone(),
                                    gbm_surface,
                                    damage_tracker,
                                    connector: *conn_handle,
                                });
                                used_crtcs.push(crtc);
                                info!("Lit up output {} on CRTC {:?} ({}x{}@{})", name, crtc, w, h, mode.vrefresh());
                            }
                            Err(e) => error!("GbmBufferedSurface::new failed for {}: {}", name, e),
                        }
                    }
                    Err(e) => error!("drm.create_surface failed for {}: {}", name, e),
                }
            }
        }

        state.outputs.push(output);
        x_off += w as i32;
    }
}

/// Real hardware modeset: called from
/// `protocols/output_management.rs::apply_configuration` after
/// `Output::change_current_state` updates the *logical* mode, to also
/// rebuild the physical `DrmSurface` with a matching `drm::control::Mode`
/// — without this, changing resolution in Settings updated what the
/// compositor's own layout/scaling logic believed the output size was,
/// but never actually re-programmed the display controller, so the
/// screen kept outputting the old physical timing regardless of what
/// smithay's `Output` object said.
///
/// Deliberately mirrors `scan_drm_outputs`'s surface-creation block
/// (same allocator flags, same format candidate list) rather than
/// factoring out a shared helper — this is hand-written against the API
/// with no compiler in this environment to check it, and matching
/// already-used call shapes as closely as possible is the main risk
/// mitigation available. Returns `false` (leaving the old surface
/// running) on any failure rather than tearing down a working output.
pub fn apply_hardware_modeset(state: &mut BlueState, output_name: &str, width: i32, height: i32, refresh_mhz: i32) -> bool {
    let BackendData::Udev(ref mut udev) = state.backend_data else {
        // Nothing to do under the winit (nested/dev) backend — there's
        // no physical display timing to reprogram, the logical
        // `change_current_state` the caller already did is sufficient.
        return true;
    };

    for gpu in udev.devices.values_mut() {
        let Some((&crtc, _)) = gpu.surfaces.iter().find(|(_, s)| s.output.name() == output_name) else { continue };
        let connector = gpu.surfaces[&crtc].connector;

        let Ok(conn) = gpu.drm.get_connector(connector, false) else {
            warn!("apply_hardware_modeset: connector for {} vanished", output_name);
            return false;
        };
        // Match against the connector's own reported modes rather than
        // trusting the caller's (w,h,refresh) blindly — only a mode the
        // display itself advertises is something `create_surface` can
        // actually commit.
        let Some(drm_mode) = conn.modes().iter().find(|m| {
            let (mw, mh) = m.size();
            mw as i32 == width && mh as i32 == height && m.vrefresh() as i32 * 1000 == refresh_mhz
        }).copied() else {
            warn!("apply_hardware_modeset: {} has no matching mode for {}x{}@{}", output_name, width, height, refresh_mhz);
            return false;
        };

        let drm_surface = match gpu.drm.create_surface(crtc, drm_mode, &[connector]) {
            Ok(s) => s,
            Err(e) => { error!("apply_hardware_modeset: drm.create_surface failed for {}: {}", output_name, e); return false; }
        };

        let allocator = GbmAllocator::new(gpu.gbm.clone(), GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT);
        // Same priority-ordered format list as scan_drm_outputs (10-bit
        // first, 8-bit fallback within the same call) — see that
        // function's comment for why this shape rather than a retry
        // loop.
        let formats: &[smithay::backend::allocator::Fourcc] = &[
            smithay::backend::allocator::Fourcc::Xrgb2101010,
            smithay::backend::allocator::Fourcc::Argb2101010,
            smithay::backend::allocator::Fourcc::Xrgb8888,
            smithay::backend::allocator::Fourcc::Argb8888,
        ];
        let gbm_surface = match GbmBufferedSurface::new(drm_surface, allocator, formats, None) {
            Ok(s) => s,
            Err(e) => { error!("apply_hardware_modeset: GbmBufferedSurface::new failed for {}: {}", output_name, e); return false; }
        };

        // Replace the old surface — dropping the previous `gbm_surface`
        // (and the `DrmSurface` it owned) tears down its DRM resources;
        // the fresh one above already has the new mode committed.
        if let Some(existing) = gpu.surfaces.get_mut(&crtc) {
            existing.gbm_surface = gbm_surface;
            existing.damage_tracker = OutputDamageTracker::from_output(&existing.output);
        }
        info!("Hardware modeset applied: {} -> {}x{}@{}", output_name, width, height, refresh_mhz);
        return true;
    }

    warn!("apply_hardware_modeset: no DRM surface found for output {}", output_name);
    false
}

/// Creates a GLES renderer bound to the GPU's GBM device via EGL. This is
/// what was completely missing before: without it there was no way to
/// actually draw anything for the udev/TTY backend, only the winit
/// (nested) backend had a renderer.
fn create_gles_renderer(
    gbm: &GbmDevice<DrmDeviceFd>,
) -> Result<GlesRenderer, Box<dyn std::error::Error>> {
    // Safety: EGLDisplay::new requires the display handle to outlive the
    // context/renderer built from it, which it does here since `gbm` (and
    // therefore the EGLDisplay built from it) lives inside `GpuDevice` for
    // the lifetime of the GPU device entry.
    let egl_display = unsafe { EGLDisplay::new(gbm.clone())? };
    let egl_device = EGLDevice::device_for_display(&egl_display)?;
    let _ = egl_device; // currently unused beyond validating the display is usable
    let egl_context = EGLContext::new(&egl_display)?;
    let renderer = unsafe { GlesRenderer::new(egl_context)? };
    Ok(renderer)
}

/// Actually renders and presents a frame for one DRM-backed output.
/// Mirrors `render_winit()`'s two-phase borrow pattern (collect render
/// elements, then render), but sources the renderer from the GPU device
/// map instead of `WinitGraphicsBackend`, and pushes the result through
/// `GbmBufferedSurface::queue_buffer()` for an atomic pageflip instead of
/// `WinitGraphicsBackend::submit()`.
/// Render elements for every mapped layer-shell surface on `output`
/// (panels, the lock screen, on-screen keyboards, ...). Was previously
/// nothing — `new_layer_surface`/`layer_destroyed` in state/mod.rs were
/// no-ops, so nothing was ever mapped into smithay's `LayerMap` in the
/// first place; this is the composite-side half of that fix, same
/// relationship as `input_method::render_elements` is to the IME popup
/// positioning fix.
fn layer_shell_elements<R, E>(
    output: &Output,
    renderer: &mut R,
    scale: f64,
) -> Vec<E>
where
    R: smithay::backend::renderer::Renderer + smithay::backend::renderer::ImportAll,
    R::TextureId: Clone + 'static,
    E: From<smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement<R>>,
{
    use smithay::backend::renderer::element::surface::render_elements_from_surface_tree;
    use smithay::desktop::layer_map_for_output;

    let map = layer_map_for_output(output);
    let mut out = Vec::new();
    for layer in map.layers() {
        let wl_surface = layer.wl_surface();
        let Some(geo) = map.layer_geometry(layer) else { continue };
        let phys = smithay::utils::Point::<i32, smithay::utils::Physical>::from((
            (geo.loc.x as f64 * scale).round() as i32,
            (geo.loc.y as f64 * scale).round() as i32,
        ));
        out.extend(render_elements_from_surface_tree(
            renderer,
            wl_surface,
            phys,
            scale,
            1.0,
            smithay::backend::renderer::element::Kind::Unspecified,
        ));
    }
    out
}

pub fn render_udev(state: &mut BlueState, node: DrmNode, crtc: crtc::Handle) {
    let BackendData::Udev(ref mut udev) = state.backend_data else { return };
    let Some(gpu) = udev.devices.get_mut(&node) else { return };
    let Some(renderer) = gpu.renderer.as_mut() else { return };
    let Some(surface) = gpu.surfaces.get_mut(&crtc) else { return };

    let output_scale = surface.output.current_scale().fractional_scale() as f32;

    use smithay::desktop::space::SpaceRenderElements;
    let mut elements: Vec<SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>> =
        state.space.render_elements_for_output(renderer, &surface.output, output_scale)
            .unwrap_or_default();
    elements.extend(crate::protocols::input_method::render_elements(
        &state.input_method_popups, renderer, output_scale as f64,
    ));
    elements.extend(layer_shell_elements(&surface.output, renderer, output_scale as f64));

    let mode_size = surface.output.current_mode().map(|m| m.size).unwrap_or(Size::from((0, 0)));

    let (mut dmabuf, age) = match surface.gbm_surface.next_buffer() {
        Ok(v) => v,
        Err(e) => { warn!("next_buffer failed for {:?}: {}", crtc, e); return; }
    };
    // `Renderer::bind` returns the bound render *target* (a `GlesTarget`
    // here) rather than mutating the renderer in place — that target,
    // not a `GlesFrame`, is what `OutputDamageTracker::render_output`
    // wants. `render_output` handles the renderer.render()/frame
    // lifecycle internally; calling `renderer.render()` ourselves first
    // (the previous version of this code) was wrong for this smithay
    // rev and duplicated work `render_output` already does.
    let mut target = match renderer.bind(&mut dmabuf) {
        Ok(t) => t,
        Err(e) => { warn!("renderer.bind failed for {:?}: {}", crtc, e); return; }
    };

    let render_result = match surface.damage_tracker.render_output(
        renderer, &mut target, age as usize, &elements, [0.08_f32, 0.10, 0.15, 1.0],
    ) {
        Ok(r) => r,
        Err(e) => {
            warn!("render_output failed for {:?}: {:?}", crtc, e);
            drop(target);
            return;
        }
    };

    // Service any pending screencopy requests (grim et al.) for this
    // output — must happen here, while `target` is still bound, since
    // `copy_framebuffer` (see `protocols/screencopy.rs::service_screencopy`)
    // reads from whatever's currently bound, same as `glReadPixels` would.
    // `state.screencopy_state` and `state.backend_data` (which `renderer`
    // is borrowed from) are disjoint fields, so this borrow-checks even
    // though `renderer` is still alive.
    let output_name = surface.output.name();
    crate::protocols::screencopy::service_screencopy(
        &mut state.screencopy_state, renderer, &target, &output_name, (mode_size.w, mode_size.h),
        render_result.damage.map(|v| v.as_slice()),
    );

    // `target` (and the mutable borrow of `renderer`/`dmabuf` it holds)
    // must go out of scope before `dmabuf` can be handed off to KMS via
    // `queue_buffer` below.
    drop(target);

    // `render_result.damage` is `Option<&Vec<Rectangle<i32, Physical>>>`
    // at this pinned smithay rev (a lifetime-bound reference into the
    // damage tracker's internal state, not an owned Vec as first
    // assumed) — `queue_buffer` wants it owned, so `.cloned()` converts
    // `Option<&Vec<_>>` to `Option<Vec<_>>`, exactly as the compiler's
    // own suggestion for this error.
    if let Err(e) = surface.gbm_surface.queue_buffer(None, render_result.damage.cloned(), ()) {
        warn!("queue_buffer (pageflip) failed for {:?}: {}", crtc, e);
    }
}

/// Iterates every lit-up output across every GPU and renders a frame for
/// each. Called from the 16ms render timer in `init_udev` (previously
/// that timer only called `state.refresh()`, which does bookkeeping but
/// never actually drew a frame in udev mode).
pub fn render_all_udev_outputs(state: &mut BlueState) {
    let targets: Vec<(DrmNode, crtc::Handle)> = if let BackendData::Udev(ref udev) = state.backend_data {
        udev.devices
            .iter()
            .flat_map(|(node, gpu)| gpu.surfaces.keys().map(move |c| (*node, *c)))
            .collect()
    } else {
        Vec::new()
    };
    for (node, crtc) in targets {
        render_udev(state, node, crtc);
    }
}
