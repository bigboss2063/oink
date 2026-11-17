//! Watch Wayland output layout changes on a long-lived connection.
//!
//! Aligns with macOS / Windows: events wake the observer, a 300 ms debounce
//! merges hotplug jitter, and a 2 s fallback enumeration covers missed wakes.
//! Unlike the old poller, idle ticks reuse one Wayland client — they do not
//! reconnect.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use oink_foundation::display::DisplayInfo;

use crate::{ChangeHandler, DisplayError};

use super::enumerate::{self, LiveSession};

/// A changed layout must stay unchanged for this long before handlers run.
const DEBOUNCE: Duration = Duration::from_millis(300);
/// Callback-free fallback, same cadence as macOS.
const POLL: Duration = Duration::from_secs(2);

pub(crate) struct Watcher {
    handlers: Arc<Mutex<Vec<ChangeHandler>>>,
    wake: Option<UnixStream>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Watcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watcher").finish_non_exhaustive()
    }
}

impl Watcher {
    pub(crate) fn start(first: ChangeHandler) -> Result<Self, DisplayError> {
        let (mut session, initial) = LiveSession::connect()?;
        let handlers = Arc::new(Mutex::new(vec![first]));
        let (wake_w, wake_r) = UnixStream::pair().map_err(|e| DisplayError::Platform(format!("创建唤醒管道失败：{e}")))?;
        wake_r
            .set_nonblocking(true)
            .map_err(|e| DisplayError::Platform(format!("设置唤醒管道失败：{e}")))?;
        wake_w
            .set_nonblocking(true)
            .map_err(|e| DisplayError::Platform(format!("设置唤醒管道失败：{e}")))?;
        let thread_handlers = Arc::clone(&handlers);
        let thread = std::thread::Builder::new()
            .name("oink-display-watch".into())
            .spawn(move || run(wake_r, &thread_handlers, &mut session, &initial))
            .map_err(|e| DisplayError::Platform(format!("启动显示器监听线程失败：{e}")))?;
        Ok(Self {
            handlers,
            wake: Some(wake_w),
            thread: Some(thread),
        })
    }

    pub(crate) fn add_handler(&self, on_change: ChangeHandler) {
        self.handlers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(on_change);
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        if let Some(mut wake) = self.wake.take() {
            let _ = wake.write_all(&[1]);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn fingerprint(displays: &[DisplayInfo]) -> String {
    displays
        .iter()
        .map(|d| {
            format!(
                "{}:{:.0},{:.0},{:.0}x{:.0}:{}x{}:{}:{}",
                d.key,
                d.bounds.x,
                d.bounds.y,
                d.bounds.w,
                d.bounds.h,
                d.pixel_width,
                d.pixel_height,
                d.rotation_deg,
                d.is_primary
            )
        })
        .collect::<Vec<_>>()
        .join("|")
}

fn wake_pending(wake: &UnixStream) -> bool {
    let mut buf = [0u8; 8];
    match Read::read(&mut &*wake, &mut buf) {
        Ok(0) => true, // writer closed
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => false,
        Err(_) => true,
    }
}

fn run(
    wake: UnixStream,
    handlers: &Mutex<Vec<ChangeHandler>>,
    session: &mut LiveSession,
    initial: &[DisplayInfo],
) {
    let mut last = fingerprint(initial);
    let mut dirty_since: Option<Instant> = None;
    let mut last_poll = Instant::now();
    loop {
        if wake_pending(&wake) {
            return;
        }
        if session.take_dirty() {
            dirty_since = Some(Instant::now());
        }
        let settled = dirty_since.is_some_and(|at| at.elapsed() >= DEBOUNCE);
        let poll_due = dirty_since.is_none() && last_poll.elapsed() >= POLL;
        if !(settled || poll_due) {
            let due = dirty_since.map_or(last_poll + POLL, |at| at + DEBOUNCE);
            let wait = due.saturating_duration_since(Instant::now());
            if let Err(error) = session.wait_and_dispatch(wait, &wake) {
                tracing::warn!(%error, "显示器 Wayland 监听失败");
                return;
            }
            continue;
        }
        dirty_since = None;
        last_poll = Instant::now();
        if !session.all_ready() {
            match session.wait_ready(enumerate::OUTPUT_BUDGET) {
                Ok(()) => {}
                Err(error) => {
                    tracing::debug!(%error, "显示器变化后枚举尚未完成，继续等待");
                    dirty_since = Some(Instant::now());
                    continue;
                }
            }
        }
        let Ok(current) = session.displays() else {
            continue;
        };
        let next = fingerprint(&current);
        // Event-driven settles notify even when DisplayInfo is unchanged (refresh /
        // color mode), matching macOS. The 2 s fallback only notifies on change.
        if settled || next != last {
            tracing::debug!("显示器配置变化");
            last = next;
            let handlers = handlers.lock().unwrap_or_else(|e| e.into_inner()).clone();
            for handler in handlers {
                handler();
            }
        }
    }
}
