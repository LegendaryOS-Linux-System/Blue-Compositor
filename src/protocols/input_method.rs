use smithay::{
    desktop::{Window},
    utils::{Logical, Point, Rectangle},
    wayland::input_method::PopupSurface as ImePopupSurface,
};
use tracing::debug;

use crate::state::BlueState;
use crate::ipc::CompositorMessage;

/// One tracked IME popup (candidate window). `BlueState::input_method_popups`
/// holds a `Vec` of these — in practice there's realistically at most one
/// live at a time (one active text input), but the protocol doesn't forbid
/// more than one IME client, so this doesn't assume a single-element list.
pub struct TrackedImePopup {
    pub popup: ImePopupSurface,
}

/// Find the on-screen (space-relative) geometry of the surface an IME
/// popup is anchored to. Mirrors `parent_geometry` in `state/mod.rs`'s
/// `InputMethodHandler` impl, but as a free function so `render/mod.rs`
/// can reuse it too without going through the trait.
pub fn parent_geometry(state: &BlueState, parent: Option<&smithay::reexports::wayland_server::protocol::wl_surface::WlSurface>) -> Rectangle<i32, Logical> {
    let Some(parent) = parent else { return Rectangle::default() };
    if let Some(window) = state.window_by_surface(parent) {
        return state.space.element_geometry(&window).unwrap_or_default();
    }
    // Not a toplevel window — check every output's layer-shell map (the
    // lock screen's password prompt, an on-screen keyboard, etc. — see
    // `state/mod.rs`'s `new_layer_surface`, which used to be a no-op so
    // this branch could never have found anything even in principle; now
    // that layer surfaces are actually mapped, this real lookup works).
    // `LayerMap` indexes by `LayerSurface` handle, not `WlSurface`
    // directly, so this has to search each output's mapped layers for
    // one whose own `wl_surface()` matches — there's no reverse index,
    // but the number of mapped layer surfaces on a desktop is always
    // small (panels + maybe a lock screen), so a linear scan is fine.
    use smithay::desktop::layer_map_for_output;
    for output in &state.outputs {
        let map = layer_map_for_output(output);
        for layer in map.layers() {
            if layer.wl_surface() == parent {
                if let Some(geo) = map.layer_geometry(layer) {
                    // `layer_geometry` is output-local; offset by the
                    // output's own position in the global logical space
                    // (matters on multi-monitor setups) to match what
                    // `window_by_surface`'s branch above returns.
                    let output_loc = output.current_location();
                    return Rectangle::new(output_loc + geo.loc, geo.size);
                }
            }
        }
    }
    // Genuinely unknown (surface not a window, not a mapped layer — e.g.
    // a subsurface or a surface mid-teardown): anchor at the origin
    // rather than panic/guess. The popup stays trackable/dismissable
    // correctly either way, just not positioned under the cursor.
    Rectangle::default()
}

/// Called from `InputMethodHandler::new_popup`.
pub fn popup_created(state: &mut BlueState, popup: ImePopupSurface) {
    let parent_geo = popup
        .get_parent()
        .map(|p| p.location)
        .unwrap_or_else(|| parent_geometry(state, popup.get_parent().map(|p| &p.surface)));
    let rect = popup.text_input_rectangle();
    let abs = Point::<i32, Logical>::from((
        parent_geo.loc.x + rect.loc.x,
        parent_geo.loc.y + rect.loc.y + rect.size.h,
    ));
    popup.set_location(abs - parent_geo.loc);

    debug!("IME candidate popup opened near ({}, {})", abs.x, abs.y);
    state.ipc_broadcast(CompositorMessage::ImeCandidateWindow {
        visible: true,
        x: abs.x,
        y: abs.y,
        width: rect.size.w.max(1) as u32,
        height: rect.size.h.max(1) as u32,
    });

    state.input_method_popups.push(TrackedImePopup { popup });
}

/// Called from `InputMethodHandler::dismiss_popup`.
pub fn popup_dismissed(state: &mut BlueState, popup: &ImePopupSurface) {
    state.input_method_popups.retain(|p| &p.popup != popup);
    debug!("IME candidate popup dismissed");
    state.ipc_broadcast(CompositorMessage::ImeCandidateWindow {
        visible: false, x: 0, y: 0, width: 0, height: 0,
    });
}

/// Called from `InputMethodHandler::popup_repositioned` — the IME updated
/// `set_text_input_rectangle` (e.g. the app scrolled, or the caret moved
/// to a new line) and wants its on-screen position recomputed.
pub fn popup_repositioned(state: &mut BlueState, popup: &ImePopupSurface) {
    let parent_geo = popup
        .get_parent()
        .map(|p| p.location)
        .unwrap_or_default();
    let rect = popup.text_input_rectangle();
    let abs = Point::<i32, Logical>::from((
        parent_geo.loc.x + rect.loc.x,
        parent_geo.loc.y + rect.loc.y + rect.size.h,
    ));
    popup.set_location(abs - parent_geo.loc);

    state.ipc_broadcast(CompositorMessage::ImeCandidateWindow {
        visible: true,
        x: abs.x,
        y: abs.y,
        width: rect.size.w.max(1) as u32,
        height: rect.size.h.max(1) as u32,
    });
}

/// Build render elements for every live IME popup, in absolute
/// (output-relative-once-offset-by-the-caller) space coordinates. Kept
/// generic over the render element type the same way the rest of
/// `render/mod.rs` already is (`SpaceRenderElements<GlesRenderer,
/// WaylandSurfaceRenderElement<GlesRenderer>>`), so call sites can collect
/// straight into the same `Vec` they already build from `space.
/// render_elements_for_output`.
///
/// Takes `&[TrackedImePopup]` rather than `&BlueState` deliberately: both
/// call sites in `render/mod.rs` already hold a live `&mut` borrow of
/// `state.backend_data` (for the bound renderer) at the point they need
/// this, and passing `state` as a whole would conflict with that —
/// borrowing just the one field they don't otherwise touch
/// (`state.input_method_popups`) keeps this a disjoint field borrow the
/// compiler can reason about, same as the pre-existing `state.space.
/// render_elements_for_output(...)` call right above each call site.
pub fn render_elements<R, E>(
    popups: &[TrackedImePopup],
    renderer: &mut R,
    scale: f64,
) -> Vec<E>
where
    R: smithay::backend::renderer::Renderer + smithay::backend::renderer::ImportAll,
    R::TextureId: Clone + 'static,
    E: From<smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement<R>>,
{
    let mut out = Vec::new();
    for tracked in popups {
        if !tracked.popup.alive() {
            continue;
        }
        let parent_geo = tracked
            .popup
            .get_parent()
            .map(|p| p.location)
            .unwrap_or_default();
        let loc = parent_geo.loc + tracked.popup.location();
        let phys = Point::<i32, smithay::utils::Physical>::from((
            (loc.x as f64 * scale).round() as i32,
            (loc.y as f64 * scale).round() as i32,
        ));
        let elems = smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
            renderer,
            tracked.popup.wl_surface(),
            phys,
            scale,
            1.0,
            smithay::backend::renderer::element::Kind::Unspecified,
        );
        out.extend(elems);
    }
    out
}

/// Referenced from `state/mod.rs` only for the `Window`-geometry helper
/// signature above; keeps the `desktop::Window` import from being unused
/// if `parent_geometry`'s window branch is ever simplified away.
#[allow(dead_code)]
fn _keep_window_import(_: &Window) {}
