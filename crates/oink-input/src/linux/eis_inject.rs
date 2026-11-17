//! libei sender on the RemoteDesktop EIS connection.
//!
//! KWin only honours absolute pointer motion for a device region; the portal's
//! `NotifyPointerMotionAbsolute` additionally needs a ScreenCast stream and
//! silently drops the event without one. KWin's EIS absolute device covers
//! every output in compositor logical coordinates, matching `oink-display`.

use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use reis::ei;
use reis::event::{DeviceCapability, EiEvent};

use crate::{InputError, source_time};

use super::runtime;

type Devices = Arc<Mutex<Vec<(reis::event::Device, bool)>>>;

pub(crate) struct Injector {
    context: ei::Context,
    connection: reis::event::Connection,
    /// Devices announced by the server and whether we are emulating on them.
    devices: Devices,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Injector {
    pub(crate) fn start(fd: OwnedFd) -> Result<Self, InputError> {
        let devices: Devices = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (ready, connected) = mpsc::channel();
        let thread_devices = Arc::clone(&devices);
        let thread_stop = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("oink-input-eis".into())
            .spawn(move || {
                // The event stream is not Send; keep it on this thread.
                runtime::handle().block_on(run(fd, ready, thread_devices, thread_stop));
            })
            .map_err(|e| InputError::Platform(format!("启动 EIS 注入线程失败：{e}")))?;
        let (context, connection) = connected
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| InputError::Platform("EIS 注入握手超时".into()))??;
        let injector = Self {
            context,
            connection,
            devices,
            stop,
            thread: Some(thread),
        };
        // Devices arrive right after the handshake; give the compositor a moment
        // so the first remote motion or capture-arming step is not rejected.
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline && !(injector.has::<ei::PointerAbsolute>() && injector.has::<ei::Pointer>()) {
            std::thread::sleep(Duration::from_millis(10));
        }
        if !injector.has::<ei::PointerAbsolute>() || !injector.has::<ei::Pointer>() {
            return Err(InputError::Platform(
                "EIS 注入握手完成但未获得绝对/相对指针设备".into(),
            ));
        }
        Ok(injector)
    }

    fn has<T: ei::Interface>(&self) -> bool {
        self.devices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|(device, emulating)| *emulating && device.interface::<T>().is_some())
    }

    /// Send one event on the first emulating device with interface `T`, as its own frame.
    fn emit<T: ei::Interface>(
        &self,
        what: &str,
        send: impl FnOnce(&reis::event::Device, &T),
    ) -> Result<(), InputError> {
        let (device, interface) = self
            .devices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(_, emulating)| *emulating)
            .find_map(|(device, _)| device.interface::<T>().map(|interface| (device.clone(), interface)))
            .ok_or_else(|| InputError::Platform(format!("EIS 没有可用的{what}设备")))?;
        send(&device, &interface);
        device
            .device()
            .frame(self.connection.serial(), source_time::capture_time().0 as u64);
        self.context
            .flush()
            .map_err(|e| InputError::Platform(format!("EIS 发送{what}事件失败：{e}")))
    }

    pub(crate) fn motion_absolute(&self, x: f64, y: f64) -> Result<(), InputError> {
        self.emit::<ei::PointerAbsolute>("绝对指针", |device, p| {
            let (xf, yf) = clamp_to_eis_regions(device.regions(), x, y);
            p.motion_absolute(xf, yf);
        })
    }

    pub(crate) fn motion_relative(&self, dx: f64, dy: f64) -> Result<(), InputError> {
        self.emit::<ei::Pointer>("相对指针", |_, p| p.motion_relative(dx as f32, dy as f32))
    }

    pub(crate) fn button(&self, code: u32, pressed: bool) -> Result<(), InputError> {
        let state = if pressed {
            ei::button::ButtonState::Press
        } else {
            ei::button::ButtonState::Released
        };
        self.emit::<ei::Button>("鼠标按钮", |_, b| b.button(code, state))
    }

    pub(crate) fn key(&self, evdev: u32, pressed: bool) -> Result<(), InputError> {
        let state = if pressed {
            ei::keyboard::KeyState::Press
        } else {
            ei::keyboard::KeyState::Released
        };
        self.emit::<ei::Keyboard>("键盘", |_, k| k.key(evdev, state))
    }

    /// Smooth scroll in logical pixels, positive toward the bottom/right.
    pub(crate) fn scroll(&self, dx: f64, dy: f64) -> Result<(), InputError> {
        if dx == 0.0 && dy == 0.0 {
            return Ok(());
        }
        self.emit::<ei::Scroll>("滚动", |_, s| s.scroll(dx as f32, dy as f32))
    }

    /// Wheel scroll in 1/120 notches, positive toward the bottom/right.
    pub(crate) fn scroll_discrete(&self, dx120: i32, dy120: i32) -> Result<(), InputError> {
        if dx120 == 0 && dy120 == 0 {
            return Ok(());
        }
        self.emit::<ei::Scroll>("滚动", |_, s| s.scroll_discrete(dx120, dy120))
    }
}

/// `libeis` validates `r.x <= x < r.x + r.width` in `f32`. Oink's `Rect::clamp`
/// uses `right - 1e-6` in `f64`, which rounds back up to `right` in `f32` and
/// would otherwise be dropped by `libeis` at the right/bottom edge.
fn clamp_to_eis_regions(regions: &[reis::event::Region], x: f64, y: f64) -> (f32, f32) {
    let valid: Vec<(f64, f64, f64, f64)> = regions
        .iter()
        .filter(|r| r.width > 0 && r.height > 0)
        .map(|r| {
            (
                f64::from(r.x),
                f64::from(r.y),
                f64::from(r.x) + f64::from(r.width),
                f64::from(r.y) + f64::from(r.height),
            )
        })
        .collect();
    if valid.is_empty() {
        return (x as f32, y as f32);
    }
    let best = valid
        .into_iter()
        .min_by(|a, b| rect_distance_sq(*a, x, y).total_cmp(&rect_distance_sq(*b, x, y)))
        .expect("non-empty regions");
    let min_x = best.0 as f32;
    let max_x = (best.2 as f32).next_down().max(min_x);
    let min_y = best.1 as f32;
    let max_y = (best.3 as f32).next_down().max(min_y);
    ((x as f32).clamp(min_x, max_x), (y as f32).clamp(min_y, max_y))
}

fn rect_distance_sq((x0, y0, x1, y1): (f64, f64, f64, f64), x: f64, y: f64) -> f64 {
    let dx = if x < x0 {
        x0 - x
    } else if x > x1 {
        x - x1
    } else {
        0.0
    };
    let dy = if y < y0 {
        y0 - y
    } else if y > y1 {
        y - y1
    } else {
        0.0
    };
    dx * dx + dy * dy
}

impl Drop for Injector {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

async fn run(
    fd: OwnedFd,
    ready: mpsc::Sender<Result<(ei::Context, reis::event::Connection), InputError>>,
    devices: Devices,
    stop: Arc<AtomicBool>,
) {
    let setup = async {
        let stream = UnixStream::from(fd);
        stream
            .set_nonblocking(true)
            .map_err(|e| InputError::Platform(format!("EIS fd nonblocking：{e}")))?;
        let context =
            ei::Context::new(stream).map_err(|e| InputError::Platform(format!("创建 EI context 失败：{e}")))?;
        let _ = context.flush();
        let (connection, events) = context
            .handshake_tokio("oink-input", ei::handshake::ContextType::Sender)
            .await
            .map_err(|e| InputError::Platform(format!("EI handshake 失败：{e}")))?;
        Ok::<_, InputError>((context, connection, events))
    };
    let (context, connection, mut events) = match setup.await {
        Ok(parts) => parts,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    if ready.send(Ok((context.clone(), connection))).is_err() {
        return;
    }
    let mut sequence = 1u32;
    while !stop.load(Ordering::Acquire) {
        let event = tokio::select! {
            event = events.next() => event,
            () = tokio::time::sleep(Duration::from_millis(200)) => continue,
        };
        let event = match event {
            Some(Ok(event)) => event,
            Some(Err(error)) => {
                tracing::warn!(%error, "EIS injection connection failed");
                break;
            }
            None => break,
        };
        let mut devices = devices.lock().unwrap_or_else(|e| e.into_inner());
        match event {
            EiEvent::SeatAdded(seat) => {
                seat.seat.bind_capabilities(
                    DeviceCapability::Pointer
                        | DeviceCapability::PointerAbsolute
                        | DeviceCapability::Keyboard
                        | DeviceCapability::Button
                        | DeviceCapability::Scroll,
                );
            }
            EiEvent::DeviceAdded(added) => devices.push((added.device, false)),
            EiEvent::DeviceResumed(resumed) => {
                resumed.device.device().start_emulating(resumed.serial, sequence);
                sequence = sequence.wrapping_add(1);
                for (device, emulating) in devices.iter_mut() {
                    if *device == resumed.device {
                        *emulating = true;
                    }
                }
            }
            EiEvent::DevicePaused(paused) => {
                for (device, emulating) in devices.iter_mut() {
                    if *device == paused.device {
                        *emulating = false;
                    }
                }
            }
            EiEvent::DeviceRemoved(removed) => devices.retain(|(device, _)| *device != removed.device),
            EiEvent::Disconnected(_) => break,
            _ => {}
        }
        drop(devices);
        let _ = context.flush();
    }
    devices.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(x: u32, y: u32, width: u32, height: u32) -> reis::event::Region {
        reis::event::Region {
            x,
            y,
            width,
            height,
            scale: 1.65,
            mapping_id: None,
        }
    }

    #[test]
    fn f64_right_edge_clamp_stays_strictly_inside_f32_eis_region() {
        let regions = [region(0, 0, 1862, 1164)];
        let (x, y) = clamp_to_eis_regions(&regions, 1862.0 - 1e-6, 1164.0 - 1e-6);
        assert!(x < 1862.0);
        assert!(y < 1164.0);
        // Integer seat coordinates such as 1861.0 remain exact.
        assert_eq!(clamp_to_eis_regions(&regions, 1861.0, 500.0), (1861.0, 500.0));
    }
}
