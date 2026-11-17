//! Wayland display enumeration for KDE Plasma (and other compositors that
//! expose `wl_output` + `zxdg_output_v1`).
//!
//! - `bounds`: compositor logical coordinates (origin top-left of the layout,
//!   y down), matching cursor / InputCapture zones / libei absolute motion.
//! - pixels: `wl_output` mode size (current mode), oriented like `bounds`.
//! - name / key: prefer `zxdg_output.name` / `description`; fall back to a
//!   stable `wl:<name>` or geometry-derived key.

mod enumerate;
mod watch;

use std::sync::Mutex;

use oink_foundation::display::DisplayInfo;

use crate::{ChangeHandler, DisplayBackend, DisplayError};

pub(crate) fn backend() -> Result<Box<dyn DisplayBackend>, DisplayError> {
    // Fail fast if the session is not Wayland — X11 is not supported.
    let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
    if !session.eq_ignore_ascii_case("wayland") && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return Err(DisplayError::Platform(
            "需要 Wayland 会话（XDG_SESSION_TYPE=wayland 或 WAYLAND_DISPLAY）".into(),
        ));
    }
    // Probe once so agent startup fails clearly when the compositor is unreachable.
    let _ = enumerate::displays()?;
    Ok(Box::new(LinuxDisplays {
        watcher: Mutex::new(None),
    }))
}

struct LinuxDisplays {
    watcher: Mutex<Option<watch::Watcher>>,
}

impl DisplayBackend for LinuxDisplays {
    fn displays(&self) -> Result<Vec<DisplayInfo>, DisplayError> {
        enumerate::displays()
    }

    fn watch(&self, on_change: ChangeHandler) -> Result<(), DisplayError> {
        let mut w = self.watcher.lock().unwrap_or_else(|e| e.into_inner());
        match w.as_ref() {
            Some(existing) => existing.add_handler(on_change),
            None => *w = Some(watch::Watcher::start(on_change)?),
        }
        Ok(())
    }
}
