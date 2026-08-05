use std::collections::VecDeque;
use tracing::warn;

use smithay::{
    output::Output,
    reexports::wayland_server::{
        backend::GlobalId, protocol::{wl_buffer::WlBuffer, wl_shm}, Client, DataInit, Dispatch,
        DisplayHandle, GlobalDispatch, New,
    },
};
use wayland_protocols_wlr::screencopy::v1::server::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
};

use crate::state::BlueState;

pub struct ScreencopyState {
    global: GlobalId,
    /// Frames that have received a `copy` request and are waiting for
    /// the next `render_udev`/`render_winit` pass to service them. Kept
    /// as a queue (rather than serviced synchronously inside the
    /// `Dispatch` handler) because the render pipeline runs on the fixed
    /// 16ms timer, not on-demand — servicing here would mean rendering
    /// twice per frame on every screenshot.
    pending: VecDeque<PendingCapture>,
}

struct PendingCapture {
    frame: ZwlrScreencopyFrameV1,
    buffer: WlBuffer,
    output_name: String,
    /// True for `copy_with_damage` — per protocol, that request means
    /// "don't complete until the output has actually changed since this
    /// frame object was created", as opposed to plain `copy` which
    /// completes immediately with whatever's on screen right now. Used
    /// by screen-recording clients (wf-recorder, OBS's wlr-screencopy
    /// source) specifically so they don't have to encode/transmit
    /// identical frames when nothing changed.
    with_damage: bool,
}

pub struct ScreencopyGlobalData;

impl ScreencopyState {
    pub fn new(display: &DisplayHandle) -> Self {
        let global = display.create_global::<BlueState, ZwlrScreencopyManagerV1, _>(3, ScreencopyGlobalData);
        Self { global, pending: VecDeque::new() }
    }

    pub fn global_id(&self) -> &GlobalId {
        &self.global
    }
}

impl GlobalDispatch<ZwlrScreencopyManagerV1, ScreencopyGlobalData> for BlueState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrScreencopyManagerV1>,
        _global_data: &ScreencopyGlobalData,
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for BlueState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ZwlrScreencopyManagerV1,
        request: zwlr_screencopy_manager_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        use zwlr_screencopy_manager_v1::Request;
        match request {
            Request::CaptureOutput { frame, overlay_cursor: _, output } => {
                let output_name = Output::from_resource(&output)
                    .map(|o| o.name())
                    .unwrap_or_default();
                let f = data_init.init(frame, ());
                send_buffer_offer(state, &f, &output_name, None);
            }
            Request::CaptureOutputRegion { frame, overlay_cursor: _, output, x, y, width, height } => {
                let output_name = Output::from_resource(&output)
                    .map(|o| o.name())
                    .unwrap_or_default();
                let f = data_init.init(frame, ());
                send_buffer_offer(state, &f, &output_name, Some((x, y, width, height)));
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for BlueState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwlrScreencopyFrameV1,
        request: zwlr_screencopy_frame_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        use zwlr_screencopy_frame_v1::Request;
        match request {
            Request::Copy { buffer } => {
                let output_name = state
                    .screencopy_state
                    .pending
                    .iter()
                    .find(|p| p.frame == *resource)
                    .map(|p| p.output_name.clone())
                    .unwrap_or_default();
                state.screencopy_state.pending.push_back(PendingCapture {
                    frame: resource.clone(), buffer, output_name, with_damage: false,
                });
            }
            Request::CopyWithDamage { buffer } => {
                let output_name = state
                    .screencopy_state
                    .pending
                    .iter()
                    .find(|p| p.frame == *resource)
                    .map(|p| p.output_name.clone())
                    .unwrap_or_default();
                state.screencopy_state.pending.push_back(PendingCapture {
                    frame: resource.clone(), buffer, output_name, with_damage: true,
                });
            }
            Request::Destroy => {
                state.screencopy_state.pending.retain(|p| p.frame != *resource);
            }
            _ => {}
        }
    }
}

fn send_buffer_offer(
    state: &BlueState,
    frame: &ZwlrScreencopyFrameV1,
    output_name: &str,
    region: Option<(i32, i32, i32, i32)>,
) {
    let Some(output) = state.outputs.iter().find(|o| o.name() == output_name) else {
        frame.failed();
        return;
    };
    let Some(mode) = output.current_mode() else {
        frame.failed();
        return;
    };
    let (w, h) = region.map(|(_, _, w, h)| (w, h)).unwrap_or((mode.size.w, mode.size.h));
    let stride = w as u32 * 4;
    frame.buffer(wl_shm::Format::Argb8888, w as u32, h as u32, stride);
    // Newer protocol versions also want `linux_dmabuf`/`buffer_done` —
    // sending SHM-only here (via `buffer_done`) is protocol-valid and is
    // what most screenshot tools (grim) actually request; a dmabuf fast
    // path is a follow-up.
    frame.buffer_done();
}

/// Services every pending capture against the currently-bound render
/// target for `output_name`. This is a method on `ScreencopyState` (not a
/// free function taking `&mut BlueState`, which the first version of this
/// file used) specifically so it can be called from `render_udev` while
/// `renderer`/`target` are still borrowed from `state.backend_data` —
/// `state.screencopy_state` and `state.backend_data` are disjoint fields,
/// so `state.screencopy_state.service(renderer, ...)` borrow-checks fine
/// even while `renderer` itself traces back to a live borrow of
/// `state.backend_data`, whereas passing the whole `&mut state` would not.
///
/// # Pixel copy — the part most likely to need on-hardware iteration
/// Uses smithay's `ExportMem` trait (`copy_framebuffer` +
/// `map_texture`), which is the standard mechanism other Wayland
/// compositors built on smithay use for exactly this (screenshot/
/// screencast) purpose. It must run while the render target from
/// `renderer.bind()` is still alive — `copy_framebuffer` reads from
/// whatever's currently bound, the same way `glReadPixels` reads from the
/// currently-bound framebuffer in raw OpenGL. Call this **before**
/// dropping the bound target, not after.
pub fn service_screencopy(
    screencopy: &mut ScreencopyState,
    renderer: &mut smithay::backend::renderer::gles::GlesRenderer,
    target: &smithay::backend::renderer::gles::GlesTarget<'_>,
    output_name: &str,
    output_size: (i32, i32),
    // This pass's real damage for `output_name`, straight from the same
    // `OutputDamageTracker::render_output` call `render_udev` already
    // makes for the pageflip itself (see render/mod.rs) — `None` means
    // nothing changed at all this frame, `Some(&[])` a pathological
    // "changed but no regions" case treated the same as `None` below,
    // `Some(rects)` the real changed regions in output-physical pixels.
    damage: Option<&[smithay::utils::Rectangle<i32, smithay::utils::Physical>]>,
) {
    use smithay::backend::allocator::Fourcc;
    use smithay::backend::renderer::ExportMem;
    use smithay::utils::{Buffer as BufferCoord, Rectangle};

    let has_real_damage = damage.map(|d| !d.is_empty()).unwrap_or(false);

    let mut remaining = VecDeque::new();
    while let Some(pending) = screencopy.pending.pop_front() {
        if pending.output_name != output_name {
            remaining.push_back(pending);
            continue;
        }
        // `copy_with_damage` frames stay pending — not serviced, not
        // failed — until this output actually has damage. This is the
        // whole point of the request: a recorder holding one of these
        // gets woken up only on real changes instead of every pageflip.
        if pending.with_damage && !has_real_damage {
            remaining.push_back(pending);
            continue;
        }

        let region: Rectangle<i32, BufferCoord> =
            Rectangle::new((0, 0).into(), (output_size.0, output_size.1).into());

        let mapping = match renderer.copy_framebuffer(target, region, Fourcc::Argb8888) {
            Ok(m) => m,
            Err(e) => {
                warn!("screencopy: copy_framebuffer failed: {:?}", e);
                pending.frame.failed();
                continue;
            }
        };
        let pixels: &[u8] = match renderer.map_texture(&mapping) {
            Ok(p) => p,
            Err(e) => {
                warn!("screencopy: map_texture failed: {:?}", e);
                pending.frame.failed();
                continue;
            }
        };

        // `pixels` is a tightly-packed ARGB8888 buffer for the requested
        // region; copy it into the client's shm pool. `with_buffer_contents_mut`
        // hands us the pool's raw bytes plus its stride/format, which we
        // trust matches what we advertised in `send_buffer_offer` (grim
        // et al. allocate exactly what `buffer_done` describes).
        // `with_buffer_contents_mut`'s closure takes a raw `*mut u8` +
        // byte length + buffer metadata (mirroring the read-only
        // `with_buffer_contents(&buffer, |ptr: *const u8, len: usize,
        // buffer_metadata: BufferData| ...)` shape smithay documents) —
        // not a pre-built `&mut [u8]` slice. Build the slice ourselves
        // from the raw parts before copying into it.
        // NOTE: unlike the read-only variant, the bound here is
        // `FnOnce(*mut u8, usize, BufferData) -> T` — `BufferData` is
        // passed *by value*, not `&BufferData` (confirmed by the
        // compiler-reported bound in smithay's shm/mod.rs).
        let copy_ok = smithay::wayland::shm::with_buffer_contents_mut(
            &pending.buffer,
            |ptr: *mut u8, len: usize, _shm_info: smithay::wayland::shm::BufferData| {
                let shm_data = unsafe { std::slice::from_raw_parts_mut(ptr, len) };
                let copy_len = pixels.len().min(shm_data.len());
                shm_data[..copy_len].copy_from_slice(&pixels[..copy_len]);
            },
        )
        .is_ok();

        if copy_ok {
            pending.frame.flags(zwlr_screencopy_frame_v1::Flags::empty());
            if pending.with_damage {
                // Per protocol, `copy_with_damage` frames should describe
                // *which* regions changed via zero-or-more `damage`
                // events before `ready` — this is the actual payoff for
                // a recorder client (it can re-encode only the changed
                // rectangles instead of the whole frame). Plain `copy`
                // frames don't get these; they're a full-frame request
                // by definition, `damage` events would just be noise.
                if let Some(rects) = damage {
                    for r in rects {
                        pending.frame.damage(r.loc.x as u32, r.loc.y as u32, r.size.w as u32, r.size.h as u32);
                    }
                }
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            let secs = now.as_secs();
            pending.frame.ready((secs >> 32) as u32, secs as u32, now.subsec_nanos());
        } else {
            pending.frame.failed();
        }
    }
    screencopy.pending = remaining;
}
