pub mod idle;
pub mod idle_inhibit;
pub mod hotplug;
pub mod decoration;
pub mod cursor_shape;
pub mod session_lock;
pub mod foreign_toplevel;
pub mod output_management;
pub mod screencopy;
/// `zwp_linux_dmabuf_v1` — see dmabuf.rs module doc for why this global
/// didn't exist at all before this pass, and its relation to the
/// separate, still-blocked scanout-side modifier limitation documented
/// in `render/mod.rs`.
pub mod dmabuf;
/// IME candidate-window popup positioning/rendering — the actual
/// remaining gap in "ibus/fcitx5 support", since the wire-protocol
/// managers were already registered. See module doc.
pub mod input_method;
/// `wp_color_management_v1` (HDR / wide-gamut negotiation) — hand-vendored
/// protocol, parametric path only. See module doc for scope + the
/// verification caveat (never compiled with an MSRV-matching toolchain).
pub mod color_management;

use smithay::{
    delegate_xdg_activation,
    wayland::{
        seat::WaylandFocus,
        xdg_activation::{
            XdgActivationHandler, XdgActivationState,
            XdgActivationToken, XdgActivationTokenData,
        },
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
};
use crate::state::BlueState;

pub fn init_xdg_activation(_state: &mut BlueState) {
    // XdgActivationState is created inside BlueState::new() via
    // XdgActivationState::new::<BlueState>(&display_handle).
    // Nothing to do here — the field is already initialised.
}

impl XdgActivationHandler for BlueState {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.xdg_activation_state
    }

    fn token_created(&mut self, _token: XdgActivationToken, _data: XdgActivationTokenData) -> bool {
        true
    }

    fn request_activation(
        &mut self,
        _token: XdgActivationToken,
        _token_data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        if let Some(window) = self.window_by_surface(&surface) {
            self.space.raise_element(&window, true);
            if let Some(surf) = window.wl_surface() {
                let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                if let Some(kb) = self.seat.get_keyboard() {
                    kb.set_focus(self, Some(surf.into_owned()), serial);
                }
            }
            tracing::info!("XDG activation: raised window");
        }
    }
}

delegate_xdg_activation!(BlueState);
