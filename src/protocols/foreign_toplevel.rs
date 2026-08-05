use std::collections::HashMap;

use smithay::reexports::wayland_server::{
    backend::GlobalId,
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use wayland_protocols_wlr::foreign_toplevel::v1::server::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};

use crate::state::BlueState;

pub struct ForeignToplevelManagerState {
    global: GlobalId,
    /// Every client's bound manager object — new toplevel handles get
    /// created against each of these when a window is mapped.
    managers: Vec<ZwlrForeignToplevelManagerV1>,
    /// window_meta id -> (handle resource, owning manager) for every
    /// currently-advertised toplevel, across all clients.
    handles: HashMap<u64, Vec<ZwlrForeignToplevelHandleV1>>,
}

/// Per-handle bookkeeping so `Dispatch` can map a
/// `ZwlrForeignToplevelHandleV1` request (e.g. "activate") back to our
/// internal window id.
#[derive(Clone, Copy)]
pub struct ToplevelHandleData {
    pub window_id: u64,
}

pub struct ForeignToplevelManagerGlobalData;

impl ForeignToplevelManagerState {
    pub fn new(display: &DisplayHandle) -> Self {
        let global = display.create_global::<BlueState, ZwlrForeignToplevelManagerV1, _>(
            3, // protocol version
            ForeignToplevelManagerGlobalData,
        );
        Self { global, managers: Vec::new(), handles: HashMap::new() }
    }

    pub fn global_id(&self) -> &GlobalId {
        &self.global
    }
}

impl GlobalDispatch<ZwlrForeignToplevelManagerV1, ForeignToplevelManagerGlobalData> for BlueState {
    fn bind(
        state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrForeignToplevelManagerV1>,
        _global_data: &ForeignToplevelManagerGlobalData,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let manager = data_init.init(resource, ());
        // Immediately advertise every currently-mapped window to the
        // newly-bound client, mirroring what a fresh `wl_registry` bind
        // should do for any "list of existing things" style global.
        for (id, meta) in state.window_meta.clone() {
            emit_new_toplevel(state, &manager, id, &meta);
        }
        state.foreign_toplevel_state.managers.push(manager);
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for BlueState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwlrForeignToplevelManagerV1,
        request: zwlr_foreign_toplevel_manager_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        if let zwlr_foreign_toplevel_manager_v1::Request::Stop = request {
            resource.finished();
            state.foreign_toplevel_state.managers.retain(|m| m != resource);
        }
    }
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ToplevelHandleData> for BlueState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwlrForeignToplevelHandleV1,
        request: zwlr_foreign_toplevel_handle_v1::Request,
        data: &ToplevelHandleData,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        use zwlr_foreign_toplevel_handle_v1::Request;
        let Some(window) = state
            .space
            .elements()
            .find(|w| crate::state::BlueState::window_id(w) == data.window_id)
            .cloned()
        else {
            return;
        };
        match request {
            Request::SetMaximized => { /* TODO: window.set_maximized via xdg/x11-specific path */ }
            Request::UnsetMaximized => {}
            Request::SetMinimized => {}
            Request::UnsetMinimized => {}
            Request::Activate { seat: _ } => {
                state.space.raise_element(&window, true);
                if let Some(kb) = state.seat.get_keyboard() {
                    if let Some(surf) = smithay::wayland::seat::WaylandFocus::wl_surface(&window) {
                        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                        kb.set_focus(state, Some(surf.into_owned()), serial);
                    }
                }
            }
            Request::Close => {
                if let Some(toplevel) = window.toplevel() {
                    toplevel.send_close();
                } else if let Some(x11) = window.x11_surface() {
                    let _ = x11.close();
                }
            }
            Request::SetRectangle { .. } => { /* minimize-to-taskbar-icon animation target, cosmetic only */ }
            Request::Destroy => {
                if let Some(v) = state.foreign_toplevel_state.handles.get_mut(&data.window_id) {
                    v.retain(|h| h != resource);
                }
            }
            _ => {}
        }
    }
}

fn emit_new_toplevel(
    state: &mut BlueState,
    manager: &ZwlrForeignToplevelManagerV1,
    window_id: u64,
    meta: &crate::state::WindowMeta,
) {
    // Creating a protocol object for an *existing* bound client, outside
    // of that client's own `request`/`bind` call, needs `Client::
    // create_resource::<Interface, UserData, D>(&DisplayHandle, version,
    // user_data)`. `Resource::client()` gets the owning `Client` handle
    // from the manager resource itself. This is the part of the file
    // that couldn't be verified without a toolchain in earlier sessions
    // — now filled in with smithay/wayland-server 0.31's documented
    // shape for this operation; still worth double-checking against
    // `cargo check` output specifically for this function first if new
    // errors show up here, since it's the least-exercised API surface in
    // this whole file.
    let Some(client) = manager.client() else { return };
    let dh = state.display_handle.clone();
    let version = manager.version();

    match client.create_resource::<ZwlrForeignToplevelHandleV1, ToplevelHandleData, BlueState>(
        &dh,
        version,
        ToplevelHandleData { window_id },
    ) {
        Ok(handle) => {
            manager.toplevel(&handle);
            handle.title(meta.title.clone());
            handle.app_id(meta.app_id.clone());
            if meta.workspace > 0 {
                // No dedicated "workspace" event in this protocol version
                // — state (minimized/maximized/activated/fullscreen) is
                // the closest fit, and workspace membership itself isn't
                // modeled by wlr-foreign-toplevel-management at all
                // (that's `ext-workspace-v1`'s job, not implemented
                // here). Left as a no-op rather than misusing `state()`.
            }
            handle.done();
            state
                .foreign_toplevel_state
                .handles
                .entry(window_id)
                .or_default()
                .push(handle);
        }
        Err(e) => {
            tracing::warn!("foreign-toplevel: failed to create handle resource: {:?}", e);
        }
    }
}

impl BlueState {
    /// Call when a window is mapped for the first time. Not yet wired
    /// into the xdg-shell/XWayland mapping call sites — see the
    /// TODO(foreign-toplevel) markers in `state/mod.rs`/`xwayland/mod.rs`.
    pub fn notify_toplevel_mapped(&mut self, window_id: u64) {
        let Some(meta) = self.window_meta.get(&window_id).cloned() else { return };
        let managers = self.foreign_toplevel_state.managers.clone();
        for m in &managers {
            emit_new_toplevel(self, m, window_id, &meta);
        }
    }

    /// Call when a window is unmapped/destroyed.
    pub fn notify_toplevel_unmapped(&mut self, window_id: u64) {
        if let Some(handles) = self.foreign_toplevel_state.handles.remove(&window_id) {
            for h in handles {
                h.closed();
            }
        }
    }

    /// Call whenever the tracked title/app_id for a window changes.
    pub fn notify_toplevel_title_appid(&mut self, window_id: u64, title: &str, app_id: &str) {
        if let Some(handles) = self.foreign_toplevel_state.handles.get(&window_id) {
            for h in handles {
                h.title(title.to_string());
                h.app_id(app_id.to_string());
                h.done();
            }
        }
    }
}
