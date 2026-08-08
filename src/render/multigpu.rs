use smithay::{
    backend::{
        allocator::{dmabuf::Dmabuf, Fourcc},
        drm::{DrmDeviceFd, DrmNode},
        renderer::{
            multigpu::{gbm::GbmGlesBackend, GpuManager, MultiTexture},
            gles::GlesRenderer,
            ImportDma,
        },
    },
    utils::{Buffer as BufferCoords, Rectangle},
};
use tracing::warn;

/// Concrete alias for the one `GraphicsApi` this compositor uses. Spelled
/// out once here so `UdevData`'s field type (state/mod.rs) and every
/// call site agree without repeating the generic parameters everywhere.
pub type Backend = GbmGlesBackend<GlesRenderer, DrmDeviceFd>;

/// Import `dmabuf` (allocated on `source`) so it can be composited into
/// an output driven by `target`. If `source == target` this still works
/// correctly (falls back to a same-device import with no copy — see
/// `GpuManager::single_renderer`'s doc comment upstream), so callers
/// don't need to special-case the common single-GPU case themselves.
///
/// `copy_format` is the pixel format used for the intermediate buffer
/// when a real cross-device copy is needed; `Argb8888` is the safe
/// universal choice (see the module's `SCANOUT_CANDIDATE_FORMATS` in
/// `protocols/dmabuf.rs` for the equivalent scanout-side reasoning) —
/// callers with a specific reason to preserve higher bit depth across
/// the copy (e.g. HDR content, see `protocols/color_management.rs`) can
/// pass a wider format instead.
pub fn import_from_other_gpu(
    gpu_manager: &mut GpuManager<Backend>,
    source: &DrmNode,
    target: &DrmNode,
    dmabuf: &Dmabuf,
    damage: Option<&[Rectangle<i32, BufferCoords>]>,
    copy_format: Fourcc,
) -> Option<MultiTexture> {
    let mut renderer = match gpu_manager.renderer(source, target, copy_format) {
        Ok(r) => r,
        Err(e) => {
            warn!("multi-gpu: failed to build cross-GPU renderer {source:?} -> {target:?}: {e:?}");
            return None;
        }
    };
    match renderer.import_dmabuf(dmabuf, damage) {
        Ok(tex) => Some(tex),
        Err(e) => {
            warn!("multi-gpu: cross-GPU dmabuf import {source:?} -> {target:?} failed: {e:?}");
            None
        }
    }
}

/// Per-surface subset of [`import_from_other_gpu`]'s prerequisite:
/// *which* node a surface's current buffer was allocated on, if any.
/// `BlueState::surface_gpu_origin` (state/mod.rs) is this map — call
/// [`track_surface_origin`] from `CompositorHandler::commit` (before
/// `on_commit_buffer_handler` consumes the pending buffer assignment —
/// see the call site's comment) to keep it current.
///
/// This is the detection half of "multi-GPU per-surface routing" — a
/// surface whose entry here differs from the output's own node is a
/// candidate for [`import_from_other_gpu`]. Nothing in the render loop
/// consults this map yet (see this module's doc comment for exactly what
/// the remaining wiring looks like and why it wasn't attempted blind);
/// this function's job ends at keeping the map correct.
pub fn track_surface_origin(
    surface_gpu_origin: &mut std::collections::HashMap<
        smithay::reexports::wayland_server::backend::ObjectId,
        DrmNode,
    >,
    surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
) {
    use smithay::wayland::compositor::{with_states, SurfaceAttributes, BufferAssignment};
    use smithay::reexports::wayland_server::Resource;

    let node = with_states(surface, |states| {
        let mut guard = states.cached_state.get::<SurfaceAttributes>();
        let attrs = guard.current();
        match &attrs.buffer {
            // Only a *new* buffer attachment tells us anything — a
            // commit that doesn't reattach (e.g. just an opaque-region
            // update) should leave whatever origin we already recorded
            // alone, not clear it, which is why this returns `None` (no
            // update) rather than an explicit "unknown" here.
            Some(BufferAssignment::NewBuffer(buffer)) => {
                smithay::wayland::dmabuf::get_dmabuf(buffer).ok().and_then(|d| d.node())
            }
            _ => None,
        }
    });

    if let Some(node) = node {
        surface_gpu_origin.insert(surface.id(), node);
    }
}
