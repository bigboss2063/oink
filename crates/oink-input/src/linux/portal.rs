//! xdg-desktop-portal InputCapture + RemoteDesktop sessions (KDE Plasma 6.1+).

use std::num::NonZeroU32;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ashpd::WindowIdentifier;
use ashpd::desktop::clipboard::{Clipboard, RequestClipboardOptions};
use ashpd::desktop::input_capture::{
    Barrier, Capabilities, ConnectToEISOptions as IcEisOptions, CreateSessionOptions as IcCreate, EnableOptions,
    GetZonesOptions, InputCapture, Region, ReleaseOptions, SetPointerBarriersOptions,
};
use ashpd::desktop::remote_desktop::{
    ConnectToEISOptions as RdEisOptions, DeviceType, RemoteDesktop, SelectDevicesOptions, StartOptions,
};
use ashpd::desktop::{CreateSessionOptions as RdCreate, PersistMode, Session};
use oink_foundation::{Point, Rect};

use crate::InputError;

use super::clipboard_hooks::ClipboardPortalHooks;
use super::eis_inject::Injector;
use super::runtime;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BarrierSide {
    Left,
    Right,
    Top,
    Bottom,
}

/// One exposed outer-perimeter barrier segment in portal coordinates.
///
/// Portal coordinates place `Right` at `x + w` and `Bottom` at `y + h`, while
/// KWin's `xdg-desktop-portal-kde` subtracts 1 so the compositor tests against
/// the inclusive last logical pixel (`x + w - 1` / `y + h - 1`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OuterBarrier {
    pub side: BarrierSide,
    pub line: i32,
    pub start: i32,
    pub end: i32,
}

impl OuterBarrier {
    fn to_portal_barrier(self, id: NonZeroU32) -> Barrier {
        let pos = match self.side {
            BarrierSide::Left | BarrierSide::Right => (self.line, self.start, self.line, self.end),
            BarrierSide::Top | BarrierSide::Bottom => (self.start, self.line, self.end, self.line),
        };
        Barrier::new(id, pos)
    }

    /// Inclusive compositor coordinate of this barrier line and its outward unit vector.
    fn kwin_line_and_outward(self) -> (f64, f64, f64) {
        match self.side {
            BarrierSide::Left => (f64::from(self.line), -1.0, 0.0),
            BarrierSide::Right => (f64::from(self.line - 1), 1.0, 0.0),
            BarrierSide::Top => (f64::from(self.line), 0.0, -1.0),
            BarrierSide::Bottom => (f64::from(self.line - 1), 0.0, 1.0),
        }
    }
}

#[derive(Clone, Debug)]
struct PortalGeometry {
    zones: Vec<Rect>,
    barriers: Vec<OuterBarrier>,
}

/// Live portal sessions required for capture and injection.
pub(crate) struct PortalSessions {
    pub input_capture: InputCapture,
    pub ic_session: Session<InputCapture>,
    /// Owns the session behind the injection EIS connection.
    _remote_desktop: RemoteDesktop,
    /// Kept alive for EIS inject; clipboard attaches through [`ClipboardPortalHooks`].
    _rd_session: Arc<Session<RemoteDesktop>>,
    clipboard: Option<Arc<ClipboardPortalHooks>>,
    /// EIS fd from InputCapture (receiver); taken once by the capture thread.
    capture_eis: Mutex<Option<OwnedFd>>,
    activation_id: Mutex<Option<u32>>,
    geometry: Mutex<PortalGeometry>,
    /// KWin hides the pointer only while an InputCapture activation is current.
    captured: AtomicBool,
    /// Non-zero while an `arm_capture` awaits its `Activated`. Only
    /// [`Self::take_arm_pending`] clears it — release/restore must not, or a late
    /// `Activated` looks like a real edge and can bounce focus away again.
    pending_arm: AtomicU64,
    /// Monotonic ticket source for [`Self::pending_arm`].
    arm_ticket: AtomicU64,
    injector: Injector,
}

struct Opened {
    input_capture: InputCapture,
    ic_session: Session<InputCapture>,
    remote_desktop: RemoteDesktop,
    rd_session: Arc<Session<RemoteDesktop>>,
    clipboard: Option<Clipboard>,
    capture_eis: OwnedFd,
    geometry: PortalGeometry,
}

impl PortalSessions {
    pub(crate) fn open(clipboard: Option<Arc<ClipboardPortalHooks>>) -> Result<Arc<Self>, InputError> {
        let (opened, inject_eis) = runtime::block_on(async { Self::open_async().await })?;
        let injector = Injector::start(inject_eis)?;
        let Opened {
            input_capture,
            ic_session,
            remote_desktop,
            rd_session,
            clipboard: clipboard_proxy,
            capture_eis,
            geometry,
        } = opened;
        if let Some(hooks) = clipboard.as_ref() {
            if let Some(proxy) = clipboard_proxy {
                (hooks.bind)(proxy, Arc::clone(&rd_session));
            } else {
                (hooks.unbind)();
                tracing::warn!("Clipboard portal 不可用（需要 Plasma 6.4+ 且 RequestClipboard 成功）");
            }
        }
        Ok(Arc::new(Self {
            input_capture,
            ic_session,
            _remote_desktop: remote_desktop,
            _rd_session: rd_session,
            clipboard,
            capture_eis: Mutex::new(Some(capture_eis)),
            activation_id: Mutex::new(None),
            geometry: Mutex::new(geometry),
            captured: AtomicBool::new(false),
            pending_arm: AtomicU64::new(0),
            arm_ticket: AtomicU64::new(0),
            injector,
        }))
    }

    async fn open_async() -> Result<(Opened, OwnedFd), InputError> {
        let input_capture = InputCapture::new()
            .await
            .map_err(|e| InputError::Platform(format!("InputCapture portal：{e}")))?;
        let (ic_session, _caps) = input_capture
            .create_session(
                None,
                IcCreate::default()
                    .set_capabilities(Capabilities::KEYBOARD | Capabilities::POINTER),
            )
            .await
            .map_err(|e| InputError::Platform(format!("创建 InputCapture 会话失败：{e}")))?;

        let capture_eis = input_capture
            .connect_to_eis(&ic_session, IcEisOptions::default())
            .await
            .map_err(|e| InputError::Platform(format!("InputCapture ConnectToEIS 失败：{e}")))?;

        let geometry = configure_zones_and_barriers(&input_capture, &ic_session).await?;

        let remote_desktop = RemoteDesktop::new()
            .await
            .map_err(|e| InputError::Platform(format!("RemoteDesktop portal：{e}")))?;
        let rd_session = remote_desktop
            .create_session(RdCreate::default())
            .await
            .map_err(|e| InputError::Platform(format!("创建 RemoteDesktop 会话失败：{e}")))?;
        remote_desktop
            .select_devices(
                &rd_session,
                SelectDevicesOptions::default()
                    .set_devices(DeviceType::KEYBOARD | DeviceType::POINTER)
                    .set_persist_mode(PersistMode::ExplicitlyRevoked),
            )
            .await
            .map_err(|e| InputError::Platform(format!("选择 RemoteDesktop 设备失败：{e}")))?;
        // Soft-fail: older portals (pre-Plasma 6.4) may lack Clipboard; input still works.
        let clipboard_proxy = match Clipboard::new().await {
            Ok(proxy) => match proxy.request(&rd_session, RequestClipboardOptions::default()).await {
                Ok(()) => Some(proxy),
                Err(error) => {
                    tracing::warn!(%error, "RequestClipboard 失败；剪贴板 portal 不可用");
                    None
                }
            },
            Err(error) => {
                tracing::warn!(%error, "Clipboard portal 不可用");
                None
            }
        };
        let selected = remote_desktop
            .start(&rd_session, Option::<&WindowIdentifier>::None, StartOptions::default())
            .await
            .map_err(|e| InputError::Platform(format!("启动 RemoteDesktop 失败（用户是否拒绝？）：{e}")))?
            .response()
            .map_err(|e| InputError::Platform(format!("RemoteDesktop 启动响应失败：{e}")))?;
        let devices = selected.devices();
        if !devices.contains(DeviceType::KEYBOARD) || !devices.contains(DeviceType::POINTER) {
            return Err(InputError::Platform(format!(
                "RemoteDesktop 未授予完整键鼠设备：{devices:?}"
            )));
        }
        // RequestClipboard success is enough: some portals omit clipboard_enabled.
        if clipboard_proxy.is_some() && !selected.is_clipboard_enabled() {
            tracing::debug!("Start 未报告 clipboard_enabled；仍按 RequestClipboard 成功绑定");
        }
        let clipboard = clipboard_proxy;
        let rd_session = Arc::new(rd_session);
        // Inject over EIS rather than Notify*: KDE drops NotifyPointerMotionAbsolute
        // without a ScreenCast stream, and Notify* is rejected once EIS is connected.
        let inject_eis = remote_desktop
            .connect_to_eis(rd_session.as_ref(), RdEisOptions::default())
            .await
            .map_err(|e| InputError::Platform(format!("RemoteDesktop ConnectToEIS 失败：{e}")))?;
        Ok((
            Opened {
                input_capture,
                ic_session,
                remote_desktop,
                rd_session,
                clipboard,
                capture_eis,
                geometry,
            },
            inject_eis,
        ))
    }

    pub(crate) async fn refresh_zones(&self) -> Result<(), InputError> {
        let geometry = configure_zones_and_barriers(&self.input_capture, &self.ic_session).await?;
        *self.geometry.lock().unwrap_or_else(|e| e.into_inner()) = geometry;
        Ok(())
    }

    pub(crate) fn take_capture_eis(&self) -> Result<OwnedFd, InputError> {
        self.capture_eis
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .ok_or_else(|| InputError::Platform("InputCapture EIS fd 已被取走".into()))
    }

    pub(crate) fn set_activation(&self, id: Option<u32>) {
        *self.activation_id.lock().unwrap_or_else(|e| e.into_inner()) = id;
        self.captured.store(id.is_some(), Ordering::Release);
    }

    /// Drop a stale deactivation. A newer `Activated` may already own the session.
    pub(crate) fn clear_activation(&self, id: Option<u32>) {
        let mut slot = self.activation_id.lock().unwrap_or_else(|e| e.into_inner());
        if id.is_none() || *slot == id {
            *slot = None;
            self.captured.store(false, Ordering::Release);
        }
    }

    pub(crate) fn captured(&self) -> bool {
        self.captured.load(Ordering::Acquire)
    }

    pub(crate) fn take_arm_pending(&self) -> bool {
        self.pending_arm.swap(0, Ordering::AcqRel) != 0
    }

    pub(crate) fn clamp_to_zones(&self, at: Point) -> Point {
        let geom = self.geometry.lock().unwrap_or_else(|e| e.into_inner());
        clamp_point_to_zones(&geom.zones, at)
    }

    pub(crate) fn outward_step_at(&self, at: Point) -> (f64, f64) {
        let geom = self.geometry.lock().unwrap_or_else(|e| e.into_inner());
        barrier_seat(&geom.barriers, at).map_or((0.0, 0.0), |(_, dx, dy)| (dx, dy))
    }

    pub(crate) async fn release_id_async(&self, activation_id: Option<u32>, at: Point) -> Result<(), InputError> {
        let clamped = self.clamp_to_zones(at);
        self.input_capture
            .release(
                &self.ic_session,
                ReleaseOptions::default()
                    .set_activation_id(activation_id)
                    .set_cursor_position((clamped.x, clamped.y)),
            )
            .await
            .map_err(|e| InputError::Platform(format!("InputCapture release 失败：{e}")))?;
        // Only forget the activation after KWin accepted Release. Clearing on
        // failure would leave oink thinking capture is off while the compositor
        // still holds it.
        self.clear_activation(activation_id);
        Ok(())
    }

    pub(crate) async fn release_current_async(&self, at: Point) -> Result<(), InputError> {
        let activation_id = *self.activation_id.lock().unwrap_or_else(|e| e.into_inner());
        if activation_id.is_some() {
            self.release_id_async(activation_id, at).await?;
        }
        Ok(())
    }

    pub(crate) fn release_at(&self, at: Point) -> Result<(), InputError> {
        let activation_id = *self.activation_id.lock().unwrap_or_else(|e| e.into_inner());
        if activation_id.is_some() {
            runtime::block_on(self.release_id_async(activation_id, at))?;
        } else {
            // When InputCapture is not currently active, KWin's Release is a no-op;
            // warp via RemoteDesktop EIS so initial remote entry still starts at `at`.
            self.injector.motion_absolute(at.x, at.y)?;
        }
        Ok(())
    }

    /// Hide the pointer by re-entering InputCapture.
    ///
    /// KWin calls `hideCursor` only inside an active capture. Absolute EIS motion
    /// (how a remote pointer moves on this machine) sets `lastPosition` in KWin's
    /// barrier spy with zero delta, then a single outward relative motion against
    /// the inclusive edge coordinate (`x + w - 1` / `y + h - 1` on right/bottom)
    /// trips the barrier without echoing a second motion into the capture stream.
    pub(crate) fn arm_capture(&self, at: Point) -> Result<(), InputError> {
        let seat = {
            let geom = self.geometry.lock().unwrap_or_else(|e| e.into_inner());
            barrier_seat(&geom.barriers, at)
        };
        let Some((seat, dx, dy)) = seat else {
            return Ok(());
        };
        let ticket = self.arm_ticket.fetch_add(1, Ordering::AcqRel) + 1;
        self.pending_arm.store(ticket, Ordering::Release);
        if let Err(err) = self
            .injector
            .motion_absolute(seat.x, seat.y)
            .and_then(|()| self.injector.motion_relative(dx, dy))
        {
            let _ = self
                .pending_arm
                .compare_exchange(ticket, 0, Ordering::AcqRel, Ordering::Acquire);
            return Err(err);
        }
        Ok(())
    }

    pub(crate) fn injector(&self) -> &Injector {
        &self.injector
    }
}

impl Drop for PortalSessions {
    fn drop(&mut self) {
        if let Some(hooks) = self.clipboard.as_ref() {
            (hooks.unbind)();
        }
    }
}

async fn configure_zones_and_barriers(
    input_capture: &InputCapture,
    ic_session: &Session<InputCapture>,
) -> Result<PortalGeometry, InputError> {
    let zones = input_capture
        .zones(ic_session, GetZonesOptions::default())
        .await
        .map_err(|e| InputError::Platform(format!("读取 InputCapture zones 失败：{e}")))?
        .response()
        .map_err(|e| InputError::Platform(format!("InputCapture zones 响应失败：{e}")))?;

    let zone_rects = parse_zone_rects(zones.regions());
    let outer = outer_edge_barriers(&zone_rects);
    if outer.is_empty() {
        return Err(InputError::Platform(
            "InputCapture zones 为空，无法设置切边 barrier".into(),
        ));
    }
    let portal_barriers: Vec<Barrier> = outer
        .iter()
        .enumerate()
        .map(|(idx, b)| b.to_portal_barrier(NonZeroU32::new(idx as u32 + 1).expect("barrier id")))
        .collect();
    let barrier_resp = input_capture
        .set_pointer_barriers(
            ic_session,
            &portal_barriers,
            zones.zone_set(),
            SetPointerBarriersOptions::default(),
        )
        .await
        .map_err(|e| InputError::Platform(format!("设置 pointer barriers 失败：{e}")))?
        .response()
        .map_err(|e| InputError::Platform(format!("pointer barriers 响应失败：{e}")))?;
    if !barrier_resp.failed_barriers().is_empty() {
        return Err(InputError::Platform(format!(
            "pointer barriers 被合成器拒绝：{:?}",
            barrier_resp.failed_barriers()
        )));
    }
    input_capture
        .enable(ic_session, EnableOptions::default())
        .await
        .map_err(|e| InputError::Platform(format!("启用 InputCapture 失败：{e}")))?;

    Ok(PortalGeometry {
        zones: zone_rects
            .into_iter()
            .map(|(x, y, w, h)| Rect::new(f64::from(x), f64::from(y), f64::from(w), f64::from(h)))
            .collect(),
        barriers: outer,
    })
}

fn parse_zone_rects(regions: &[Region]) -> Vec<(i32, i32, i32, i32)> {
    regions
        .iter()
        .filter_map(|r| {
            let w = i32::try_from(r.width()).ok().filter(|&w| w > 0)?;
            let h = i32::try_from(r.height()).ok().filter(|&h| h > 0)?;
            Some((r.x_offset(), r.y_offset(), w, h))
        })
        .collect()
}

/// Clamp a point into KWin's inclusive desktop coordinates `[x, x + w - 1] × [y, y + h - 1]`.
pub(crate) fn clamp_point_to_zones(zones: &[Rect], at: Point) -> Point {
    if !(at.x.is_finite() && at.y.is_finite()) || zones.is_empty() {
        return at;
    }
    let best = zones
        .iter()
        .copied()
        .min_by(|a, b| inclusive_dist_sq(*a, at).total_cmp(&inclusive_dist_sq(*b, at)))
        .expect("non-empty zones");
    let max_x = (best.right() - 1.0).max(best.x);
    let max_y = (best.bottom() - 1.0).max(best.y);
    Point::new(at.x.clamp(best.x, max_x), at.y.clamp(best.y, max_y))
}

fn inclusive_dist_sq(zone: Rect, p: Point) -> f64 {
    let max_x = (zone.right() - 1.0).max(zone.x);
    let max_y = (zone.bottom() - 1.0).max(zone.y);
    let dx = if p.x < zone.x {
        zone.x - p.x
    } else if p.x > max_x {
        p.x - max_x
    } else {
        0.0
    };
    let dy = if p.y < zone.y {
        zone.y - p.y
    } else if p.y > max_y {
        p.y - max_y
    } else {
        0.0
    };
    dx * dx + dy * dy
}

/// Seat coordinate on the nearest installed outer barrier (in KWin's inclusive
/// logical coordinates) and one outward logical pixel step.
///
/// Keeps an 8px margin away from segment corners when the segment is wide enough,
/// avoiding KDE Plasma's screen-corner hotkeys (such as top-left Overview).
pub(crate) fn barrier_seat(barriers: &[OuterBarrier], at: Point) -> Option<(Point, f64, f64)> {
    if barriers.is_empty() || !(at.x.is_finite() && at.y.is_finite()) {
        return None;
    }
    let best = barriers
        .iter()
        .copied()
        .filter(|b| b.start <= b.end)
        .min_by(|a, b| segment_dist_sq(*a, at).total_cmp(&segment_dist_sq(*b, at)))?;
    let (line, dx, dy) = best.kwin_line_and_outward();
    let raw_along = match best.side {
        BarrierSide::Left | BarrierSide::Right => at.y,
        BarrierSide::Top | BarrierSide::Bottom => at.x,
    };
    let along = clamp_along_avoiding_corners(raw_along, best.start, best.end);
    let seat = match best.side {
        BarrierSide::Left | BarrierSide::Right => Point::new(line, along),
        BarrierSide::Top | BarrierSide::Bottom => Point::new(along, line),
    };
    Some((seat, dx, dy))
}

fn clamp_along_avoiding_corners(value: f64, start: i32, end: i32) -> f64 {
    const CORNER_MARGIN: i32 = 8;
    let (lo, hi) = if end - start >= CORNER_MARGIN * 2 {
        (start + CORNER_MARGIN, end - CORNER_MARGIN)
    } else {
        (start, end)
    };
    value.round().clamp(f64::from(lo), f64::from(hi))
}

fn segment_dist_sq(barrier: OuterBarrier, at: Point) -> f64 {
    let (line, _, _) = barrier.kwin_line_and_outward();
    let (perp, tang) = match barrier.side {
        BarrierSide::Left | BarrierSide::Right => (at.x - line, at.y),
        BarrierSide::Top | BarrierSide::Bottom => (at.y - line, at.x),
    };
    let s = f64::from(barrier.start);
    let e = f64::from(barrier.end);
    let dt = if tang < s {
        s - tang
    } else if tang > e {
        tang - e
    } else {
        0.0
    };
    perp * perp + dt * dt
}

/// Subtract half-open intervals `cut` from `[start, end)`.
fn subtract_intervals(start: i32, end: i32, cuts: impl IntoIterator<Item = (i32, i32)>) -> Vec<(i32, i32)> {
    let mut spans = vec![(start, end)];
    for (cs, ce) in cuts {
        if cs >= ce {
            continue;
        }
        let mut next = Vec::with_capacity(spans.len() + 1);
        for (s, e) in spans {
            if ce <= s || cs >= e {
                next.push((s, e));
            } else {
                if s < cs {
                    next.push((s, cs));
                }
                if ce < e {
                    next.push((ce, e));
                }
            }
        }
        spans = next;
        if spans.is_empty() {
            break;
        }
    }
    spans
}

/// Barriers on every exposed outer perimeter segment of the zone union,
/// excluding all shared internal multi-monitor seams.
fn outer_edge_barriers(regions: &[(i32, i32, i32, i32)]) -> Vec<OuterBarrier> {
    let mut out = Vec::new();
    for (i, &(x, y, w, h)) in regions.iter().enumerate() {
        if w <= 0 || h <= 0 {
            continue;
        }
        // Left edge at x, spanning [y, y + h)
        for (s, e) in subtract_intervals(
            y,
            y + h,
            regions
                .iter()
                .enumerate()
                .filter(|&(j, &(ox, _, ow, _))| j != i && ox + ow == x)
                .map(|(_, &(_, oy, _, oh))| (oy, oy + oh)),
        ) {
            out.push(OuterBarrier {
                side: BarrierSide::Left,
                line: x,
                start: s,
                end: e - 1,
            });
        }
        // Right edge at x + w, spanning [y, y + h)
        for (s, e) in subtract_intervals(
            y,
            y + h,
            regions
                .iter()
                .enumerate()
                .filter(|&(j, &(ox, _, _, _))| j != i && ox == x + w)
                .map(|(_, &(_, oy, _, oh))| (oy, oy + oh)),
        ) {
            out.push(OuterBarrier {
                side: BarrierSide::Right,
                line: x + w,
                start: s,
                end: e - 1,
            });
        }
        // Top edge at y, spanning [x, x + w)
        for (s, e) in subtract_intervals(
            x,
            x + w,
            regions
                .iter()
                .enumerate()
                .filter(|&(j, &(_, oy, _, oh))| j != i && oy + oh == y)
                .map(|(_, &(ox, _, ow, _))| (ox, ox + ow)),
        ) {
            out.push(OuterBarrier {
                side: BarrierSide::Top,
                line: y,
                start: s,
                end: e - 1,
            });
        }
        // Bottom edge at y + h, spanning [x, x + w)
        for (s, e) in subtract_intervals(
            x,
            x + w,
            regions
                .iter()
                .enumerate()
                .filter(|&(j, &(_, oy, _, _))| j != i && oy == y + h)
                .map(|(_, &(ox, _, ow, _))| (ox, ox + ow)),
        ) {
            out.push(OuterBarrier {
                side: BarrierSide::Bottom,
                line: y + h,
                start: s,
                end: e - 1,
            });
        }
    }
    out
}

/// How InputCapture `Activated` is handled after reading the arm ticket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ActivationKind {
    /// Focus is already remote — keep the capture, no fake edge event.
    KeepAsRemote,
    /// Local focus with a pending arm ticket — release without emitting an edge.
    ReleaseSynthetic,
    /// Local focus, no arm ticket — real barrier crossing.
    EmitEdge,
}

pub(crate) fn activation_kind(remote: bool, synthetic: bool) -> ActivationKind {
    if remote {
        ActivationKind::KeepAsRemote
    } else if synthetic {
        ActivationKind::ReleaseSynthetic
    } else {
        ActivationKind::EmitEdge
    }
}

/// Pure ticket ledger mirroring [`PortalSessions`]' arm atomics for tests.
#[derive(Default)]
struct ArmTickets {
    pending: u64,
    next: u64,
}

impl ArmTickets {
    fn begin(&mut self) -> u64 {
        self.next += 1;
        self.pending = self.next;
        self.next
    }

    fn fail(&mut self, ticket: u64) {
        if self.pending == ticket {
            self.pending = 0;
        }
    }

    fn take(&mut self) -> bool {
        let pending = self.pending;
        self.pending = 0;
        pending != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activation_kind_table() {
        assert_eq!(activation_kind(true, true), ActivationKind::KeepAsRemote);
        assert_eq!(activation_kind(true, false), ActivationKind::KeepAsRemote);
        assert_eq!(activation_kind(false, true), ActivationKind::ReleaseSynthetic);
        assert_eq!(activation_kind(false, false), ActivationKind::EmitEdge);
    }

    #[test]
    fn restore_leaves_arm_ticket_for_late_activated() {
        let mut arms = ArmTickets::default();
        arms.begin();
        // release/restore must not clear the ticket — otherwise the late Activated
        // looks like a real edge (EmitEdge) and can bounce focus away again.
        assert_eq!(
            activation_kind(false, arms.take()),
            ActivationKind::ReleaseSynthetic
        );
        assert_eq!(activation_kind(false, arms.take()), ActivationKind::EmitEdge);
    }

    #[test]
    fn failed_arm_clears_only_its_ticket() {
        let mut arms = ArmTickets::default();
        let first = arms.begin();
        let second = arms.begin();
        arms.fail(first);
        assert!(arms.take(), "second arm still pending");
        assert_eq!(second, 2);
        assert!(!arms.take());
    }

    fn laptop_barriers() -> Vec<OuterBarrier> {
        // 3072×1920 panel at 165% → 1862×1164 logical desktop.
        outer_edge_barriers(&[(0, 0, 1862, 1164)])
    }

    #[test]
    fn right_and_bottom_edges_seat_on_kwin_inclusive_last_pixel() {
        let barriers = laptop_barriers();
        let (seat, dx, dy) = barrier_seat(&barriers, Point::new(1862.0, 500.0)).unwrap();
        assert_eq!(seat, Point::new(1861.0, 500.0));
        assert_eq!((dx, dy), (1.0, 0.0));

        let (seat_b, bdx, bdy) = barrier_seat(&barriers, Point::new(600.0, 1164.0)).unwrap();
        assert_eq!(seat_b, Point::new(600.0, 1163.0));
        assert_eq!((bdx, bdy), (0.0, 1.0));
    }

    #[test]
    fn corner_sample_stays_away_from_hot_corners() {
        let barriers = laptop_barriers();
        let (seat, dx, dy) = barrier_seat(&barriers, Point::new(1862.0, 1164.0)).unwrap();
        assert_eq!(seat.x, 1861.0);
        assert!(seat.y <= 1163.0 - 8.0);
        assert_eq!((dx, dy), (1.0, 0.0));

        let (tl, _, _) = barrier_seat(&barriers, Point::new(0.0, 2.0)).unwrap();
        assert_eq!(tl, Point::new(0.0, 8.0));
    }

    #[test]
    fn l_shaped_dual_monitors_skip_internal_seam_and_cover_step_edge() {
        let barriers = outer_edge_barriers(&[(0, 0, 1920, 1080), (1920, 0, 1920, 1440)]);
        // Internal seam at x = 1920 for y in 0..1080 is absent, while the exposed
        // lower step of the second monitor (y in 1080..1440) is present.
        assert!(!barriers.iter().any(|b| b.side == BarrierSide::Right && b.line == 1920));
        assert!(barriers.contains(&OuterBarrier {
            side: BarrierSide::Left,
            line: 1920,
            start: 1080,
            end: 1439,
        }));
        // Monitor 1's bottom at y = 1080 is still covered.
        assert!(barriers.contains(&OuterBarrier {
            side: BarrierSide::Bottom,
            line: 1080,
            start: 0,
            end: 1919,
        }));
        // A cursor near (500, 1079) seats on y = 1079, not the taller monitor's 1439.
        let (seat, dx, dy) = barrier_seat(&barriers, Point::new(500.0, 1079.0)).unwrap();
        assert_eq!(seat, Point::new(500.0, 1079.0));
        assert_eq!((dx, dy), (0.0, 1.0));
    }

    #[test]
    fn non_finite_cursor_is_not_a_barrier_seat() {
        assert!(barrier_seat(&laptop_barriers(), Point::new(f64::NAN, 0.0)).is_none());
    }
}
