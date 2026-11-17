//! InputCapture activation + libei receiver → [`crate::capture_loop::CaptureLoop`].

use std::collections::HashSet;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use oink_foundation::Point;
use oink_foundation::input::{
    CaptureMode, CapturedEvent, Disposition, FeelProfile, InputEvent, KeyCode, MouseButton, ScrollPhase, ScrollUnit,
    SourceKind,
};
use reis::ei::{self, button::ButtonState, keyboard::KeyState as EiKeyState};
use reis::event::{DeviceCapability, EiEvent};

use crate::capture_loop::{self, CaptureLoop};
use crate::{InputError, source_time};

use super::keymap;
use super::portal::PortalSessions;
use super::runtime;

pub(crate) struct CaptureHandle {
    stop: Arc<AtomicBool>,
    stop_notify: Arc<tokio::sync::Notify>,
    thread: Option<JoinHandle<()>>,
}

impl CaptureHandle {
    pub(crate) fn start(
        portals: Arc<PortalSessions>,
        eis_fd: OwnedFd,
        mode: Arc<Mutex<CaptureMode>>,
        cursor: Arc<Mutex<Point>>,
        owner: Box<dyn CaptureLoop>,
    ) -> Result<Self, InputError> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_notify = Arc::new(tokio::sync::Notify::new());
        let stop_thread = Arc::clone(&stop);
        let stop_notify_thread = Arc::clone(&stop_notify);
        let thread = std::thread::Builder::new()
            .name("oink-input-capture".into())
            .spawn(move || {
                if let Err(e) = run(portals, eis_fd, mode, cursor, owner, stop_thread, stop_notify_thread) {
                    tracing::error!(%e, "Linux input capture loop exited");
                }
            })
            .map_err(|e| InputError::Platform(format!("启动截获线程失败：{e}")))?;
        Ok(Self {
            stop,
            stop_notify,
            thread: Some(thread),
        })
    }

    pub(crate) fn running(&self) -> bool {
        self.thread.as_ref().is_some_and(|thread| !thread.is_finished())
    }

    pub(crate) fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.stop_notify.notify_waiters();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Default)]
struct CaptureState {
    held_keys: HashSet<KeyCode>,
    held_buttons: HashSet<MouseButton>,
    repeat: Option<(KeyCode, Instant)>,
}

impl CaptureState {
    fn key_event(&mut self, code: KeyCode, pressed: bool, now: Instant) {
        let feel = FeelProfile::default();
        if pressed {
            self.held_keys.insert(code);
            if !code.is_modifier() && code != KeyCode::CAPS_LOCK {
                self.repeat = Some((code, now + Duration::from_millis(u64::from(feel.key_repeat_delay_ms))));
            }
        } else {
            self.held_keys.remove(&code);
            if self.repeat.is_some_and(|(held, _)| held == code) {
                self.repeat = None;
            }
        }
    }

    fn button_event(&mut self, button: MouseButton, pressed: bool) {
        if pressed {
            self.held_buttons.insert(button);
        } else {
            self.held_buttons.remove(&button);
        }
    }

    fn advance_repeat(&mut self, now: Instant) -> Option<KeyCode> {
        let (code, due) = self.repeat?;
        if now < due {
            return None;
        }
        let interval = Duration::from_millis(u64::from(FeelProfile::default().key_repeat_interval_ms.max(10)));
        self.repeat = Some((code, now + interval));
        Some(code)
    }

    fn release_all(&mut self, mode: &Mutex<CaptureMode>, cursor: &Mutex<Point>) {
        self.repeat = None;
        let remote = is_remote(mode);
        let cur = if remote {
            None
        } else {
            Some(*cursor.lock().unwrap_or_else(|e| e.into_inner()))
        };
        for button in self.held_buttons.drain() {
            let _ = dispatch(
                mode,
                CapturedEvent {
                    physical: true,
                    event: InputEvent::Button { button, pressed: false },
                    source: SourceKind::Mouse,
                    time_us: now_us(),
                    cursor: cur,
                },
            );
        }
        for code in self.held_keys.drain() {
            let _ = dispatch(
                mode,
                CapturedEvent {
                    physical: true,
                    event: InputEvent::Key { code, pressed: false },
                    source: SourceKind::Keyboard,
                    time_us: now_us(),
                    cursor: cur,
                },
            );
        }
    }
}

fn run(
    portals: Arc<PortalSessions>,
    eis_fd: OwnedFd,
    mode: Arc<Mutex<CaptureMode>>,
    cursor: Arc<Mutex<Point>>,
    owner: Box<dyn CaptureLoop>,
    stop: Arc<AtomicBool>,
    stop_notify: Arc<tokio::sync::Notify>,
) -> Result<(), InputError> {
    let wake = Arc::new(tokio::sync::Notify::new());
    let wake_cb = Arc::clone(&wake);
    capture_loop::install(owner, Arc::new(move || wake_cb.notify_one()));
    capture_loop::capture_available(true);

    // The future must run on this thread, which owns the thread-local capture loop.
    let result = runtime::handle().block_on(async move {
        let stream = UnixStream::from(eis_fd);
        stream
            .set_nonblocking(true)
            .map_err(|e| InputError::Platform(format!("EIS fd nonblocking：{e}")))?;
        let context =
            ei::Context::new(stream).map_err(|e| InputError::Platform(format!("创建 EI context 失败：{e}")))?;
        let _ = context.flush();
        let (_connection, mut events) = context
            .handshake_tokio("oink-input", ei::handshake::ContextType::Receiver)
            .await
            .map_err(|e| InputError::Platform(format!("EI handshake 失败：{e}")))?;

        let mut activated = portals
            .input_capture
            .receive_activated()
            .await
            .map_err(|e| InputError::Platform(format!("订阅 InputCapture Activated 失败：{e}")))?;
        let mut deactivated = portals
            .input_capture
            .receive_deactivated()
            .await
            .map_err(|e| InputError::Platform(format!("订阅 InputCapture Deactivated 失败：{e}")))?;
        let mut zones_changed = portals
            .input_capture
            .receive_zones_changed()
            .await
            .map_err(|e| InputError::Platform(format!("订阅 InputCapture ZonesChanged 失败：{e}")))?;

        let mut capturing = false;
        let mut state = CaptureState::default();
        let mut deadline = Instant::now() + Duration::from_millis(5);

        loop {
            if stop.load(Ordering::Acquire) {
                break;
            }

            let repeat_due = state
                .repeat
                .filter(|_| capturing && is_remote(&mode))
                .map(|(_, due)| due);
            let sleep_target = match repeat_due {
                Some(due) => deadline.min(due),
                None => deadline,
            };

            let mut poll_now = false;
            tokio::select! {
                _ = stop_notify.notified() => break,
                msg = activated.next() => {
                    let Some(msg) = msg else { break; };
                    let activation_id = msg.activation_id();
                    let synthetic = portals.take_arm_pending();
                    match super::portal::activation_kind(is_remote(&mode), synthetic) {
                        super::portal::ActivationKind::KeepAsRemote => {
                            // Armed after a remote pointer left Linux (or while already remote):
                            // KWin now hides the cursor; do not emit a fake physical event.
                            portals.set_activation(activation_id);
                            capturing = true;
                        }
                        super::portal::ActivationKind::ReleaseSynthetic => {
                            // `restore_local` leaves the arm ticket set so this late Activated
                            // is still synthetic (not a real edge). Release at the current local
                            // cursor so KWin does not trap local input.
                            let cur = *cursor.lock().unwrap_or_else(|e| e.into_inner());
                            let _ = portals.release_id_async(activation_id, cur).await;
                            capturing = false;
                        }
                        super::portal::ActivationKind::EmitEdge => {
                            portals.set_activation(activation_id);
                            capturing = true;
                            let cur = if let Some((x, y)) = msg.cursor_position() {
                                let raw = Point::new(f64::from(x), f64::from(y));
                                let clamped = portals.clamp_to_zones(raw);
                                let mut dx = raw.x - clamped.x;
                                let mut dy = raw.y - clamped.y;
                                if dx == 0.0 && dy == 0.0 {
                                    (dx, dy) = portals.outward_step_at(clamped);
                                }
                                *cursor.lock().unwrap_or_else(|e| e.into_inner()) = clamped;
                                let _ = dispatch(
                                    &mode,
                                    CapturedEvent {
                                        physical: true,
                                        event: InputEvent::PointerDelta { dx, dy },
                                        source: SourceKind::Mouse,
                                        time_us: now_us(),
                                        cursor: Some(clamped),
                                    },
                                );
                                clamped
                            } else {
                                *cursor.lock().unwrap_or_else(|e| e.into_inner())
                            };
                            // If the router kept focus local (wall, offline peer, paused, locked,
                            // or edge resistance not yet exceeded), release InputCapture immediately
                            // so the cursor stays visible and local clicks/keys work normally.
                            if !is_remote(&mode) {
                                let _ = portals.release_id_async(activation_id, cur).await;
                                capturing = false;
                            }
                            poll_now = true;
                        }
                    }
                }
                msg = deactivated.next() => {
                    let Some(msg) = msg else { break; };
                    portals.clear_activation(msg.activation_id());
                    capturing = portals.captured();
                    if !capturing {
                        state.release_all(&mode, &cursor);
                        poll_now = true;
                    }
                }
                msg = zones_changed.next() => {
                    let Some(_msg) = msg else { break; };
                    if let Err(error) = portals.refresh_zones().await {
                        tracing::warn!(%error, "刷新 InputCapture zones 失败");
                    }
                }
                evt = events.next() => {
                    let Some(evt) = evt else { break; };
                    let evt = evt.map_err(|e| InputError::Platform(format!("EI 事件错误：{e}")))?;
                    if handle_ei(&portals, &context, &mode, &cursor, &mut capturing, &mut state, evt).await? {
                        poll_now = true;
                    }
                }
                _ = wake.notified() => {
                    poll_now = true;
                }
                _ = tokio::time::sleep_until(sleep_target.into()) => {
                    poll_now = true;
                }
            }

            let now = Instant::now();
            if capturing
                && is_remote(&mode)
                && let Some(code) = state.advance_repeat(now)
            {
                let _ = dispatch(
                    &mode,
                    CapturedEvent {
                        physical: true,
                        event: InputEvent::Key { code, pressed: true },
                        source: SourceKind::Keyboard,
                        time_us: now_us(),
                        cursor: None,
                    },
                );
                poll_now = true;
            }

            if poll_now || Instant::now() >= deadline {
                let (next, stopped) = capture_loop::poll(Instant::now());
                if stopped {
                    break;
                }
                deadline = next.unwrap_or_else(|| Instant::now() + Duration::from_millis(16));
            }
        }
        Ok(())
    });
    capture_loop::capture_available(false);
    capture_loop::clear();
    result
}

fn now_us() -> u64 {
    source_time::capture_time().0 as u64
}

async fn handle_ei(
    portals: &PortalSessions,
    context: &ei::Context,
    mode: &Mutex<CaptureMode>,
    cursor: &Mutex<Point>,
    capturing: &mut bool,
    state: &mut CaptureState,
    evt: EiEvent,
) -> Result<bool, InputError> {
    let mut dispatched = false;
    match evt {
        EiEvent::SeatAdded(seat) => {
            seat.seat.bind_capabilities(
                DeviceCapability::Pointer
                    | DeviceCapability::PointerAbsolute
                    | DeviceCapability::Keyboard
                    | DeviceCapability::Scroll
                    | DeviceCapability::Button,
            );
            let _ = context.flush();
        }
        EiEvent::PointerMotion(motion) if *capturing => {
            let remote = is_remote(mode);
            let dx = f64::from(motion.dx);
            let dy = f64::from(motion.dy);
            let event = CapturedEvent {
                physical: true,
                event: InputEvent::PointerDelta { dx, dy },
                source: SourceKind::Mouse,
                time_us: now_us(),
                cursor: if remote {
                    None
                } else {
                    let mut c = cursor.lock().unwrap_or_else(|e| e.into_inner());
                    *c = portals.clamp_to_zones(Point::new(c.x + dx, c.y + dy));
                    Some(*c)
                },
            };
            let _ = dispatch(mode, event);
            dispatched = true;
        }
        EiEvent::Button(btn) if *capturing => {
            let button = match btn.button {
                0x110 => MouseButton::Left,
                0x111 => MouseButton::Right,
                0x112 => MouseButton::Middle,
                0x113 => MouseButton::Back,
                0x114 => MouseButton::Forward,
                _ => return Ok(false),
            };
            let pressed = matches!(btn.state, ButtonState::Press);
            state.button_event(button, pressed);
            let remote = is_remote(mode);
            let cur = *cursor.lock().unwrap_or_else(|e| e.into_inner());
            let _ = dispatch(
                mode,
                CapturedEvent {
                    physical: true,
                    event: InputEvent::Button { button, pressed },
                    source: SourceKind::Mouse,
                    time_us: now_us(),
                    cursor: if remote { None } else { Some(cur) },
                },
            );
            dispatched = true;
        }
        // libei axes are positive toward the bottom/right; Oink scroll is
        // positive toward the document top/left.
        EiEvent::ScrollDelta(scroll) if *capturing => {
            let remote = is_remote(mode);
            let cur = *cursor.lock().unwrap_or_else(|e| e.into_inner());
            let _ = dispatch(
                mode,
                CapturedEvent {
                    physical: true,
                    event: InputEvent::Scroll {
                        dx: -f64::from(scroll.dx),
                        dy: -f64::from(scroll.dy),
                        unit: ScrollUnit::Pixel,
                        phase: ScrollPhase::Changed,
                        wheel_steps: None,
                    },
                    source: SourceKind::Mouse,
                    time_us: now_us(),
                    cursor: if remote { None } else { Some(cur) },
                },
            );
            dispatched = true;
        }
        EiEvent::ScrollDiscrete(scroll) if *capturing => {
            let remote = is_remote(mode);
            let cur = *cursor.lock().unwrap_or_else(|e| e.into_inner());
            // Discrete values are in 1/120 of a wheel notch.
            let dx = -f64::from(scroll.discrete_dx) / 120.0;
            let dy = -f64::from(scroll.discrete_dy) / 120.0;
            let _ = dispatch(
                mode,
                CapturedEvent {
                    physical: true,
                    event: InputEvent::Scroll {
                        dx,
                        dy,
                        unit: ScrollUnit::Line,
                        phase: ScrollPhase::None,
                        wheel_steps: Some(Point::new(dx, dy)),
                    },
                    source: SourceKind::Mouse,
                    time_us: now_us(),
                    cursor: if remote { None } else { Some(cur) },
                },
            );
            dispatched = true;
        }
        EiEvent::KeyboardKey(key) if *capturing => {
            let Some(code) = keymap::evdev_to_hid(key.key) else {
                return Ok(false);
            };
            let pressed = matches!(key.state, EiKeyState::Press);
            state.key_event(code, pressed, Instant::now());
            let remote = is_remote(mode);
            let cur = *cursor.lock().unwrap_or_else(|e| e.into_inner());
            let _ = dispatch(
                mode,
                CapturedEvent {
                    physical: true,
                    event: InputEvent::Key { code, pressed },
                    source: SourceKind::Keyboard,
                    time_us: now_us(),
                    cursor: if remote { None } else { Some(cur) },
                },
            );
            dispatched = true;
        }
        _ => {}
    }

    if dispatched && *capturing && !is_remote(mode) {
        let cur = *cursor.lock().unwrap_or_else(|e| e.into_inner());
        let _ = portals.release_current_async(cur).await;
        *capturing = false;
        state.repeat = None;
    }

    Ok(dispatched)
}

fn is_remote(mode: &Mutex<CaptureMode>) -> bool {
    matches!(
        *mode.lock().unwrap_or_else(|e| e.into_inner()),
        CaptureMode::Remote { .. }
    )
}

fn dispatch(mode: &Mutex<CaptureMode>, event: CapturedEvent) -> Disposition {
    let disposition = capture_loop::event(&event);
    if is_remote(mode) {
        Disposition::Swallow
    } else {
        disposition
    }
}
