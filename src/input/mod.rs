use smithay::{
    backend::input::{
        Axis, AxisSource, ButtonState, InputBackend, InputEvent,
        KeyState, KeyboardKeyEvent,
        PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
        PointerMotionAbsoluteEvent,
    },
    desktop::WindowSurfaceType,
    input::{
        keyboard::{FilterResult, Keysym},
        pointer::{
            AxisFrame, ButtonEvent,
            GrabStartData as PointerGrabStartData,
            MotionEvent, PointerGrab, PointerInnerHandle, RelativeMotionEvent,
        },
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, Rectangle, Size, SERIAL_COUNTER},
    wayland::seat::WaylandFocus,
};

use crate::state::BlueState;

pub fn handle_input<B: InputBackend>(state: &mut BlueState, event: InputEvent<B>) {
    state.record_input();

    match event {
        InputEvent::Keyboard { event } => handle_keyboard(state, &event),
        InputEvent::PointerMotion { event } => handle_pointer_motion(state, &event),
        InputEvent::PointerMotionAbsolute { event } => {
            handle_pointer_motion_abs(state, &event)
        }
        InputEvent::PointerButton { event } => handle_pointer_button(state, &event),
        InputEvent::PointerAxis { event } => handle_pointer_axis(state, &event),
        _ => {}
    }
}

// ── Keyboard ──────────────────────────────────────────────────────────────

fn handle_keyboard<B: InputBackend, E: KeyboardKeyEvent<B>>(
    state: &mut BlueState,
    event: &E,
) {
    let serial = SERIAL_COUNTER.next_serial();
    let keyboard = state.seat.get_keyboard().unwrap();

    keyboard.input(
        state,
        event.key_code(),
        event.state(),
        serial,
        event.time_msec(),
        |state, mods, handle| {
            let sym = handle.modified_sym();
            let pressed = event.state() == KeyState::Pressed;

            // ── Alt+Tab (window switcher) ─────────────────────────────────
            if mods.alt && sym == Keysym::Tab && pressed {
                if !state.show_switcher {
                    state.show_switcher = true;
                    state.switcher_index = 0;
                } else {
                    state.cycle_switcher(true);
                }
                return FilterResult::Intercept(());
            }

            // ── Alt+Shift+Tab (backwards switcher) ────────────────────────
            if mods.alt && mods.shift && sym == Keysym::Tab && pressed {
                if state.show_switcher {
                    state.cycle_switcher(false);
                }
                return FilterResult::Intercept(());
            }

            // ── Alt release → commit switcher ─────────────────────────────
            if (sym == Keysym::Alt_L || sym == Keysym::Alt_R)
                && event.state() == KeyState::Released
                && state.show_switcher
            {
                state.apply_switcher_selection();
                return FilterResult::Intercept(());
            }

            // ── Super / Win key ───────────────────────────────────────────
            if sym == Keysym::Super_L || sym == Keysym::Super_R {
                if pressed {
                    state.super_pressed = true;
                    state.super_used = false;
                } else {
                    if state.super_pressed && !state.super_used {
                        state.toggle_start_menu();
                    }
                    state.super_pressed = false;
                    state.super_used = false;
                }
                return FilterResult::Intercept(());
            }

            // ── Win+Tab → full-screen app picker ─────────────────────────
            if mods.logo && sym == Keysym::Tab && pressed {
                state.super_used = true;
                state.toggle_fullscreen_menu();
                return FilterResult::Intercept(());
            }

            // ── Win+1..4 → switch workspace ───────────────────────────────
            if mods.logo && pressed {
                let ws = match sym {
                    Keysym::_1 => Some(0usize),
                    Keysym::_2 => Some(1),
                    Keysym::_3 => Some(2),
                    Keysym::_4 => Some(3),
                    _ => None,
                };
                if let Some(idx) = ws {
                    state.super_used = true;
                    state.switch_workspace(idx);
                    return FilterResult::Intercept(());
                }
            }

            // ── Win+Arrow → workspace ─────────────────────────────────────
            if mods.logo && sym == Keysym::Right && pressed {
                state.super_used = true;
                let next = (state.current_workspace + 1).min(state.workspace_count - 1);
                state.switch_workspace(next);
                return FilterResult::Intercept(());
            }
            if mods.logo && sym == Keysym::Left && pressed {
                state.super_used = true;
                let prev = state.current_workspace.saturating_sub(1);
                state.switch_workspace(prev);
                return FilterResult::Intercept(());
            }

            // ── Win+Up → maximize focused window ─────────────────────────
            if mods.logo && sym == Keysym::Up && pressed {
                state.super_used = true;
                if let Some(surface) = state.seat.get_keyboard().unwrap().current_focus() {
                    if let Some(win) = state.window_by_surface(&surface) {
                        if let Some(t) = win.toplevel() {
                            t.with_pending_state(|s| {
                                if s.states.contains(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Maximized) {
                                    s.states.unset(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Maximized);
                                } else {
                                    s.states.set(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Maximized);
                                }
                            });
                            t.send_configure();
                        }
                    }
                }
                return FilterResult::Intercept(());
            }

            // ── Win+Down → minimize focused window ───────────────────────
            if mods.logo && sym == Keysym::Down && pressed {
                state.super_used = true;
                if let Some(surface) = state.seat.get_keyboard().unwrap().current_focus() {
                    if let Some(win) = state.window_by_surface(&surface) {
                        let id = BlueState::window_id(&win);
                        if let Some(meta) = state.window_meta.get_mut(&id) {
                            meta.is_minimized = true;
                        }
                    }
                }
                return FilterResult::Intercept(());
            }

            // ── Alt+F4 → close focused window ─────────────────────────────
            if mods.alt && sym == Keysym::F4 && pressed {
                if let Some(surface) = state.seat.get_keyboard().unwrap().current_focus() {
                    if let Some(win) = state.window_by_surface(&surface) {
                        if let Some(t) = win.toplevel() {
                            t.send_close();
                        }
                    }
                }
                return FilterResult::Intercept(());
            }

            // ── Ctrl+Alt+T → launch terminal ──────────────────────────────
            if mods.ctrl && mods.alt && sym == Keysym::t && pressed {
                let _ = std::process::Command::new("sh")
                    .args(["-c", "kitty & || alacritty & || gnome-terminal & || xterm &"])
                    .spawn();
                return FilterResult::Intercept(());
            }

            // ── PrintScreen → screenshot ──────────────────────────────────
            if sym == Keysym::Print && pressed {
                let home = dirs::home_dir().unwrap_or_default();
                let path = home
                    .join("Pictures")
                    .join(format!(
                        "screenshot_{}.png",
                        chrono::Local::now().format("%Y%m%d_%H%M%S")
                    ))
                    .to_string_lossy()
                    .to_string();
                let _ = std::process::Command::new("sh")
                    .arg("-c")
                    .arg(format!(
                        "flameshot gui -p '{}' 2>/dev/null || scrot '{}' 2>/dev/null",
                        path, path
                    ))
                    .spawn();
                return FilterResult::Intercept(());
            }

            // ── Escape → close panels / switcher ──────────────────────────
            if sym == Keysym::Escape && pressed {
                if state.show_switcher {
                    state.show_switcher = false;
                    return FilterResult::Intercept(());
                }
            }

            FilterResult::Forward
        },
    );
}

// ── Pointer motion ────────────────────────────────────────────────────────

fn handle_pointer_motion<B: InputBackend, E: PointerMotionEvent<B>>(
    state: &mut BlueState,
    event: &E,
) {
    let serial = SERIAL_COUNTER.next_serial();
    let delta = event.delta();

    // Clamp to output bounds
    let (min_x, min_y, max_x, max_y) = output_bounds(state);
    state.pointer_location.x = (state.pointer_location.x + delta.x).clamp(min_x, max_x);
    state.pointer_location.y = (state.pointer_location.y + delta.y).clamp(min_y, max_y);

    update_pointer_focus(state, serial, event.time_msec());
}

fn handle_pointer_motion_abs<B: InputBackend, E: PointerMotionAbsoluteEvent<B>>(
    state: &mut BlueState,
    event: &E,
) {
    let serial = SERIAL_COUNTER.next_serial();
    let size = {
        state
            .space
            .outputs()
            .next()
            .and_then(|o| state.space.output_geometry(o))
            .map(|g| g.size)
            .unwrap_or(Size::from((1920, 1080)))
    };
    state.pointer_location = event.position_transformed(size);
    update_pointer_focus(state, serial, event.time_msec());
}

fn output_bounds(state: &BlueState) -> (f64, f64, f64, f64) {
    state
        .space
        .outputs()
        .next()
        .and_then(|o| state.space.output_geometry(o))
        .map(|g| {
            (
                g.loc.x as f64,
                g.loc.y as f64,
                (g.loc.x + g.size.w) as f64,
                (g.loc.y + g.size.h) as f64,
            )
        })
        .unwrap_or((0.0, 0.0, 1920.0, 1080.0))
}

fn update_pointer_focus(state: &mut BlueState, serial: smithay::utils::Serial, time: u32) {
    let pointer = state.seat.get_pointer().unwrap();
    let pos = state.pointer_location;

    let focus: Option<(WlSurface, Point<f64, Logical>)> = state
        .space
        .element_under(pos)
        .and_then(|(win, win_loc)| {
            let rel = pos - win_loc.to_f64();
            win.surface_under(rel, WindowSurfaceType::ALL)
                .map(|(s, sp)| (s, (win_loc + sp).to_f64()))
        });

    pointer.motion(
        state,
        focus,
        &MotionEvent {
            location: pos,
            serial,
            time,
        },
    );
    pointer.frame(state);
}

// ── Pointer button ────────────────────────────────────────────────────────

fn handle_pointer_button<B: InputBackend, E: PointerButtonEvent<B>>(
    state: &mut BlueState,
    event: &E,
) {
    let serial = SERIAL_COUNTER.next_serial();
    let pos = state.pointer_location;

    if event.state() == ButtonState::Pressed {
        let maybe_window = state
            .space
            .element_under(pos)
            .map(|(w, _)| w.clone());

        if let Some(window) = maybe_window {
            state.space.raise_element(&window, true);
            let keyboard = state.seat.get_keyboard().unwrap();
            if let Some(surface) = window.wl_surface() {
                keyboard.set_focus(state, Some(surface.into_owned()), serial);
            }
        } else {
            // Click on empty desktop - unfocus
            let keyboard = state.seat.get_keyboard().unwrap();
            keyboard.set_focus(state, Option::<WlSurface>::None, serial);
        }
    }

    let pointer = state.seat.get_pointer().unwrap();
    pointer.button(
        state,
        &ButtonEvent {
            button: event.button_code(),
            state: event.state(),
            serial,
            time: event.time_msec(),
        },
    );
    pointer.frame(state);
}

// ── Pointer axis (scroll) ─────────────────────────────────────────────────

fn handle_pointer_axis<B: InputBackend, E: PointerAxisEvent<B>>(
    state: &mut BlueState,
    event: &E,
) {
    let pointer = state.seat.get_pointer().unwrap();
    let mut frame = AxisFrame::new(event.time_msec()).source(AxisSource::Wheel);

    for axis in [Axis::Horizontal, Axis::Vertical] {
        if let Some(v) = event.amount(axis) {
            frame = frame
                .relative_direction(axis, event.relative_direction(axis))
                .value(axis, v);
            if let Some(d) = event.amount_v120(axis) {
                frame = frame.v120(axis, d as i32);
            }
        }
    }

    pointer.axis(state, frame);
    pointer.frame(state);
}

// ── Move grab ─────────────────────────────────────────────────────────────

pub struct MoveGrab {
    pub start_data: PointerGrabStartData<BlueState>,
    pub window: smithay::desktop::Window,
    pub initial_window_location: Point<i32, Logical>,
}

impl PointerGrab<BlueState> for MoveGrab {
    fn motion(
        &mut self,
        data: &mut BlueState,
        handle: &mut PointerInnerHandle<'_, BlueState>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);
        let delta = event.location - self.start_data.location;
        let new_loc = self.initial_window_location + delta.to_i32_round();
        data.space.map_element(self.window.clone(), new_loc, true);
    }

    fn relative_motion(
        &mut self,
        data: &mut BlueState,
        handle: &mut PointerInnerHandle<'_, BlueState>,
        focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, focus, event);
    }

    fn button(
        &mut self,
        data: &mut BlueState,
        handle: &mut PointerInnerHandle<'_, BlueState>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);
        if event.state == ButtonState::Released {
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(
        &mut self,
        data: &mut BlueState,
        handle: &mut PointerInnerHandle<'_, BlueState>,
        details: AxisFrame,
    ) {
        handle.axis(data, details);
    }

    fn frame(
        &mut self,
        data: &mut BlueState,
        handle: &mut PointerInnerHandle<'_, BlueState>,
    ) {
        handle.frame(data);
    }

    fn gesture_swipe_begin(
        &mut self,
        _: &mut BlueState,
        _: &mut PointerInnerHandle<'_, BlueState>,
        _: &smithay::input::pointer::GestureSwipeBeginEvent,
    ) {
    }
    fn gesture_swipe_update(
        &mut self,
        _: &mut BlueState,
        _: &mut PointerInnerHandle<'_, BlueState>,
        _: &smithay::input::pointer::GestureSwipeUpdateEvent,
    ) {
    }
    fn gesture_swipe_end(
        &mut self,
        _: &mut BlueState,
        _: &mut PointerInnerHandle<'_, BlueState>,
        _: &smithay::input::pointer::GestureSwipeEndEvent,
    ) {
    }
    fn gesture_pinch_begin(
        &mut self,
        _: &mut BlueState,
        _: &mut PointerInnerHandle<'_, BlueState>,
        _: &smithay::input::pointer::GesturePinchBeginEvent,
    ) {
    }
    fn gesture_pinch_update(
        &mut self,
        _: &mut BlueState,
        _: &mut PointerInnerHandle<'_, BlueState>,
        _: &smithay::input::pointer::GesturePinchUpdateEvent,
    ) {
    }
    fn gesture_pinch_end(
        &mut self,
        _: &mut BlueState,
        _: &mut PointerInnerHandle<'_, BlueState>,
        _: &smithay::input::pointer::GesturePinchEndEvent,
    ) {
    }
    fn gesture_hold_begin(
        &mut self,
        _: &mut BlueState,
        _: &mut PointerInnerHandle<'_, BlueState>,
        _: &smithay::input::pointer::GestureHoldBeginEvent,
    ) {
    }
    fn gesture_hold_end(
        &mut self,
        _: &mut BlueState,
        _: &mut PointerInnerHandle<'_, BlueState>,
        _: &smithay::input::pointer::GestureHoldEndEvent,
    ) {
    }

    fn start_data(&self) -> &PointerGrabStartData<BlueState> {
        &self.start_data
    }

    fn unset(&mut self, _: &mut BlueState) {}
}

pub fn start_move_grab(
    state: &mut BlueState,
    window: smithay::desktop::Window,
    start_data: PointerGrabStartData<BlueState>,
    _serial: smithay::utils::Serial,
) {
    let initial = state
        .space
        .element_location(&window)
        .unwrap_or_default();

    let grab = MoveGrab {
        start_data,
        window,
        initial_window_location: initial,
    };

    state.seat.get_pointer().unwrap().set_grab(
        state,
        grab,
        SERIAL_COUNTER.next_serial(),
        smithay::input::pointer::Focus::Clear,
    );
}

// ── Resize grab ───────────────────────────────────────────────────────────
//
// Previously `resize_request` was a no-op stub for both xdg-shell toplevels
// (state/mod.rs) and XWayland/X11 windows (xwayland/mod.rs) — dragging a
// window's edge/corner from a client-side decoration or the compositor's
// own titlebar did nothing. This mirrors the existing `MoveGrab` pattern.
//
// Note on correctness: for xdg-shell toplevels the "proper" way to handle
// north/west edge resizes is to let the client ack the new size via
// `xdg_surface.configure` and only reposition the window once the new
// buffer has actually committed (otherwise the window can visually jitter
// for a frame or two while the client catches up). This implementation
// takes the simpler approach of resizing eagerly, which is a large
// functional improvement over "resize does nothing at all" and matches
// what many lightweight compositors do, but a follow-up could track
// pending-size-vs-committed-size per window for pixel-perfect behavior.

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResizeEdges {
    pub top: bool,
    pub bottom: bool,
    pub left: bool,
    pub right: bool,
}

impl From<smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge> for ResizeEdges {
    fn from(e: smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge) -> Self {
        use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge as E;
        match e {
            E::Top => Self { top: true, ..Default::default() },
            E::Bottom => Self { bottom: true, ..Default::default() },
            E::Left => Self { left: true, ..Default::default() },
            E::Right => Self { right: true, ..Default::default() },
            E::TopLeft => Self { top: true, left: true, ..Default::default() },
            E::TopRight => Self { top: true, right: true, ..Default::default() },
            E::BottomLeft => Self { bottom: true, left: true, ..Default::default() },
            E::BottomRight => Self { bottom: true, right: true, ..Default::default() },
            _ => Self::default(),
        }
    }
}

impl From<smithay::xwayland::xwm::ResizeEdge> for ResizeEdges {
    // smithay's X11 `ResizeEdge` variant names have shifted a bit across
    // revisions; matched defensively with a wildcard fallback so this
    // keeps compiling even if the pinned rev's variant set differs
    // slightly (worst case: an unrecognized edge falls back to a
    // bottom-right resize, which is the most common default anyway).
    fn from(e: smithay::xwayland::xwm::ResizeEdge) -> Self {
        use smithay::xwayland::xwm::ResizeEdge as E;
        match e {
            E::Top => Self { top: true, ..Default::default() },
            E::Bottom => Self { bottom: true, ..Default::default() },
            E::Left => Self { left: true, ..Default::default() },
            E::Right => Self { right: true, ..Default::default() },
            E::TopLeft => Self { top: true, left: true, ..Default::default() },
            E::TopRight => Self { top: true, right: true, ..Default::default() },
            E::BottomLeft => Self { bottom: true, left: true, ..Default::default() },
            E::BottomRight => Self { bottom: true, right: true, ..Default::default() },
            #[allow(unreachable_patterns)]
            _ => Self { bottom: true, right: true, ..Default::default() },
        }
    }
}

pub struct ResizeGrab {
    pub start_data: PointerGrabStartData<BlueState>,
    pub window: smithay::desktop::Window,
    pub edges: ResizeEdges,
    pub initial_window_location: Point<i32, Logical>,
    pub initial_window_size: Size<i32, Logical>,
}

const MIN_WINDOW_SIZE: i32 = 32;

impl PointerGrab<BlueState> for ResizeGrab {
    fn motion(
        &mut self,
        data: &mut BlueState,
        handle: &mut PointerInnerHandle<'_, BlueState>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);
        let delta = (event.location - self.start_data.location).to_i32_round::<i32>();

        let mut new_w = self.initial_window_size.w;
        let mut new_h = self.initial_window_size.h;
        let mut new_x = self.initial_window_location.x;
        let mut new_y = self.initial_window_location.y;

        if self.edges.right {
            new_w = (self.initial_window_size.w + delta.x).max(MIN_WINDOW_SIZE);
        } else if self.edges.left {
            new_w = (self.initial_window_size.w - delta.x).max(MIN_WINDOW_SIZE);
            new_x = self.initial_window_location.x + (self.initial_window_size.w - new_w);
        }
        if self.edges.bottom {
            new_h = (self.initial_window_size.h + delta.y).max(MIN_WINDOW_SIZE);
        } else if self.edges.top {
            new_h = (self.initial_window_size.h - delta.y).max(MIN_WINDOW_SIZE);
            new_y = self.initial_window_location.y + (self.initial_window_size.h - new_h);
        }

        let new_size = Size::from((new_w, new_h));
        let new_loc = Point::from((new_x, new_y));
        apply_resize(data, &self.window, new_loc, new_size, self.edges);
    }

    fn relative_motion(
        &mut self,
        data: &mut BlueState,
        handle: &mut PointerInnerHandle<'_, BlueState>,
        focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, focus, event);
    }

    fn button(
        &mut self,
        data: &mut BlueState,
        handle: &mut PointerInnerHandle<'_, BlueState>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);
        if event.state == ButtonState::Released {
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(
        &mut self,
        data: &mut BlueState,
        handle: &mut PointerInnerHandle<'_, BlueState>,
        details: AxisFrame,
    ) {
        handle.axis(data, details);
    }

    fn frame(&mut self, data: &mut BlueState, handle: &mut PointerInnerHandle<'_, BlueState>) {
        handle.frame(data);
    }

    fn gesture_swipe_begin(&mut self, _: &mut BlueState, _: &mut PointerInnerHandle<'_, BlueState>, _: &smithay::input::pointer::GestureSwipeBeginEvent) {}
    fn gesture_swipe_update(&mut self, _: &mut BlueState, _: &mut PointerInnerHandle<'_, BlueState>, _: &smithay::input::pointer::GestureSwipeUpdateEvent) {}
    fn gesture_swipe_end(&mut self, _: &mut BlueState, _: &mut PointerInnerHandle<'_, BlueState>, _: &smithay::input::pointer::GestureSwipeEndEvent) {}
    fn gesture_pinch_begin(&mut self, _: &mut BlueState, _: &mut PointerInnerHandle<'_, BlueState>, _: &smithay::input::pointer::GesturePinchBeginEvent) {}
    fn gesture_pinch_update(&mut self, _: &mut BlueState, _: &mut PointerInnerHandle<'_, BlueState>, _: &smithay::input::pointer::GesturePinchUpdateEvent) {}
    fn gesture_pinch_end(&mut self, _: &mut BlueState, _: &mut PointerInnerHandle<'_, BlueState>, _: &smithay::input::pointer::GesturePinchEndEvent) {}
    fn gesture_hold_begin(&mut self, _: &mut BlueState, _: &mut PointerInnerHandle<'_, BlueState>, _: &smithay::input::pointer::GestureHoldBeginEvent) {}
    fn gesture_hold_end(&mut self, _: &mut BlueState, _: &mut PointerInnerHandle<'_, BlueState>, _: &smithay::input::pointer::GestureHoldEndEvent) {}

    fn start_data(&self) -> &PointerGrabStartData<BlueState> {
        &self.start_data
    }

    fn unset(&mut self, _: &mut BlueState) {}
}

/// Pushes a resized geometry to whichever kind of window this is —
/// xdg-shell toplevel (via a new configure) or an XWayland/X11 window
/// (via a direct `configure()`, since X11 has no client-ack round trip).
fn apply_resize(
    state: &mut BlueState,
    window: &smithay::desktop::Window,
    new_loc: Point<i32, Logical>,
    new_size: Size<i32, Logical>,
    edges: ResizeEdges,
) {
    if let Some(toplevel) = window.toplevel() {
        toplevel.with_pending_state(|s| {
            s.size = Some(new_size);
        });
        toplevel.send_configure();
        // Only reposition eagerly for edges that move the window's origin
        // (top/left) — the alternative (waiting for the client's next
        // commit) is more correct but requires per-window pending-state
        // tracking that doesn't exist yet.
        if edges.top || edges.left {
            state.space.map_element(window.clone(), new_loc, false);
        }
    } else if let Some(x11) = window.x11_surface() {
        let geo = Rectangle::new(new_loc, new_size);
        if let Err(e) = x11.configure(geo) {
            tracing::warn!("X11 resize configure failed: {}", e);
        }
        state.space.map_element(window.clone(), new_loc, false);
    }
}

pub fn start_resize_grab(
    state: &mut BlueState,
    window: smithay::desktop::Window,
    start_data: PointerGrabStartData<BlueState>,
    edges: ResizeEdges,
) {
    let initial_window_location = state.space.element_location(&window).unwrap_or_default();
    let initial_window_size = window.geometry().size;

    let grab = ResizeGrab {
        start_data,
        window,
        edges,
        initial_window_location,
        initial_window_size,
    };

    state.seat.get_pointer().unwrap().set_grab(
        state,
        grab,
        SERIAL_COUNTER.next_serial(),
        smithay::input::pointer::Focus::Clear,
    );
}

#[cfg(test)]
mod resize_edges_tests {
    use super::ResizeEdges;
    use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge as XdgEdge;

    #[test]
    fn xdg_single_edges_map_correctly() {
        assert_eq!(ResizeEdges::from(XdgEdge::Top), ResizeEdges { top: true, bottom: false, left: false, right: false });
        assert_eq!(ResizeEdges::from(XdgEdge::Bottom), ResizeEdges { top: false, bottom: true, left: false, right: false });
        assert_eq!(ResizeEdges::from(XdgEdge::Left), ResizeEdges { top: false, bottom: false, left: true, right: false });
        assert_eq!(ResizeEdges::from(XdgEdge::Right), ResizeEdges { top: false, bottom: false, left: false, right: true });
    }

    #[test]
    fn xdg_corner_edges_set_two_flags() {
        assert_eq!(ResizeEdges::from(XdgEdge::TopLeft), ResizeEdges { top: true, left: true, bottom: false, right: false });
        assert_eq!(ResizeEdges::from(XdgEdge::TopRight), ResizeEdges { top: true, right: true, bottom: false, left: false });
        assert_eq!(ResizeEdges::from(XdgEdge::BottomLeft), ResizeEdges { bottom: true, left: true, top: false, right: false });
        assert_eq!(ResizeEdges::from(XdgEdge::BottomRight), ResizeEdges { bottom: true, right: true, top: false, left: false });
    }

    #[test]
    fn xdg_none_edge_sets_no_flags() {
        assert_eq!(ResizeEdges::from(XdgEdge::None), ResizeEdges::default());
    }
}
