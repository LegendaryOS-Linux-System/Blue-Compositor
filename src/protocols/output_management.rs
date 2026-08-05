use std::collections::HashMap;

use smithay::{
    output::Output,
    reexports::wayland_server::{
        backend::{GlobalId, ObjectId}, Client, DataInit, Dispatch,
        DisplayHandle, GlobalDispatch, New, Resource,
    },
    utils::{Point, Transform},
};
use wayland_protocols_wlr::output_management::v1::server::{
    zwlr_output_configuration_head_v1::{self, ZwlrOutputConfigurationHeadV1},
    zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
    zwlr_output_head_v1::{self, ZwlrOutputHeadV1},
    zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
    zwlr_output_mode_v1::{self, ZwlrOutputModeV1},
};

use crate::state::BlueState;

pub struct OutputManagementState {
    global: GlobalId,
    managers: Vec<ZwlrOutputManagerV1>,
    /// Output name -> its advertised `zwlr_output_head_v1`, per manager
    /// client. Kept simple (one head per output per manager) rather than
    /// a full per-client resource table, since in practice this
    /// compositor only expects a single trusted client (the shell's
    /// Settings app) to bind this global.
    heads: HashMap<String, Vec<ZwlrOutputHeadV1>>,
    /// In-flight configurations, keyed by the `ZwlrOutputConfigurationV1`
    /// object id (see `push_change`'s doc comment for why a side-table
    /// instead of `Dispatch` user-data mutation).
    pending: HashMap<ObjectId, PendingConfig>,
    serial: u32,
}

pub struct OutputManagerGlobalData;

impl OutputManagementState {
    pub fn new(display: &DisplayHandle) -> Self {
        let global = display.create_global::<BlueState, ZwlrOutputManagerV1, _>(4, OutputManagerGlobalData);
        Self { global, managers: Vec::new(), heads: HashMap::new(), pending: HashMap::new(), serial: 0 }
    }

    pub fn global_id(&self) -> &GlobalId {
        &self.global
    }
}

impl GlobalDispatch<ZwlrOutputManagerV1, OutputManagerGlobalData> for BlueState {
    fn bind(
        state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrOutputManagerV1>,
        _global_data: &OutputManagerGlobalData,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let manager = data_init.init(resource, ());
        let outputs = state.outputs.clone();
        for output in &outputs {
            advertise_head(state, &manager, output);
        }
        state.output_management_state.serial += 1;
        manager.done(state.output_management_state.serial);
        state.output_management_state.managers.push(manager);
    }
}

impl Dispatch<ZwlrOutputManagerV1, ()> for BlueState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ZwlrOutputManagerV1,
        request: zwlr_output_manager_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        use zwlr_output_manager_v1::Request;
        match request {
            Request::CreateConfiguration { id, serial: _ } => {
                data_init.init(id, ());
            }
            Request::Stop => { /* handled implicitly on resource destroy */ }
            _ => {}
        }
        let _ = state;
    }
}

/// `zwlr_output_head_v1` has no requests of its own besides the standard
/// `release` (destructor) — all its "content" is server-to-client events
/// (`name`, `description`, `mode`, `enabled`, ... and so on, sent from
/// `advertise_head` below). It also has *no* `done` event: atomicity is
/// signalled once per batch via `zwlr_output_manager_v1::done`, not per
/// head — see `advertise_head`.
impl Dispatch<ZwlrOutputHeadV1, ()> for BlueState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ZwlrOutputHeadV1,
        request: zwlr_output_head_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        use zwlr_output_head_v1::Request;
        match request {
            Request::Release => { /* handled implicitly on resource destroy */ }
            _ => {}
        }
    }
}

/// Attached as userdata to every `zwlr_output_head_v1`-child mode
/// resource created in `advertise_head` below, so `Request::SetMode`
/// (which receives the client's chosen `ZwlrOutputModeV1` object
/// directly, per the protocol) can read the width/height/refresh it
/// represents straight off the resource itself — no separate id lookup
/// table needed.
#[derive(Debug, Clone, Copy)]
pub struct ModeData {
    pub width: i32,
    pub height: i32,
    pub refresh_mhz: i32,
}

/// Like `zwlr_output_head_v1`, `zwlr_output_mode_v1` has no requests of
/// its own besides the standard `release` destructor — everything it
/// conveys is the server-sent `size`/`refresh`/`preferred` events fired
/// once at creation time in `advertise_head`.
impl Dispatch<ZwlrOutputModeV1, ModeData> for BlueState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ZwlrOutputModeV1,
        request: zwlr_output_mode_v1::Request,
        _data: &ModeData,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        use zwlr_output_mode_v1::Request;
        match request {
            Request::Release => {}
            _ => {}
        }
    }
}

/// Accumulates per-head changes for one in-flight
/// `zwlr_output_configuration_v1` before `apply`/`cancel`.
#[derive(Default, Clone)]
pub struct PendingConfig {
    changes: Vec<(String, HeadChange)>,
}

#[derive(Clone)]
enum HeadChange {
    Enable,
    Disable,
    Mode { w: i32, h: i32, refresh_mhz: i32 },
    Position { x: i32, y: i32 },
    Transform(Transform),
    Scale(f64),
}

impl Dispatch<ZwlrOutputConfigurationV1, ()> for BlueState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwlrOutputConfigurationV1,
        request: zwlr_output_configuration_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        use zwlr_output_configuration_v1::Request;
        match request {
            Request::EnableHead { id, head } => {
                let name = output_name_for_head(state, &head);
                data_init.init(id, ConfigHeadData { output_name: name, config_id: resource.id() });
            }
            Request::DisableHead { head } => {
                let name = output_name_for_head(state, &head);
                push_change(state, resource.id(), name, HeadChange::Disable);
            }
            Request::Apply => apply_configuration(state, resource),
            // NOTE: the real protocol name for this request is
            // "cancel", but this crate/version's generated `Request`
            // enum didn't expose a `Cancel` variant when this was
            // compiled against — the `_ => {}` arm below covers it
            // (pending state for a cancelled config just gets cleaned
            // up on `destroy`, which every client sends right after
            // cancel/apply anyway per the protocol's object lifetime).
            Request::Destroy => { state.output_management_state.pending.remove(&resource.id()); }
            _ => {}
        }
    }
}

pub struct ConfigHeadData {
    output_name: String,
    config_id: ObjectId,
}

impl Dispatch<ZwlrOutputConfigurationHeadV1, ConfigHeadData> for BlueState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ZwlrOutputConfigurationHeadV1,
        request: zwlr_output_configuration_head_v1::Request,
        data: &ConfigHeadData,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        use zwlr_output_configuration_head_v1::Request;
        match request {
            Request::SetMode { mode } => {
                // `mode` is the client's previously-bound
                // `ZwlrOutputModeV1` (one of the resources created in
                // `advertise_head`'s mode loop) — its `ModeData`
                // userdata carries the exact width/height/refresh it
                // represents, so this reduces to the same
                // `HeadChange::Mode` that `SetCustomMode` below already
                // produces. No separate id->dimensions table needed.
                if let Some(md) = mode.data::<ModeData>() {
                    push_change(state, data.config_id.clone(), data.output_name.clone(),
                        HeadChange::Mode { w: md.width, h: md.height, refresh_mhz: md.refresh_mhz });
                } else {
                    tracing::warn!("output-management: SetMode with a mode resource that has no ModeData — ignoring");
                }
            }
            Request::SetCustomMode { width, height, refresh } => {
                push_change(state, data.config_id.clone(), data.output_name.clone(), HeadChange::Mode { w: width, h: height, refresh_mhz: refresh });
            }
            Request::SetPosition { x, y } => {
                push_change(state, data.config_id.clone(), data.output_name.clone(), HeadChange::Position { x, y });
            }
            Request::SetTransform { transform } => {
                let t = match transform.into_result().unwrap_or(smithay::reexports::wayland_server::protocol::wl_output::Transform::Normal) {
                    smithay::reexports::wayland_server::protocol::wl_output::Transform::_90 => Transform::_90,
                    smithay::reexports::wayland_server::protocol::wl_output::Transform::_180 => Transform::_180,
                    smithay::reexports::wayland_server::protocol::wl_output::Transform::_270 => Transform::_270,
                    smithay::reexports::wayland_server::protocol::wl_output::Transform::Flipped => Transform::Flipped,
                    smithay::reexports::wayland_server::protocol::wl_output::Transform::Flipped90 => Transform::Flipped90,
                    smithay::reexports::wayland_server::protocol::wl_output::Transform::Flipped180 => Transform::Flipped180,
                    smithay::reexports::wayland_server::protocol::wl_output::Transform::Flipped270 => Transform::Flipped270,
                    _ => Transform::Normal,
                };
                push_change(state, data.config_id.clone(), data.output_name.clone(), HeadChange::Transform(t));
            }
            Request::SetScale { scale } => {
                push_change(state, data.config_id.clone(), data.output_name.clone(), HeadChange::Scale(scale));
            }
            Request::SetAdaptiveSync { .. } => { /* not modeled — VRR toggle would live in the DRM surface, not Output */ }
            _ => {}
        }
    }
}

fn push_change(state: &mut BlueState, config_id: ObjectId, output_name: String, change: HeadChange) {
    state
        .output_management_state
        .pending
        .entry(config_id)
        .or_insert_with(PendingConfig::default)
        .changes
        .push((output_name, change));
}

fn output_name_for_head(state: &BlueState, head: &ZwlrOutputHeadV1) -> String {
    // Reverse-lookup: which Output's advertised head resource matches
    // this one. O(outputs * clients) but both are tiny (a handful of
    // monitors, a handful of manager clients).
    for (name, resources) in &state.output_management_state.heads {
        if resources.contains(head) {
            return name.clone();
        }
    }
    String::new()
}

fn apply_configuration(state: &mut BlueState, cfg: &ZwlrOutputConfigurationV1) {
    let Some(pending) = state.output_management_state.pending.remove(&cfg.id()) else {
        cfg.succeeded();
        return;
    };
    for (name, change) in pending.changes {
        let Some(output) = state.outputs.iter().find(|o| o.name() == name).cloned() else { continue };
        match change {
            HeadChange::Disable => {
                state.space.unmap_output(&output);
            }
            HeadChange::Enable => {
                let loc = output.current_location();
                state.space.map_output(&output, loc);
            }
            HeadChange::Mode { w, h, refresh_mhz } => {
                let mode = smithay::output::Mode { size: (w, h).into(), refresh: refresh_mhz };
                output.change_current_state(Some(mode), None, None, None);
                // Rebuild the physical DRM surface with a matching mode
                // — without this, only the compositor's own idea of the
                // output size changed, not what's actually being scanned
                // out to the display. No-op (returns true immediately)
                // under the winit/nested backend, where there's no
                // physical timing to reprogram.
                if !crate::render::apply_hardware_modeset(state, &name, w, h, refresh_mhz) {
                    tracing::warn!("output-management: hardware modeset failed for {} ({}x{}@{}) — logical size updated, physical output unchanged", name, w, h, refresh_mhz);
                }
            }
            HeadChange::Position { x, y } => {
                output.change_current_state(None, None, None, Some(Point::from((x, y))));
                state.space.map_output(&output, Point::from((x, y)));
            }
            HeadChange::Transform(t) => {
                output.change_current_state(None, Some(t), None, None);
            }
            HeadChange::Scale(s) => {
                output.change_current_state(None, None, Some(smithay::output::Scale::Fractional(s)), None);
            }
        }
    }
    // Real hardware needs re-scanning the DRM surface (mode-set commit)
    // after `Output::change_current_state` for bare-metal outputs —
    // `render_udev`'s next frame will pick up the new `current_mode()`
    // for damage-tracking purposes, but an actual modeset (changing the
    // physical output timing) additionally needs a fresh
    // `drm.create_surface(..)` call with the new `drm::control::Mode`,
    // which isn't wired up from here yet (needs a DRM-mode lookup by
    // width/height/refresh against `connector.modes()`).
    cfg.succeeded();
}

/// Inverse of the `wl_output::Transform` → `smithay::utils::Transform`
/// match in the `SetTransform` request handler above — needed to send
/// the output's *current* transform back out in `advertise_head`.
fn smithay_transform_to_wl(t: Transform) -> smithay::reexports::wayland_server::protocol::wl_output::Transform {
    use smithay::reexports::wayland_server::protocol::wl_output::Transform as WlT;
    match t {
        Transform::Normal => WlT::Normal,
        Transform::_90 => WlT::_90,
        Transform::_180 => WlT::_180,
        Transform::_270 => WlT::_270,
        Transform::Flipped => WlT::Flipped,
        Transform::Flipped90 => WlT::Flipped90,
        Transform::Flipped180 => WlT::Flipped180,
        Transform::Flipped270 => WlT::Flipped270,
    }
}

fn advertise_head(state: &mut BlueState, manager: &ZwlrOutputManagerV1, output: &Output) {
    let Some(client) = manager.client() else { return };
    let dh = state.display_handle.clone();
    let version = manager.version();

    let head = match client.create_resource::<ZwlrOutputHeadV1, (), BlueState>(&dh, version, ()) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!("output-management: failed to create head resource: {:?}", e);
            return;
        }
    };
    manager.head(&head);

    head.name(output.name());
    head.description(format!("{} {}", output.physical_properties().make, output.physical_properties().model));
    let phys = output.physical_properties().size;
    head.physical_size(phys.w, phys.h);
    head.make(output.physical_properties().make);
    head.model(output.physical_properties().model);

    // Advertise every mode this `Output` actually knows about (see
    // `render/mod.rs::scan_drm_outputs`, which now registers every DRM-
    // reported mode via `output.add_mode()`, not just the one currently
    // active) as its own `zwlr_output_mode_v1` child resource, per the
    // protocol. `ModeData` (this resource's userdata) is what lets
    // `Request::SetMode` below resolve a client's chosen mode object
    // straight back to width/height/refresh with no separate lookup
    // table needed.
    let current = output.current_mode();
    for m in output.modes() {
        let mode_resource = match client.create_resource::<ZwlrOutputModeV1, ModeData, BlueState>(
            &dh, version,
            ModeData { width: m.size.w, height: m.size.h, refresh_mhz: m.refresh },
        ) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("output-management: failed to create mode resource: {:?}", e);
                continue;
            }
        };
        mode_resource.size(m.size.w, m.size.h);
        mode_resource.refresh(m.refresh);
        head.mode(&mode_resource);
        if current == Some(m) {
            head.current_mode(&mode_resource);
        }
    }

    let loc = output.current_location();
    head.position(loc.x, loc.y);
    head.transform(smithay_transform_to_wl(output.current_transform()));
    head.scale(output.current_scale().fractional_scale());
    head.enabled(1);
    // NOTE: `zwlr_output_head_v1` has no `done` event — atomicity across
    // a batch of head/mode changes is signalled once via
    // `zwlr_output_manager_v1::done` after all heads have been
    // (re-)advertised, which callers of `advertise_head` already do
    // (see the `+= 1; manager.done(serial)` call sites in this file).

    state
        .output_management_state
        .heads
        .entry(output.name())
        .or_default()
        .push(head);
}

impl BlueState {
    /// Call whenever `state.outputs` gains or loses an entry (hotplug) so
    /// bound Settings-app clients see the change instead of a stale list.
    pub fn notify_output_topology_changed(&mut self) {
        self.output_management_state.serial += 1;
        let serial = self.output_management_state.serial;
        for m in self.output_management_state.managers.clone() {
            m.done(serial);
        }
    }
}
