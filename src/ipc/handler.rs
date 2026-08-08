use smithay::wayland::seat::WaylandFocus;
use tracing::info;

use crate::state::BlueState;
use super::messages::ShellMessage;
use super::socket::broadcast_workspace_switch;

pub fn handle_shell_message(state: &mut BlueState, msg: ShellMessage) {
    match msg {
        ShellMessage::FocusWindow { id } => {
            if let Some(window) = state.window_by_id(id) {
                state.space.raise_element(&window, true);
                if let Some(surface) = window.wl_surface() {
                    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                    if let Some(kb) = state.seat.get_keyboard() {
                        kb.set_focus(state, Some(surface.into_owned()), serial);
                    }
                }
                if let Some(meta) = state.window_meta.get_mut(&id) {
                    meta.is_minimized = false;
                }
            }
        }

        ShellMessage::CloseWindow { id } => {
            if let Some(window) = state.window_by_id(id) {
                if let Some(t) = window.toplevel() { t.send_close(); }
            }
        }

        ShellMessage::KillWindow { id } => {
            if let Some(window) = state.window_by_id(id) {
                if let Some(t) = window.toplevel() { t.send_close(); }
                info!("KillWindow id={} (graceful close sent)", id);
            }
        }

        ShellMessage::SwitchWorkspace { index } => {
            let old = state.current_workspace;
            state.switch_workspace(index);
            let count = state.workspace_count;
            let clients = state.clients.clone();
            broadcast_workspace_switch(&clients, index, count);
            info!("Workspace {} -> {}", old, index);
        }

        ShellMessage::MoveWindowToWorkspace { id, workspace } => {
            if let Some(meta) = state.window_meta.get_mut(&id) {
                meta.workspace = workspace;
            }
        }

        ShellMessage::ToggleMaximize { id } => {
            if let Some(window) = state.window_by_id(id) {
                if let Some(t) = window.toplevel() {
                    t.with_pending_state(|s| {
                        use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as XS;
                        if s.states.contains(XS::Maximized) { s.states.unset(XS::Maximized); }
                        else { s.states.set(XS::Maximized); }
                    });
                    t.send_configure();
                }
            }
        }

        ShellMessage::MinimizeWindow { id } => {
            if let Some(meta) = state.window_meta.get_mut(&id) { meta.is_minimized = true; }
        }

        ShellMessage::RestoreWindow { id } => {
            if let Some(meta) = state.window_meta.get_mut(&id) { meta.is_minimized = false; }
            if let Some(window) = state.window_by_id(id) {
                state.space.raise_element(&window, true);
                if let Some(surface) = window.wl_surface() {
                    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                    if let Some(kb) = state.seat.get_keyboard() {
                        kb.set_focus(state, Some(surface.into_owned()), serial);
                    }
                }
            }
        }

        ShellMessage::SetFullscreen { id, fullscreen } => {
            if let Some(window) = state.window_by_id(id) {
                if let Some(t) = window.toplevel() {
                    t.with_pending_state(|s| {
                        use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as XS;
                        if fullscreen { s.states.set(XS::Fullscreen); } else { s.states.unset(XS::Fullscreen); }
                    });
                    t.send_configure();
                }
                if let Some(meta) = state.window_meta.get_mut(&id) { meta.is_fullscreen = fullscreen; }
            }
        }

        ShellMessage::TileWindow { id, position } => {
            if let Some(output) = state.outputs.first() {
                let geo = state.space.output_geometry(output).unwrap_or_default();
                let (loc, size) = match position.as_str() {
                    "left"  => ((geo.loc.x, geo.loc.y + 30), (geo.size.w / 2, geo.size.h - 30)),
                    "right" => ((geo.loc.x + geo.size.w/2, geo.loc.y + 30), (geo.size.w / 2, geo.size.h - 30)),
                    "full"  => ((geo.loc.x, geo.loc.y + 30), (geo.size.w, geo.size.h - 30)),
                    _       => return,
                };
                if let Some(window) = state.window_by_id(id) {
                    state.space.map_element(window.clone(), smithay::utils::Point::from(loc), true);
                    if let Some(t) = window.toplevel() {
                        t.with_pending_state(|s| {
                            s.size = Some(smithay::utils::Size::from(size));
                        });
                        t.send_configure();
                    }
                }
            }
        }

        ShellMessage::SetWorkspaceCount { count } => {
            let count = count.max(1).min(10);
            state.workspace_count = count;
            if state.current_workspace >= count { state.switch_workspace(count - 1); }
            let clients = state.clients.clone();
            broadcast_workspace_switch(&clients, state.current_workspace, count);
        }

        ShellMessage::SetDpmsTimeout { seconds } => {
            state.dpms_timeout = std::time::Duration::from_secs(seconds);
        }

        ShellMessage::LockScreen => {
            // The compositor now natively implements ext-session-lock-v1
            // (see protocols/session_lock.rs) — a real lock client just
            // needs to speak that protocol and it will get a compositor-
            // enforced lock (all outputs blanked, confirmed only once every
            // output has a lock surface). `blue-lock` is that client, once
            // it ships; until then we fall back to whatever third-party
            // lock binary is installed, same as before.
            std::process::Command::new("sh")
                .arg("-c").arg("blue-lock 2>/dev/null || swaylock 2>/dev/null || gtklock 2>/dev/null || i3lock 2>/dev/null &")
                .spawn().ok();
        }

        ShellMessage::TakeScreenshot { path, mode } => {
            let clients = state.clients.clone();
            // Previously "focused" mode shelled out to `swaymsg -t
            // get_tree | jq ...` — that's sway's own IPC protocol, which
            // Blue Compositor does not (and will never) implement, so
            // this branch was guaranteed to fail on every single
            // invocation, silently falling through to the `grim`
            // (full-output) fallback below it. Compute the focused
            // window's geometry natively from `state.space` instead —
            // this compositor already tracks focus via `state.seat`, so
            // there's no reason to shell out to another compositor's
            // IPC for information we already have.
            let focused_geo: Option<smithay::utils::Rectangle<i32, smithay::utils::Logical>> =
                if mode == "focused" {
                    state
                        .seat
                        .get_keyboard()
                        .and_then(|kb| kb.current_focus())
                        .and_then(|surface| state.window_by_surface(&surface))
                        .and_then(|window| state.space.element_geometry(&window))
                } else {
                    None
                };
            std::thread::spawn(move || {
                let cmd = match focused_geo {
                    Some(geo) => format!(
                        "grim -g '{},{} {}x{}' {} 2>/dev/null",
                        geo.loc.x, geo.loc.y, geo.size.w, geo.size.h, path
                    ),
                    // Either full-output mode was requested, or
                    // "focused" was requested but nothing currently has
                    // keyboard focus (e.g. empty desktop) — fall back to
                    // capturing everything, same as before.
                    None => format!("grim {} 2>/dev/null || import -window root {} 2>/dev/null", path, path),
                };
                let ok = std::process::Command::new("sh").arg("-c").arg(&cmd).status()
                    .map(|s| s.success()).unwrap_or(false);
                use super::socket::broadcast;
                use super::messages::CompositorMessage;
                if ok {
                    broadcast(&clients, &CompositorMessage::ScreenshotReady { path });
                } else {
                    broadcast(&clients, &CompositorMessage::Error { message: "Screenshot failed".into() });
                }
            });
        }

        ShellMessage::SetKeyboardLayout { layout, variant } => {
            let mut xkb = smithay::input::keyboard::XkbConfig::default();
            xkb.layout  = Box::leak(layout.into_boxed_str());
            if let Some(v) = variant { xkb.variant = Box::leak(v.into_boxed_str()); }
            let _ = state.seat.add_keyboard(xkb, 400, 30);
        }

        ShellMessage::SetCursor { theme, size } => {
            std::env::set_var("XCURSOR_THEME", &theme);
            std::env::set_var("XCURSOR_SIZE", size.to_string());
        }

        ShellMessage::ReloadConfig => { info!("Config reload requested"); }
        ShellMessage::GetWindowList => { /* emitted on next poll tick */ }
        ShellMessage::SetHdrEnabled { output, enabled } => {
            // Negotiation-side plumbing exists (protocols/color_management.rs);
            // actually switching the output's active mode/plane to a
            // 10-bit HDR-capable format is the render_udev format-list
            // work already in place (see render/mod.rs), but there's no
            // per-output override wired from here into that yet — this
            // acknowledges the request over IPC so the shell's toggle
            // doesn't silently do nothing, without pretending the full
            // pipeline is connected end to end.
            info!("HDR {} requested for output {output} (negotiation path implemented; render tone-mapping is still a stub — see color_management.rs)", if enabled { "enable" } else { "disable" });
            state.ipc_broadcast(crate::ipc::messages::CompositorMessage::HdrStateChanged {
                output,
                hdr_active: false,
            });
        }
    }
}
