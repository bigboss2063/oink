//! One-shot Wayland output snapshot via `wl_output` + `zxdg_output_v1`.

use std::collections::HashMap;
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use oink_foundation::display::DisplayInfo;
use oink_foundation::geom::Rect;
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};
use wayland_protocols_plasma::primary_output::v1::client::kde_primary_output_v1;

use crate::DisplayError;
use crate::geometry::{looks_virtual, reference_extent};
use crate::layout::plausible_reference_extent;

/// Bound waiting for every output's logical geometry. Returning a quiet subset
/// would make dual-monitor Plasma look like a single screen.
pub(super) const OUTPUT_BUDGET: Duration = Duration::from_millis(500);

#[derive(Debug, Clone)]
struct OutputPending {
    name: Option<String>,
    description: Option<String>,
    /// Logical position from `zxdg_output_v1`.
    logical_pos: Option<(i32, i32)>,
    /// Logical size from `zxdg_output_v1`.
    logical_size: Option<(i32, i32)>,
    /// Physical panel dimensions in millimetres from `wl_output.geometry`.
    physical_mm: Option<(i32, i32)>,
    mode_w: Option<i32>,
    mode_h: Option<i32>,
    scale: i32,
    transform: wl_output::Transform,
    done_wl: bool,
    done_xdg: bool,
}

impl Default for OutputPending {
    fn default() -> Self {
        Self {
            name: None,
            description: None,
            logical_pos: None,
            logical_size: None,
            physical_mm: None,
            mode_w: None,
            mode_h: None,
            scale: 1,
            transform: wl_output::Transform::Normal,
            done_wl: false,
            done_xdg: false,
        }
    }
}

struct State {
    /// Keyed by the registry global name passed as Wayland userdata.
    outputs: HashMap<u32, (wl_output::WlOutput, OutputPending)>,
    xdg_manager: Option<(zxdg_output_manager_v1::ZxdgOutputManagerV1, u32)>,
    xdg_by_output: HashMap<u32, zxdg_output_v1::ZxdgOutputV1>,
    /// Connector name / uuid from `kde_primary_output_v1`, when the compositor offers it.
    primary_name: Option<String>,
    _primary: Option<kde_primary_output_v1::KdePrimaryOutputV1>,
    /// Set by dispatch when layout/primary may have changed (for the live watcher).
    dirty: bool,
}

/// Long-lived Wayland output session used by the display watcher.
pub(super) struct LiveSession {
    conn: Connection,
    queue: wayland_client::EventQueue<State>,
    state: State,
    _registry: wl_registry::WlRegistry,
}

pub(crate) fn displays() -> Result<Vec<DisplayInfo>, DisplayError> {
    let (mut session, _) = LiveSession::connect()?;
    session.wait_ready(OUTPUT_BUDGET)?;
    session.displays()
}

impl LiveSession {
    pub(super) fn connect() -> Result<(Self, Vec<DisplayInfo>), DisplayError> {
        let conn =
            Connection::connect_to_env().map_err(|e| DisplayError::Platform(format!("连接 Wayland 显示失败：{e}")))?;
        let display = conn.display();
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        let mut state = State {
            outputs: HashMap::new(),
            xdg_manager: None,
            xdg_by_output: HashMap::new(),
            primary_name: None,
            _primary: None,
            dirty: false,
        };
        let registry = display.get_registry(&qh, ());
        queue
            .roundtrip(&mut state)
            .map_err(|e| DisplayError::Platform(format!("Wayland registry roundtrip 失败：{e}")))?;
        if state.xdg_manager.is_none() {
            return Err(DisplayError::Platform(
                "合成器未提供 zxdg_output_manager_v1（需要 xdg-output）".into(),
            ));
        }
        // Initial globals are bound before the manager exists in rare races; attach now.
        attach_missing_xdg(&mut state, &qh);
        let mut session = Self {
            conn,
            queue,
            state,
            _registry: registry,
        };
        session.wait_ready(OUTPUT_BUDGET)?;
        let initial = session.displays()?;
        session.state.dirty = false;
        Ok((session, initial))
    }

    pub(super) fn displays(&self) -> Result<Vec<DisplayInfo>, DisplayError> {
        collect_displays(&self.state)
    }

    pub(super) fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.state.dirty)
    }

    pub(super) fn all_ready(&self) -> bool {
        let Some((_, xdg_version)) = self.state.xdg_manager else {
            return false;
        };
        !self.state.outputs.is_empty() && self.state.outputs.values().all(|(_, p)| output_ready(p, xdg_version))
    }

    pub(super) fn wait_ready(&mut self, budget: Duration) -> Result<(), DisplayError> {
        let deadline = Instant::now() + budget;
        loop {
            self.queue
                .roundtrip(&mut self.state)
                .map_err(|e| DisplayError::Platform(format!("Wayland output roundtrip 失败：{e}")))?;
            if self.all_ready() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                let xdg_version = self.state.xdg_manager.as_ref().map(|(_, v)| *v);
                let ready = self
                    .state
                    .outputs
                    .values()
                    .filter(|(_, p)| xdg_version.is_some_and(|v| output_ready(p, v)))
                    .count();
                return Err(DisplayError::Platform(format!(
                    "显示器枚举超时：已完成 {ready}/{}",
                    self.state.outputs.len()
                )));
            }
        }
    }

    /// Flush, wait up to `timeout` for Wayland or `wake` readability, then dispatch.
    pub(super) fn wait_and_dispatch(
        &mut self,
        timeout: Duration,
        wake: impl std::os::fd::AsFd,
    ) -> Result<(), DisplayError> {
        use std::os::fd::AsRawFd;

        self.conn
            .flush()
            .map_err(|e| DisplayError::Platform(format!("Wayland flush 失败：{e}")))?;
        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(|e| DisplayError::Platform(format!("Wayland dispatch 失败：{e}")))?;

        let Some(guard) = self.queue.prepare_read() else {
            // Another read is in progress on this connection; drain what we can.
            self.queue
                .dispatch_pending(&mut self.state)
                .map_err(|e| DisplayError::Platform(format!("Wayland dispatch 失败：{e}")))?;
            return Ok(());
        };

        let mut fds = [
            libc::pollfd {
                fd: self.conn.as_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake.as_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let ms = i32::try_from(timeout.as_millis().min(i32::MAX as u128)).unwrap_or(i32::MAX);
        loop {
            let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
            if rc < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(DisplayError::Platform(format!("poll Wayland 失败：{err}")));
            }
            break;
        }

        if fds[0].revents != 0 {
            guard
                .read()
                .map_err(|e| DisplayError::Platform(format!("Wayland read 失败：{e}")))?;
            self.queue
                .dispatch_pending(&mut self.state)
                .map_err(|e| DisplayError::Platform(format!("Wayland dispatch 失败：{e}")))?;
        } else {
            drop(guard);
        }
        Ok(())
    }
}

fn attach_missing_xdg(state: &mut State, qh: &QueueHandle<State>) {
    let Some((manager, _)) = state.xdg_manager.clone() else {
        return;
    };
    let missing: Vec<u32> = state
        .outputs
        .keys()
        .copied()
        .filter(|id| !state.xdg_by_output.contains_key(id))
        .collect();
    for id in missing {
        let Some((output, _)) = state.outputs.get(&id) else {
            continue;
        };
        let xdg = manager.get_xdg_output(output, qh, id);
        state.xdg_by_output.insert(id, xdg);
        state.dirty = true;
    }
}

fn collect_displays(state: &State) -> Result<Vec<DisplayInfo>, DisplayError> {
    let mut out = Vec::with_capacity(state.outputs.len());
    for (id, (_proxy, pending)) in &state.outputs {
        if let Some(info) = describe(*id, pending) {
            out.push(info);
        }
    }
    if out.is_empty() {
        return Err(DisplayError::Platform("未枚举到任何显示器".into()));
    }
    Ok(with_primary(out, state.primary_name.as_deref()))
}

fn output_ready(pending: &OutputPending, xdg_version: u32) -> bool {
    pending.logical_pos.is_some()
        && pending.logical_size.is_some()
        && (pending.done_wl || pending.mode_w.is_some())
        && if xdg_version >= 3 {
            pending.done_wl
        } else {
            pending.done_xdg
        }
}

fn with_primary(mut displays: Vec<DisplayInfo>, primary_name: Option<&str>) -> Vec<DisplayInfo> {
    let matched = primary_name.and_then(|primary| {
        displays.iter().position(|d| {
            d.key.strip_prefix("wl:") == Some(primary)
                || d.key == primary
                || d.name == primary
        })
    });
    let best = matched.or_else(|| {
        // shortcut: no kde_primary_output_v1 (or unmatched uuid) → leftmost bounds.
        displays
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                a.bounds
                    .x
                    .total_cmp(&b.bounds.x)
                    .then(a.bounds.y.total_cmp(&b.bounds.y))
            })
            .map(|(i, _)| i)
    });
    if let Some(best) = best {
        for (i, d) in displays.iter_mut().enumerate() {
            d.is_primary = i == best;
        }
    }
    displays.sort_by(|a, b| {
        b.is_primary
            .cmp(&a.is_primary)
            .then(a.bounds.x.total_cmp(&b.bounds.x))
            .then(a.bounds.y.total_cmp(&b.bounds.y))
    });
    displays
}

fn describe(id: u32, pending: &OutputPending) -> Option<DisplayInfo> {
    let (x, y) = pending.logical_pos?;
    let (w, h) = pending.logical_size?;
    if w <= 0 || h <= 0 {
        return None;
    }
    let bounds = Rect::new(f64::from(x), f64::from(y), f64::from(w), f64::from(h));
    let scale = pending.scale.max(1) as u32;
    let (mut pw, mut ph) = match (pending.mode_w, pending.mode_h) {
        (Some(mw), Some(mh)) if mw > 0 && mh > 0 => (mw as u32, mh as u32),
        _ => (w as u32 * scale, h as u32 * scale),
    };
    let rotation_deg = transform_rotation(pending.transform);
    if matches!(rotation_deg, 90 | 270) && (pw > ph) != (bounds.w > bounds.h) {
        std::mem::swap(&mut pw, &mut ph);
    }

    let name = pending
        .description
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| pending.name.clone().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| format!("Display {id}"));
    let key = pending
        .name
        .clone()
        .filter(|s| !s.is_empty())
        .map(|n| format!("wl:{n}"))
        .unwrap_or_else(|| format!("wl:output-{id}"));
    let is_virtual = looks_virtual(&name);
    let is_builtin = pending
        .name
        .as_deref()
        .map(|n| {
            let n = n.to_ascii_lowercase();
            n.starts_with("edp") || n.contains("edp-") || n.contains("lvds") || n.starts_with("dsi-")
        })
        .unwrap_or(false);

    let candidate_extent = pending
        .physical_mm
        .and_then(|(mm_w, mm_h)| reference_extent(f64::from(mm_w) * 0.001, f64::from(mm_h) * 0.001, &bounds, pw, ph));

    let mut info = DisplayInfo {
        key,
        name,
        bounds,
        pixel_width: pw,
        pixel_height: ph,
        reference_extent: candidate_extent,
        rotation_deg,
        is_primary: false,
        is_builtin,
        is_virtual,
    };
    info.reference_extent = plausible_reference_extent(&info);
    Some(info)
}

fn transform_rotation(t: wl_output::Transform) -> u16 {
    match t {
        wl_output::Transform::Normal | wl_output::Transform::Flipped => 0,
        wl_output::Transform::_90 | wl_output::Transform::Flipped90 => 90,
        wl_output::Transform::_180 | wl_output::Transform::Flipped180 => 180,
        wl_output::Transform::_270 | wl_output::Transform::Flipped270 => 270,
        _ => 0,
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "wl_output" => {
                    let version = version.min(4);
                    let output = registry.bind::<wl_output::WlOutput, _, _>(name, version, qh, name);
                    if let Some((manager, _)) = state.xdg_manager.clone() {
                        let xdg = manager.get_xdg_output(&output, qh, name);
                        state.xdg_by_output.insert(name, xdg);
                    }
                    state.outputs.insert(name, (output, OutputPending::default()));
                    state.dirty = true;
                }
                "zxdg_output_manager_v1" => {
                    let version = version.min(3);
                    let mgr = registry.bind::<zxdg_output_manager_v1::ZxdgOutputManagerV1, _, _>(name, version, qh, ());
                    state.xdg_manager = Some((mgr, version));
                    attach_missing_xdg(state, qh);
                }
                "kde_primary_output_v1" => {
                    let version = version.min(2);
                    let primary =
                        registry.bind::<kde_primary_output_v1::KdePrimaryOutputV1, _, _>(name, version, qh, ());
                    state._primary = Some(primary);
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                if state.outputs.remove(&name).is_some() {
                    state.xdg_by_output.remove(&name);
                    state.dirty = true;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<kde_primary_output_v1::KdePrimaryOutputV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &kde_primary_output_v1::KdePrimaryOutputV1,
        event: kde_primary_output_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let kde_primary_output_v1::Event::PrimaryOutput { output_name } = event {
            state.primary_name = Some(output_name);
            state.dirty = true;
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        registry_name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some((_, pending)) = state.outputs.get_mut(registry_name) else {
            return;
        };
        match event {
            wl_output::Event::Geometry {
                physical_width,
                physical_height,
                transform,
                ..
            } => {
                if physical_width > 0 && physical_height > 0 {
                    pending.physical_mm = Some((physical_width, physical_height));
                }
                if let wayland_client::WEnum::Value(transform) = transform {
                    pending.transform = transform;
                }
                state.dirty = true;
            }
            wl_output::Event::Mode {
                flags, width, height, ..
            } => {
                if let wayland_client::WEnum::Value(flags) = flags
                    && flags.contains(wl_output::Mode::Current)
                {
                    pending.mode_w = Some(width);
                    pending.mode_h = Some(height);
                    pending.done_wl = false;
                    state.dirty = true;
                }
            }
            wl_output::Event::Scale { factor } => {
                pending.scale = factor;
                state.dirty = true;
            }
            wl_output::Event::Name { name } => {
                pending.name = Some(name);
                state.dirty = true;
            }
            wl_output::Event::Description { description } => {
                pending.description = Some(description);
                state.dirty = true;
            }
            wl_output::Event::Done => {
                pending.done_wl = true;
                state.dirty = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<zxdg_output_manager_v1::ZxdgOutputManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &zxdg_output_manager_v1::ZxdgOutputManagerV1,
        _: zxdg_output_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, u32> for State {
    fn event(
        state: &mut Self,
        _: &zxdg_output_v1::ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        output_name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some((_, pending)) = state.outputs.get_mut(output_name) else {
            return;
        };
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                pending.logical_pos = Some((x, y));
                pending.done_xdg = false;
                state.dirty = true;
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                pending.logical_size = Some((width, height));
                pending.done_xdg = false;
                state.dirty = true;
            }
            zxdg_output_v1::Event::Name { name } => {
                if pending.name.as_ref().is_none_or(|s| s.is_empty()) {
                    pending.name = Some(name);
                    state.dirty = true;
                }
            }
            zxdg_output_v1::Event::Description { description } => {
                if pending.description.as_ref().is_none_or(|s| s.is_empty()) {
                    pending.description = Some(description);
                    state.dirty = true;
                }
            }
            zxdg_output_v1::Event::Done => {
                pending.done_xdg = true;
                state.dirty = true;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(key: &str, x: f64, y: f64) -> DisplayInfo {
        DisplayInfo {
            key: key.into(),
            name: key.into(),
            bounds: Rect::new(x, y, 1920.0, 1080.0),
            pixel_width: 1920,
            pixel_height: 1080,
            reference_extent: None,
            rotation_deg: 0,
            is_primary: false,
            is_builtin: false,
            is_virtual: false,
        }
    }

    #[test]
    fn kde_primary_name_wins_over_leftmost() {
        let displays = with_primary(vec![info("wl:eDP-1", 0.0, 0.0), info("wl:DP-1", 1920.0, 0.0)], Some("DP-1"));
        assert!(displays[0].is_primary);
        assert_eq!(displays[0].key, "wl:DP-1");
        assert!(!displays[1].is_primary);
    }

    #[test]
    fn missing_primary_falls_back_to_leftmost() {
        let displays = with_primary(vec![info("wl:DP-1", 1920.0, 0.0), info("wl:eDP-1", 0.0, 0.0)], None);
        assert!(displays[0].is_primary);
        assert_eq!(displays[0].key, "wl:eDP-1");
    }

    #[test]
    fn xdg_v3_ready_requires_wl_done() {
        let pending = OutputPending {
            logical_pos: Some((0, 0)),
            logical_size: Some((1920, 1080)),
            mode_w: Some(1920),
            done_xdg: true,
            done_wl: false,
            ..OutputPending::default()
        };
        assert!(!output_ready(&pending, 3));
        let mut ready = pending.clone();
        ready.done_wl = true;
        assert!(output_ready(&ready, 3));
        assert!(output_ready(&pending, 2));
    }

    #[test]
    fn incomplete_logical_geometry_is_rejected() {
        let mut pending = OutputPending {
            name: Some("eDP-1".into()),
            logical_pos: Some((1862, 0)),
            logical_size: None,
            mode_w: Some(3072),
            mode_h: Some(1920),
            ..OutputPending::default()
        };
        assert!(describe(1, &pending).is_none());
        pending.logical_pos = None;
        pending.logical_size = Some((1862, 1164));
        assert!(describe(1, &pending).is_none());
    }

    #[test]
    fn fractional_scaled_panel_keeps_logical_bounds_and_mode_pixels() {
        let pending = OutputPending {
            name: Some("eDP-1".into()),
            description: Some("Built-in Display".into()),
            logical_pos: Some((0, 0)),
            logical_size: Some((1862, 1164)),
            physical_mm: Some((302, 189)),
            mode_w: Some(3072),
            mode_h: Some(1920),
            scale: 2,
            ..OutputPending::default()
        };
        let info = describe(1, &pending).unwrap();
        assert_eq!(info.bounds, Rect::new(0.0, 0.0, 1862.0, 1164.0));
        assert_eq!((info.pixel_width, info.pixel_height), (3072, 1920));
        assert!(info.is_builtin);
        assert!(info.reference_extent.is_some());
    }
}
