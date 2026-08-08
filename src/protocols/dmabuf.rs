use smithay::{
    backend::{
        allocator::Format,
        renderer::{ImportDma, gles::GlesRenderer},
    },
    delegate_dmabuf,
    reexports::wayland_server::DisplayHandle,
    wayland::{
        dmabuf::{
            DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier,
        },
    },
};
use tracing::{info, warn};

use crate::state::BlueState;

/// The same conservative, scanout-oriented format priority list already
/// used for the GBM swapchain in `render_udev` (kept in sync manually —
/// see the comment there). Duplicated here rather than shared because the
/// render/mod.rs list is `DrmFourcc`-only (no modifiers) for the blocked
/// API, while this one pairs each format with the LINEAR modifier for the
/// scanout-preference *tranche*, which is a distinct type
/// (`allocator::Format`, not `DrmFourcc`).
fn scanout_candidate_formats() -> Vec<Format> {
    use smithay::backend::allocator::{Fourcc, Modifier};
    vec![
        Format { code: Fourcc::Xrgb2101010, modifier: Modifier::Linear },
        Format { code: Fourcc::Argb2101010, modifier: Modifier::Linear },
        Format { code: Fourcc::Xrgb8888, modifier: Modifier::Linear },
        Format { code: Fourcc::Argb8888, modifier: Modifier::Linear },
    ]
}

/// Build the `zwp_linux_dmabuf_v1` global with real per-driver format
/// feedback. Called once at startup (both winit and udev backends can use
/// it — the winit path is what makes this testable without real DRM
/// hardware, e.g. in the headless smoke test in `build.rb check`).
///
/// `main_device` should be the primary GPU's DRM node dev_t when running
/// under udev, or omitted (falls back to a synthetic 0) under winit, where
/// there's no real backing device and the feedback's `main_device` field
/// is informational only for nested/testing use.
///
/// Deliberately returns the built `(DmabufState, DmabufGlobal)` instead of
/// taking `&mut BlueState` and assigning into it directly: both call
/// sites (`render/mod.rs`, winit and udev paths) reach this from inside a
/// scope that already holds a live borrow of `state.backend_data` (to get
/// at the renderer) — taking `&mut BlueState` here too would conflict
/// with that. Returning the pieces lets each caller assign them into
/// `state.dmabuf_state`/`state.dmabuf_global` itself, after that borrow
/// has ended.
pub fn init_dmabuf(
    display_handle: &DisplayHandle,
    renderer: &GlesRenderer,
    main_device: Option<libc::dev_t>,
) -> (DmabufState, Option<DmabufGlobal>) {
    let mut dmabuf_state = DmabufState::new();

    let render_formats: Vec<Format> = renderer.dmabuf_formats().into_iter().collect();
    if render_formats.is_empty() {
        warn!("dmabuf: renderer reported zero importable formats — client GPU buffer import will not work, clients will fall back to wl_shm");
    } else {
        info!("dmabuf: {} (format, modifier) pairs supported for import", render_formats.len());
    }

    let device = main_device.unwrap_or(0);
    let mut builder = DmabufFeedbackBuilder::new(device, render_formats.clone());

    // Scanout-preference tranche: only the modifiers/formats we'd actually
    // try to hand to KMS. Filtered against what the renderer can also
    // import, since a format the renderer can't touch at all shouldn't be
    // advertised as preferred no matter how well the display device
    // supports it (see the module doc's "fallback path" note).
    let scanout_pref: Vec<Format> = scanout_candidate_formats()
        .into_iter()
        .filter(|f| render_formats.contains(f))
        .collect();
    if !scanout_pref.is_empty() {
        builder = builder.add_preference_tranche(
            device,
            Some(smithay::reexports::wayland_protocols::wp::linux_dmabuf::zv1::server::zwp_linux_dmabuf_feedback_v1::TrancheFlags::Scanout),
            scanout_pref,
        );
    }

    match builder.build() {
        Ok(default_feedback) => {
            let global = dmabuf_state
                .create_global_with_default_feedback::<BlueState>(display_handle, &default_feedback);
            info!("zwp_linux_dmabuf_v1 global registered (with per-format feedback, v4)");
            (dmabuf_state, Some(global))
        }
        Err(e) => {
            warn!("dmabuf: failed to build default feedback ({e:?}) — falling back to the unversioned v3 global (no per-format feedback, but still real GPU import)");
            let global = dmabuf_state.create_global::<BlueState>(display_handle, render_formats);
            (dmabuf_state, Some(global))
        }
    }
}

// `BufferHandler` for `BlueState` is implemented once, in state/mod.rs
// (buffer-type-agnostic — the trait has one method, `buffer_destroyed`,
// shared across shm/dmabuf/every other buffer kind, so it can't be
// implemented per-module here too; an earlier version of this file did,
// which is a real `E0119` conflicting-impl compile error, not just a
// style nit — caught by a real `cargo build`).
impl DmabufHandler for BlueState {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        self.dmabuf_state.as_mut().expect("dmabuf_state initialised in init_dmabuf() before any client can bind the global")
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: smithay::backend::allocator::dmabuf::Dmabuf,
        notifier: ImportNotifier,
    ) {
        // Actual GPU-side import (turning the fds into a bindable texture)
        // happens lazily on first use inside smithay's renderer surface
        // state, same as with any other WlBuffer — this handler's job is
        // only to accept/reject the *allocation itself* (format/modifier
        // combination + plane layout sanity, which DmabufState already
        // checked before calling us). We don't have a cheap way to
        // validate importability without actually attempting a GL import,
        // so — consistent with the "always keep the composite fallback
        // valid" note in the module doc — we accept optimistically here
        // and let a real import failure surface later as a normal render
        // error for that surface, rather than blocking every client on a
        // synchronous test-import per buffer.
        let _ = &dmabuf;
        notifier.successful::<BlueState>();
    }
}

delegate_dmabuf!(BlueState);
