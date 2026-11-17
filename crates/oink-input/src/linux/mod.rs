//! Linux Wayland input via xdg-desktop-portal + libei (KDE Plasma 6.1+).
//!
//! - Capture: `InputCapture` barriers + EIS receiver → [`crate::capture_loop`]
//! - Inject: `RemoteDesktop` EIS sender (absolute pointer device covers all outputs)
//! - Clipboard: `RequestClipboard` on the RemoteDesktop session (Plasma 6.4+)
//! - Coordinates: compositor logical pixels (same as `oink-display` linux)

mod capture;
mod clipboard_hooks;
mod eis_inject;
mod keymap;
mod portal;
mod runtime;

pub use clipboard_hooks::ClipboardPortalHooks;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use oink_foundation::Point;
use oink_foundation::input::{
    CaptureMode, FeelProfile, InjectEvent, InputPermissions, KeyCode, MouseButton, PermissionState, ScrollPhase,
    ScrollUnit,
};

use crate::{EventHandler, InputBackend, InputError, capture_loop};

/// Oink converts wheel lines using 32 px/line × 3 lines/notch = 96 px per 120 v120 notch.
const OINK_WHEEL_PIXELS_PER_NOTCH: f64 = 96.0;
const WHEEL_LINES_PER_NOTCH: f64 = 3.0;

pub fn backend(clipboard: Option<Arc<ClipboardPortalHooks>>) -> Result<Box<dyn InputBackend>, InputError> {
    let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
    if !session.eq_ignore_ascii_case("wayland") && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return Err(InputError::Platform(
            "需要 Wayland 会话（XDG_SESSION_TYPE=wayland 或 WAYLAND_DISPLAY）".into(),
        ));
    }
    Ok(Box::new(LinuxInput {
        clipboard,
        portals: Mutex::new(None),
        capture: Mutex::new(None),
        mode: Arc::new(Mutex::new(CaptureMode::Local)),
        cursor: Arc::new(Mutex::new(Point::new(0., 0.))),
        scroll_acc: Mutex::new((0., 0.)),
        granted: AtomicBool::new(false),
        denied: AtomicBool::new(false),
    }))
}

struct LinuxInput {
    clipboard: Option<Arc<ClipboardPortalHooks>>,
    portals: Mutex<Option<Arc<portal::PortalSessions>>>,
    capture: Mutex<Option<capture::CaptureHandle>>,
    mode: Arc<Mutex<CaptureMode>>,
    cursor: Arc<Mutex<Point>>,
    scroll_acc: Mutex<(f64, f64)>,
    granted: AtomicBool,
    denied: AtomicBool,
}

impl LinuxInput {
    fn ensure_portals(&self) -> Result<Arc<portal::PortalSessions>, InputError> {
        let mut slot = self.portals.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = slot.as_ref() {
            return Ok(Arc::clone(existing));
        }
        match portal::PortalSessions::open(self.clipboard.clone()) {
            Ok(sessions) => {
                self.granted.store(true, Ordering::Release);
                self.denied.store(false, Ordering::Release);
                *slot = Some(Arc::clone(&sessions));
                Ok(sessions)
            }
            Err(e) => {
                self.denied.store(true, Ordering::Release);
                self.granted.store(false, Ordering::Release);
                Err(e)
            }
        }
    }

    fn portals(&self) -> Result<Arc<portal::PortalSessions>, InputError> {
        self.portals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .cloned()
            .ok_or(InputError::NotStarted)
    }

    fn inject_scroll(
        &self,
        eis: &eis_inject::Injector,
        dx: f64,
        dy: f64,
        unit: ScrollUnit,
        phase: ScrollPhase,
    ) -> Result<(), InputError> {
        // Oink scroll is positive toward the document top/left; EI axes are
        // positive toward the bottom/right.
        match (unit, phase) {
            (ScrollUnit::Line, _) => {
                let (vx, vy) = self.take_v120(-dx * 120.0, -dy * 120.0);
                eis.scroll_discrete(vx, vy)
            }
            // Discrete mouse wheel routed as Pixel (`ScrollPhase::None`, 96 px/notch)
            // maps to v120 ticks so Wayland apps neither jump 6.4× nor miss axis_value120.
            (ScrollUnit::Pixel, ScrollPhase::None) => {
                let scale = 120.0 / OINK_WHEEL_PIXELS_PER_NOTCH;
                let (vx, vy) = self.take_v120(-dx * scale, -dy * scale);
                eis.scroll_discrete(vx, vy)
            }
            (ScrollUnit::Pixel, _) => eis.scroll(-dx, -dy),
        }
    }

    fn take_v120(&self, vx: f64, vy: f64) -> (i32, i32) {
        let mut acc = self.scroll_acc.lock().unwrap_or_else(|e| e.into_inner());
        take_whole_pair(&mut acc, vx, vy)
    }
}

fn take_whole_pair(acc: &mut (f64, f64), vx: f64, vy: f64) -> (i32, i32) {
    fn step(slot: &mut f64, delta: f64) -> i32 {
        if delta == 0.0 || !delta.is_finite() {
            return 0;
        }
        if (*slot > 0.0 && delta < 0.0) || (*slot < 0.0 && delta > 0.0) {
            *slot = 0.0;
        }
        *slot += delta;
        let whole = slot.trunc();
        *slot -= whole;
        whole as i32
    }
    (step(&mut acc.0, vx), step(&mut acc.1, vy))
}

impl InputBackend for LinuxInput {
    fn start(&self, _handler: EventHandler) -> Result<(), InputError> {
        Err(InputError::Platform("Linux 使用 start_loop，不支持独立 start".into()))
    }

    fn start_loop(&self, owner: Box<dyn capture_loop::CaptureLoop>) -> Result<(), InputError> {
        let mut capture = self.capture.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = capture.as_ref() {
            if existing.running() {
                return Err(InputError::Platform("截获已启动".into()));
            }
            // The loop exited by itself and consumed the session's single EIS fd.
            *capture = None;
            // Dropping PortalSessions unbinds the clipboard slot.
            *self.portals.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
        let portals = self.ensure_portals()?;
        let eis = portals.take_capture_eis()?;
        let handle = capture::CaptureHandle::start(
            Arc::clone(&portals),
            eis,
            Arc::clone(&self.mode),
            Arc::clone(&self.cursor),
            owner,
        )?;
        *capture = Some(handle);
        Ok(())
    }

    fn stop(&self) {
        if let Some(mut handle) = self.capture.lock().unwrap_or_else(|e| e.into_inner()).take() {
            handle.stop();
        }
        *self.mode.lock().unwrap_or_else(|e| e.into_inner()) = CaptureMode::Local;
        *self.scroll_acc.lock().unwrap_or_else(|e| e.into_inner()) = (0., 0.);
        capture_loop::clear();
        // Drop portal sessions so the next start re-negotiates consent cleanly.
        *self.portals.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.granted.store(false, Ordering::Release);
    }

    fn capture_active(&self) -> Option<bool> {
        Some(
            self.capture
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .is_some_and(capture::CaptureHandle::running),
        )
    }

    fn set_mode(&self, mode: CaptureMode) -> Result<(), InputError> {
        let remote = matches!(mode, CaptureMode::Remote { .. });
        *self.mode.lock().unwrap_or_else(|e| e.into_inner()) = mode;
        // Leaving with the local mouse already activated InputCapture, and KWin
        // hides the pointer for that activation. Leaving with a pointer that was
        // injected from another machine does not: RemoteDesktop motion is absolute,
        // so the barrier spy never tripped and the real cursor stayed on screen.
        if remote
            && let Ok(portals) = self.portals()
            && !portals.captured()
        {
            let at = *self.cursor.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(error) = portals.arm_capture(at) {
                tracing::debug!(%error, "Linux cursor stayed visible after focus left");
            }
        }
        Ok(())
    }

    fn restore_local(&self, at: Point) -> Result<(), InputError> {
        *self.mode.lock().unwrap_or_else(|e| e.into_inner()) = CaptureMode::Local;
        *self.cursor.lock().unwrap_or_else(|e| e.into_inner()) = at;
        if let Ok(portals) = self.portals() {
            portals.release_at(at)?;
        }
        Ok(())
    }

    fn inject(&self, event: &InjectEvent) -> Result<(), InputError> {
        let portals = self.portals()?;
        let eis = portals.injector();
        match event {
            InjectEvent::MoveTo { pos } | InjectEvent::MoveToWithDelta { pos, .. } => {
                eis.motion_absolute(pos.x, pos.y)?;
                *self.cursor.lock().unwrap_or_else(|e| e.into_inner()) = *pos;
                Ok(())
            }
            InjectEvent::Button { button, pressed } => {
                let code = match button {
                    MouseButton::Left => 0x110,
                    MouseButton::Right => 0x111,
                    MouseButton::Middle => 0x112,
                    MouseButton::Back => 0x113,
                    MouseButton::Forward => 0x114,
                    MouseButton::Other(_) => {
                        tracing::debug!(?button, "unmapped extra mouse button ignored on Linux");
                        return Ok(());
                    }
                };
                eis.button(code, *pressed)
            }
            InjectEvent::Key { code, pressed } | InjectEvent::KeyWithoutLock { code, pressed } => {
                let Some(evdev) = keymap::hid_to_evdev(*code) else {
                    tracing::debug!(?code, "unmapped HID key ignored on Linux");
                    return Ok(());
                };
                eis.key(evdev, *pressed)
            }
            InjectEvent::Volume {
                code,
                pressed,
                modifiers,
                ..
            } => {
                let Some(evdev) = keymap::hid_to_evdev(*code) else {
                    return Ok(());
                };
                let mods: Vec<u32> = (0..8)
                    .filter(|bit| modifiers & (1 << bit) != 0)
                    .filter_map(|bit| keymap::hid_to_evdev(KeyCode::keyboard(0xE0 + bit)))
                    .collect();
                for &m in &mods {
                    eis.key(m, true)?;
                }
                let res = eis.key(evdev, *pressed);
                for &m in mods.iter().rev() {
                    let _ = eis.key(m, false);
                }
                res
            }
            InjectEvent::Scroll { dx, dy, unit, phase } => self.inject_scroll(eis, *dx, *dy, *unit, *phase),
            InjectEvent::TouchpadScroll { wheel, .. } => {
                self.inject_scroll(eis, wheel.dx, wheel.dy, wheel.unit, wheel.phase)
            }
            InjectEvent::MoveRelative { dx, dy } => {
                eis.motion_relative(*dx, *dy)?;
                let mut cursor = self.cursor.lock().unwrap_or_else(|e| e.into_inner());
                cursor.x += dx;
                cursor.y += dy;
                Ok(())
            }
            InjectEvent::Gesture(_) => {
                tracing::debug!("touchpad gesture ignored on Linux");
                Ok(())
            }
            InjectEvent::TouchpadContacts(_) => Err(InputError::Unsupported),
        }
    }

    fn cursor_position(&self) -> Result<Point, InputError> {
        Ok(*self.cursor.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn feel_profile(&self) -> FeelProfile {
        FeelProfile {
            wheel_lines_per_notch: Some(WHEEL_LINES_PER_NOTCH),
            ..FeelProfile::default()
        }
    }

    fn permissions(&self) -> InputPermissions {
        let state = if self.granted.load(Ordering::Acquire) {
            PermissionState::Granted
        } else if self.denied.load(Ordering::Acquire) {
            PermissionState::Denied
        } else {
            PermissionState::NotDetermined
        };
        InputPermissions {
            accessibility: state,
            input_monitoring: state,
            secure_input_active: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wheel_accumulator_preserves_fractional_steps_and_resets_on_reversal() {
        let mut acc = (0.0, 0.0);
        assert_eq!(take_whole_pair(&mut acc, 0.0, 60.0), (0, 60));
        assert_eq!(take_whole_pair(&mut acc, 0.0, 0.6), (0, 0));
        assert_eq!(take_whole_pair(&mut acc, 0.0, 0.6), (0, 1));
        // Reversing direction clears residual remainder.
        assert_eq!(take_whole_pair(&mut acc, 0.0, -0.5), (0, 0));
        assert_eq!(acc.1, -0.5);
    }
}
