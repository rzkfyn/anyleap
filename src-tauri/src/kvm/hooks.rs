use super::debug_log::{self, DebugEvent, HookKind, KeyDecision, KeySnapshot};
use super::input_logic::{
    compute_seams, detect_controller_edge, detect_controller_edge_seams, map_controller_return_pos,
    map_leap_relative_y, resolve_remote_origin, union_rect, Edge, PressedInputTracker, Seam,
};
use super::monitors::{get_local_monitors, MonitorDisplay};
use super::protocol::{KvmPacket, Point, ScreenRect, TopologyConfig};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::mpsc;

#[cfg(windows)]
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
#[cfg(windows)]
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
#[cfg(windows)]
use windows::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTONULL};
#[cfg(windows)]
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenElevation,
    TokenIntegrityLevel, TOKEN_ELEVATION, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
#[cfg(windows)]
use windows::Win32::System::SystemInformation::GetTickCount;
#[cfg(windows)]
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId, OpenProcess, OpenProcessToken,
    QueryFullProcessImageNameW, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
};
#[cfg(windows)]
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::*;

#[cfg(windows)]
const WM_KVM_RESTORE_CURSOR: u32 = WM_APP + 1;
#[cfg(windows)]
const WM_KVM_ENGAGE: u32 = WM_APP + 2;
#[cfg(windows)]
const WATCHDOG_INTERVAL_MS: u32 = 250;
#[cfg(any(windows, test))]
const WATCHDOG_BLIND_THRESHOLD_MS: u32 = 1000;
const BLOCKED_NOTICE_COOLDOWN_MS: u64 = 1000;
#[cfg(any(windows, test))]
const TIMER_BLOCKED_NOTICE_COOLDOWN_MS: u64 = 5000;
const SECURITY_MANDATORY_MEDIUM_RID: u32 = 0x2000;
const SECURITY_MANDATORY_HIGH_RID: u32 = 0x3000;

pub type LeapBlockedCallback = Arc<dyn Fn(&str) + Send + Sync>;
type MouseDeltaSender = mpsc::Sender<(i32, i32)>;

static PACKET_SENDER: OnceLock<Mutex<Option<mpsc::Sender<KvmPacket>>>> = OnceLock::new();
static MOUSE_DELTA_SENDER: OnceLock<Mutex<Option<MouseDeltaSender>>> = OnceLock::new();
static LEAP_BLOCKED_CALLBACK: OnceLock<Mutex<Option<LeapBlockedCallback>>> = OnceLock::new();
static LOCAL_MONITORS: OnceLock<Mutex<Vec<MonitorDisplay>>> = OnceLock::new();
static CONTROLLER_LAYOUT: OnceLock<Mutex<ControllerLayout>> = OnceLock::new();
static CONTROLLER_TRACKER: OnceLock<Mutex<PressedInputTracker>> = OnceLock::new();

#[derive(Default)]
struct ControllerLayout {
    main_monitors: Vec<MonitorDisplay>,
    linked_monitors: Vec<MonitorDisplay>,
    remote_origin: Option<Point>,
    seams: Vec<Seam>,
}

static HOOK_ACTIVE: AtomicBool = AtomicBool::new(false);
static IS_TRAPPED: AtomicBool = AtomicBool::new(false);
static ENGAGE_PENDING: AtomicBool = AtomicBool::new(false);
static CURSOR_CENTERED: AtomicBool = AtomicBool::new(false);
static HOOK_THREAD_ID: AtomicU32 = AtomicU32::new(0);

static CLAMP_CENTER_X: AtomicI32 = AtomicI32::new(960);
static CLAMP_CENTER_Y: AtomicI32 = AtomicI32::new(540);
static TRAP_MON_X: AtomicI32 = AtomicI32::new(0);
static TRAP_MON_Y: AtomicI32 = AtomicI32::new(0);
static TRAP_MON_W: AtomicI32 = AtomicI32::new(1920);
static TRAP_MON_H: AtomicI32 = AtomicI32::new(1080);
static PENDING_ENGAGE_REL_Y: AtomicU32 = AtomicU32::new(0);
static PENDING_ENGAGE_ALONG_PX: AtomicI32 = AtomicI32::new(0);
static PENDING_ENGAGE_ENTRY_EDGE: AtomicU8 = AtomicU8::new(Edge::Left as u8);
static PENDING_ENGAGE_LEAP_SIDE: AtomicU8 = AtomicU8::new(Edge::Right as u8);
static PENDING_ENGAGE_HAS_SEAM: AtomicBool = AtomicBool::new(false);
static PENDING_ENGAGE_TARGET_MON: OnceLock<Mutex<String>> = OnceLock::new();

static LINK_FROM_EDGE: AtomicU8 = AtomicU8::new(Edge::Right as u8);
static LINK_TO_EDGE: AtomicU8 = AtomicU8::new(Edge::Left as u8);
static HAS_ACTIVE_LINK: AtomicBool = AtomicBool::new(false);
static DEADZONE_PX: AtomicI32 = AtomicI32::new(48);
static RESISTANCE_MS: AtomicU64 = AtomicU64::new(120);
static LOCAL_UNION_X: AtomicI32 = AtomicI32::new(0);
static LOCAL_UNION_Y: AtomicI32 = AtomicI32::new(0);
static LOCAL_UNION_W: AtomicI32 = AtomicI32::new(1920);
static LOCAL_UNION_H: AtomicI32 = AtomicI32::new(1080);
static PEER_UNION_W: AtomicI32 = AtomicI32::new(0);
static PEER_UNION_H: AtomicI32 = AtomicI32::new(0);
static LINK_OFFSET_X: AtomicI32 = AtomicI32::new(0);
static LINK_OFFSET_Y: AtomicI32 = AtomicI32::new(0);

static MOUSE_BUTTONS_DOWN: AtomicU8 = AtomicU8::new(0);
// --- SPEC-25a ---
static DRAG_MODE_ENGAGE: AtomicBool = AtomicBool::new(false);
static DRAG_MODE_BUTTON: AtomicU8 = AtomicU8::new(1);
pub type DragModeEngagedCallback = Arc<dyn Fn(u8) + Send + Sync>;
static DRAG_MODE_ENGAGED_CB: OnceLock<Mutex<Option<DragModeEngagedCallback>>> = OnceLock::new();

fn drag_mode_engaged_slot() -> &'static Mutex<Option<DragModeEngagedCallback>> {
    DRAG_MODE_ENGAGED_CB.get_or_init(|| Mutex::new(None))
}
// --- end SPEC-25a ---
static KEY_CTRL_DOWN: AtomicBool = AtomicBool::new(false);
static KEY_ALT_DOWN: AtomicBool = AtomicBool::new(false);
static EDGE_DWELL_START: AtomicU64 = AtomicU64::new(0);
static EDGE_DWELL_SEAM_IDX: AtomicI32 = AtomicI32::new(-1);
#[cfg(windows)]
static LAST_BLOCKED_NOTICE_MS: AtomicU64 = AtomicU64::new(0);
#[cfg(windows)]
static LAST_TIMER_BLOCKED_NOTICE_MS: AtomicU64 = AtomicU64::new(0);
#[cfg(windows)]
static LAST_HOOK_TICK_MS: AtomicU32 = AtomicU32::new(0);
#[cfg(windows)]
static HAS_LAST_HOOK_TICK: AtomicBool = AtomicBool::new(false);
#[cfg(windows)]
static LAST_MOUSE_HOOK_US: AtomicU64 = AtomicU64::new(0);
#[cfg(windows)]
static LAST_KBD_HOOK_US: AtomicU64 = AtomicU64::new(0);

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn packet_sender() -> &'static Mutex<Option<mpsc::Sender<KvmPacket>>> {
    PACKET_SENDER.get_or_init(|| Mutex::new(None))
}

fn mouse_delta_sender() -> &'static Mutex<Option<mpsc::Sender<(i32, i32)>>> {
    MOUSE_DELTA_SENDER.get_or_init(|| Mutex::new(None))
}

fn leap_blocked_callback_slot() -> &'static Mutex<Option<LeapBlockedCallback>> {
    LEAP_BLOCKED_CALLBACK.get_or_init(|| Mutex::new(None))
}

fn local_monitors() -> &'static Mutex<Vec<MonitorDisplay>> {
    LOCAL_MONITORS.get_or_init(|| Mutex::new(Vec::new()))
}

fn controller_layout() -> &'static Mutex<ControllerLayout> {
    CONTROLLER_LAYOUT.get_or_init(|| Mutex::new(ControllerLayout::default()))
}

fn pending_engage_target_mon() -> &'static Mutex<String> {
    PENDING_ENGAGE_TARGET_MON.get_or_init(|| Mutex::new(String::new()))
}

fn controller_tracker() -> &'static Mutex<PressedInputTracker> {
    CONTROLLER_TRACKER.get_or_init(|| Mutex::new(PressedInputTracker::new()))
}

fn try_send_packet(pkt: KvmPacket) {
    if let Ok(guard) = packet_sender().try_lock() {
        if let Some(ref tx) = *guard {
            let _ = tx.try_send(pkt);
        }
    }
}

/// Reset cursor clip and display counter so cursor never stays hidden.
pub fn restore_cursor_visibility() {
    #[cfg(windows)]
    unsafe {
        let _ = ClipCursor(None);
        for _ in 0..16 {
            if ShowCursor(true) >= 0 {
                break;
            }
        }
    }
    #[cfg(target_os = "macos")]
    macos_tap::restore_cursor_direct();
}

#[cfg(windows)]
fn post_hook_thread_message(msg: u32) {
    unsafe {
        let tid = HOOK_THREAD_ID.load(Ordering::Acquire);
        if tid != 0 {
            let _ = PostThreadMessageW(tid, msg, WPARAM(0), LPARAM(0));
        }
    }
}

fn notify_hook_thread_restore() {
    restore_cursor_visibility();
    #[cfg(windows)]
    post_hook_thread_message(WM_KVM_RESTORE_CURSOR);
    #[cfg(target_os = "macos")]
    macos_tap::signal_restore();
}

// --- SPEC-25a ---
fn drag_leap_edge_tolerance(strip_width_px: i32) -> i32 {
    (strip_width_px + 2).max(4)
}

fn should_ignore_injected_mouse_hook_event(
    flags: u32,
    local_drag_source_active: bool,
    is_trapped: bool,
) -> bool {
    const LLMHF_INJECTED: u32 = 1;
    (flags & LLMHF_INJECTED) != 0 && local_drag_source_active && !is_trapped
}

fn is_physical_mouse_hook_event(flags: u32) -> bool {
    const LLMHF_INJECTED: u32 = 0x0000_0001;
    const LLMHF_LOWER_IL_INJECTED: u32 = 0x0000_0002;
    (flags & (LLMHF_INJECTED | LLMHF_LOWER_IL_INJECTED)) == 0
}

pub fn is_hook_active() -> bool {
    HOOK_ACTIVE.load(Ordering::Acquire)
}

pub fn is_physical_button_down(button: u8) -> bool {
    let bit = match button {
        1 => 1u8,
        2 => 2u8,
        3 => 4u8,
        _ => return false,
    };
    (MOUSE_BUTTONS_DOWN.load(Ordering::Acquire) & bit) != 0
}

fn button_from_mouse_up_msg(msg: u32) -> Option<u8> {
    match msg {
        0x0202 => Some(1), // WM_LBUTTONUP
        0x0205 => Some(2), // WM_RBUTTONUP
        0x0208 => Some(3), // WM_MBUTTONUP
        _ => None,
    }
}
// --- end SPEC-25a ---

#[cfg(any(windows, target_os = "macos", test))]
fn compute_trap_center(mx: i32, my: i32, mw: i32, mh: i32) -> (i32, i32) {
    (mx + mw / 2, my + mh / 2)
}

#[cfg(any(windows, test))]
fn trapped_mouse_delta(
    cursor_centered: bool,
    pt: (i32, i32),
    center: (i32, i32),
) -> Option<(i32, i32)> {
    if !cursor_centered {
        return None;
    }
    let dx = pt.0 - center.0;
    let dy = pt.1 - center.1;
    if dx == 0 && dy == 0 {
        None
    } else {
        Some((dx, dy))
    }
}

#[cfg(any(windows, target_os = "macos", test))]
fn should_perform_engage(hook_active: bool, is_trapped: bool, engage_pending: bool) -> bool {
    hook_active && is_trapped && engage_pending
}

pub struct HookController {
    _running: Arc<AtomicBool>,
}

impl Default for HookController {
    fn default() -> Self {
        Self::new()
    }
}

impl HookController {
    pub fn new() -> Self {
        Self {
            _running: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn is_active(&self) -> bool {
        HOOK_ACTIVE.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn set_active_for_test(&self, active: bool, edge: Edge) {
        HOOK_ACTIVE.store(active, Ordering::SeqCst);
        LINK_FROM_EDGE.store(edge as u8, Ordering::Relaxed);
        HAS_ACTIVE_LINK.store(active, Ordering::Release);
    }

    /// Edge that leaps to the peer, when capture is active with a link.
    pub fn active_link_edge(&self) -> Option<&'static str> {
        (HOOK_ACTIVE.load(Ordering::Relaxed) && HAS_ACTIVE_LINK.load(Ordering::Acquire))
            .then(|| Edge::from_u8(LINK_FROM_EDGE.load(Ordering::Relaxed)).as_str())
    }

    pub fn is_trapped(&self) -> bool {
        IS_TRAPPED.load(Ordering::Acquire)
    }

    pub fn set_trapped(&self, trapped: bool) {
        IS_TRAPPED.store(trapped, Ordering::SeqCst);
        if !trapped {
            CURSOR_CENTERED.store(false, Ordering::Release);
            ENGAGE_PENDING.store(false, Ordering::Release);
            notify_hook_thread_restore();
        }
    }

    pub fn handle_cursor_return(
        &self,
        edge: &str,
        relative_y: f32,
        target_monitor: Option<&str>,
        along_px: Option<i32>,
    ) {
        let was_trapped = IS_TRAPPED.swap(false, Ordering::SeqCst);
        if debug_log::is_enabled() {
            debug_log::record_event(DebugEvent::Return {
                ts_us: debug_log::now_us(),
                relative_y,
                was_trapped,
                hwnd: debug_log::foreground_hwnd_raw(),
            });
        }
        if !was_trapped {
            return;
        }
        CURSOR_CENTERED.store(false, Ordering::Release);
        ENGAGE_PENDING.store(false, Ordering::Release);
        EDGE_DWELL_START.store(0, Ordering::Relaxed);
        EDGE_DWELL_SEAM_IDX.store(-1, Ordering::Relaxed);
        notify_hook_thread_restore();

        let fallback_mon = MonitorDisplay {
            id: String::new(),
            device_name: String::new(),
            x: TRAP_MON_X.load(Ordering::Relaxed),
            y: TRAP_MON_Y.load(Ordering::Relaxed),
            width: TRAP_MON_W.load(Ordering::Relaxed),
            height: TRAP_MON_H.load(Ordering::Relaxed),
            is_primary: true,
        };
        let peer_union = ScreenRect {
            x: 0,
            y: 0,
            width: PEER_UNION_W.load(Ordering::Relaxed),
            height: PEER_UNION_H.load(Ordering::Relaxed),
        };
        let link_edge = Edge::from_u8(LINK_FROM_EDGE.load(Ordering::Relaxed));
        let return_edge = Edge::from_str(edge).unwrap_or_else(|| link_edge.opposite());
        let offset_x = LINK_OFFSET_X.load(Ordering::Relaxed);
        let offset_y = LINK_OFFSET_Y.load(Ordering::Relaxed);
        let (rx, ry) = if let Ok(mons) = local_monitors().try_lock() {
            map_controller_return_pos(
                &mons,
                target_monitor,
                return_edge,
                along_px,
                &fallback_mon,
                &peer_union,
                link_edge,
                offset_x,
                offset_y,
                relative_y,
            )
        } else {
            map_controller_return_pos(
                &[],
                target_monitor,
                return_edge,
                along_px,
                &fallback_mon,
                &peer_union,
                link_edge,
                offset_x,
                offset_y,
                relative_y,
            )
        };
        #[cfg(windows)]
        unsafe {
            let _ = SetCursorPos(rx, ry);
        }
        #[cfg(target_os = "macos")]
        macos_tap::schedule_return_warp(rx, ry);
        #[cfg(not(any(windows, target_os = "macos")))]
        let _ = (rx, ry);
    }

    pub fn emergency_release(&self) {
        if debug_log::is_enabled() {
            debug_log::record_event(DebugEvent::Release {
                ts_us: debug_log::now_us(),
                reason: "emergency_release",
            });
        }
        self.set_trapped(false);
        EDGE_DWELL_START.store(0, Ordering::Relaxed);
        EDGE_DWELL_SEAM_IDX.store(-1, Ordering::Relaxed);
        MOUSE_BUTTONS_DOWN.store(0, Ordering::Relaxed);
        KEY_CTRL_DOWN.store(false, Ordering::Relaxed);
        KEY_ALT_DOWN.store(false, Ordering::Relaxed);
        if let Ok(mut tracker) = controller_tracker().try_lock() {
            let _ = tracker.release_all();
        }
        notify_hook_thread_restore();
        super::dragdrop::cancel_active_target_drag_with_reason("emergency_release");
        super::dragdrop::hide_edge_catcher();

        try_send_packet(KvmPacket::EmergencyRelease);
        try_send_packet(KvmPacket::ReleaseAll);
    }

    // --- SPEC-25a ---
    pub fn set_drag_mode_engaged_callback(&self, cb: Option<DragModeEngagedCallback>) {
        if let Ok(mut g) = drag_mode_engaged_slot().lock() {
            *g = cb;
        }
    }

    /// Pure guard for whether a drag-mode leap may engage.
    pub fn can_engage_drag_leap(
        drag_drop_enabled: bool,
        hook_active: bool,
        is_trapped: bool,
        has_active_link: bool,
        mouse_buttons_down: u8,
    ) -> bool {
        drag_drop_enabled
            && hook_active
            && !is_trapped
            && has_active_link
            && mouse_buttons_down != 0
    }

    /// Engage a "drag mode" leap after the edge-catcher helper thread has
    /// cancelled the local OLE drag with Escape. Never call from a hook callback.
    #[cfg(windows)]
    pub fn engage_drag_leap(&self, pt: (i32, i32), button: u8) -> bool {
        if !Self::can_engage_drag_leap(
            super::dragdrop::drag_drop_enabled(),
            HOOK_ACTIVE.load(Ordering::Acquire),
            IS_TRAPPED.load(Ordering::Acquire),
            HAS_ACTIVE_LINK.load(Ordering::Acquire),
            MOUSE_BUTTONS_DOWN.load(Ordering::Acquire),
        ) {
            return false;
        }

        let link_edge = Edge::from_u8(LINK_FROM_EDGE.load(Ordering::Relaxed));
        let tol = drag_leap_edge_tolerance(2);
        let hit = {
            let Ok(layout) = controller_layout().lock() else {
                return false;
            };
            if let Some(origin) = layout.remote_origin {
                detect_controller_edge_seams(
                    pt,
                    &layout.main_monitors,
                    &layout.linked_monitors,
                    origin,
                    &layout.seams,
                    tol,
                )
                .map(|h| {
                    let m = &layout.main_monitors[h.main_monitor_index];
                    (
                        m.x,
                        m.y,
                        m.width,
                        m.height,
                        h.seam.side,
                        h.seam.side.opposite(),
                        h.relative_y,
                        Some((h.target_monitor, h.along_px)),
                    )
                })
            } else {
                detect_controller_edge(pt, &layout.main_monitors, link_edge, tol).map(|(idx, _)| {
                    let m = &layout.main_monitors[idx];
                    let clamped = (
                        pt.0.clamp(m.x, m.x + m.width - 1),
                        pt.1.clamp(m.y, m.y + m.height - 1),
                    );
                    let local_union = ScreenRect {
                        x: LOCAL_UNION_X.load(Ordering::Relaxed),
                        y: LOCAL_UNION_Y.load(Ordering::Relaxed),
                        width: LOCAL_UNION_W.load(Ordering::Relaxed),
                        height: LOCAL_UNION_H.load(Ordering::Relaxed),
                    };
                    let peer_union = ScreenRect {
                        x: 0,
                        y: 0,
                        width: PEER_UNION_W.load(Ordering::Relaxed),
                        height: PEER_UNION_H.load(Ordering::Relaxed),
                    };
                    let rel = map_leap_relative_y(
                        clamped,
                        &local_union,
                        &peer_union,
                        link_edge,
                        LINK_OFFSET_X.load(Ordering::Relaxed),
                        LINK_OFFSET_Y.load(Ordering::Relaxed),
                    );
                    (
                        m.x,
                        m.y,
                        m.width,
                        m.height,
                        link_edge,
                        Edge::from_u8(LINK_TO_EDGE.load(Ordering::Relaxed)),
                        rel,
                        None,
                    )
                })
            }
        };

        debug_log::note(|| {
            format!(
                "DRAG_LEAP_ENGAGE pt=({},{}) button={} hit={}",
                pt.0,
                pt.1,
                button,
                hit.is_some(),
            )
        });
        let Some((mx, my, mw, mh, leap_side, entry_edge, relative_y, seam_target)) = hit else {
            return false;
        };

        DRAG_MODE_BUTTON.store(button, Ordering::Release);
        DRAG_MODE_ENGAGE.store(true, Ordering::Release);
        request_engage_leap(
            mx,
            my,
            mw,
            mh,
            leap_side,
            entry_edge,
            relative_y,
            seam_target,
        );
        true
    }

    #[cfg(not(windows))]
    pub fn engage_drag_leap(&self, _pt: (i32, i32), _button: u8) -> bool {
        false
    }
    // --- end SPEC-25a ---

    pub fn start_hooks(
        &self,
        topology: TopologyConfig,
        packet_tx: mpsc::Sender<KvmPacket>,
        delta_tx: mpsc::Sender<(i32, i32)>,
        on_leap_blocked: Option<LeapBlockedCallback>,
    ) -> bool {
        let monitors = get_local_monitors();
        let local_union = union_rect(&monitors);
        LOCAL_UNION_X.store(local_union.x, Ordering::Relaxed);
        LOCAL_UNION_Y.store(local_union.y, Ordering::Relaxed);
        LOCAL_UNION_W.store(local_union.width, Ordering::Relaxed);
        LOCAL_UNION_H.store(local_union.height, Ordering::Relaxed);
        if let Ok(mut mons) = local_monitors().lock() {
            *mons = monitors.clone();
        }

        let local_id = topology
            .computers
            .iter()
            .find(|c| c.is_local)
            .map(|c| c.id.as_str());

        let link = local_id
            .and_then(|id| topology.links.iter().find(|l| l.from_id == id))
            .or_else(|| topology.links.first());

        let peer_node = link
            .and_then(|l| topology.computers.iter().find(|c| c.id == l.to_id))
            .or_else(|| topology.computers.iter().find(|c| !c.is_local));

        let linked_monitors: Vec<MonitorDisplay> = peer_node
            .map(|c| {
                if !c.monitors.is_empty() {
                    c.monitors.clone()
                } else if c.bounds.width > 0 && c.bounds.height > 0 {
                    vec![MonitorDisplay {
                        id: "peer-1".into(),
                        device_name: "peer-1".into(),
                        x: c.bounds.x,
                        y: c.bounds.y,
                        width: c.bounds.width,
                        height: c.bounds.height,
                        is_primary: true,
                    }]
                } else {
                    Vec::new()
                }
            })
            .unwrap_or_default();

        let peer_bounds = if !linked_monitors.is_empty() {
            union_rect(&linked_monitors)
        } else {
            peer_node
                .map(|c| c.bounds.clone())
                .unwrap_or_else(|| ScreenRect {
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 0,
                })
        };

        let remote_origin = if !linked_monitors.is_empty() {
            resolve_remote_origin(&topology, &monitors, &linked_monitors, local_id)
        } else {
            topology.remote_origin
        };
        let seams = match remote_origin {
            Some(origin) if !linked_monitors.is_empty() => {
                compute_seams(&monitors, &linked_monitors, origin)
            }
            _ => Vec::new(),
        };

        if let Ok(mut layout) = controller_layout().lock() {
            layout.main_monitors = monitors;
            layout.linked_monitors = linked_monitors;
            layout.remote_origin = remote_origin;
            layout.seams = seams.clone();
        }

        if link.is_some() || !seams.is_empty() {
            let from_edge = link
                .and_then(|l| Edge::from_str(&l.from_edge))
                .or_else(|| seams.first().map(|s| s.side))
                .unwrap_or(Edge::Right);
            let to_edge = link
                .and_then(|l| Edge::from_str(&l.to_edge))
                .unwrap_or_else(|| from_edge.opposite());
            LINK_FROM_EDGE.store(from_edge as u8, Ordering::Relaxed);
            LINK_TO_EDGE.store(to_edge as u8, Ordering::Relaxed);
            DEADZONE_PX.store(
                link.and_then(|l| l.deadzone_px).unwrap_or(48),
                Ordering::Relaxed,
            );
            RESISTANCE_MS.store(
                link.and_then(|l| l.resistance_ms).unwrap_or(120) as u64,
                Ordering::Relaxed,
            );
            LINK_OFFSET_X.store(
                link.and_then(|l| l.offset_x).unwrap_or(0),
                Ordering::Relaxed,
            );
            LINK_OFFSET_Y.store(
                link.and_then(|l| l.offset_y).unwrap_or(0),
                Ordering::Relaxed,
            );
            PEER_UNION_W.store(peer_bounds.width, Ordering::Relaxed);
            PEER_UNION_H.store(peer_bounds.height, Ordering::Relaxed);
            let active = if remote_origin.is_some() {
                !seams.is_empty()
            } else {
                link.is_some()
            };
            HAS_ACTIVE_LINK.store(active, Ordering::Release);
        } else {
            HAS_ACTIVE_LINK.store(false, Ordering::Release);
        }
        EDGE_DWELL_START.store(0, Ordering::Relaxed);
        EDGE_DWELL_SEAM_IDX.store(-1, Ordering::Relaxed);

        if let Ok(mut g) = packet_sender().lock() {
            *g = Some(packet_tx);
        }
        if let Ok(mut g) = mouse_delta_sender().lock() {
            *g = Some(delta_tx);
        }
        if let Ok(mut g) = leap_blocked_callback_slot().lock() {
            *g = on_leap_blocked;
        }

        #[cfg(not(any(windows, target_os = "macos")))]
        {
            // Global input capture hooks are not yet implemented on this OS.
            // Keep HOOK_ACTIVE and HAS_ACTIVE_LINK false so status reflects reality.
            HAS_ACTIVE_LINK.store(false, Ordering::Release);
            false
        }

        #[cfg(target_os = "macos")]
        {
            macos_tap::start()
        }

        #[cfg(windows)]
        {
            if HOOK_ACTIVE.swap(true, Ordering::SeqCst) {
                return true;
            }

            LAST_BLOCKED_NOTICE_MS.store(0, Ordering::Relaxed);
            LAST_TIMER_BLOCKED_NOTICE_MS.store(0, Ordering::Relaxed);
            HAS_LAST_HOOK_TICK.store(false, Ordering::Release);
            LAST_HOOK_TICK_MS.store(0, Ordering::Release);
            LAST_MOUSE_HOOK_US.store(0, Ordering::Relaxed);
            LAST_KBD_HOOK_US.store(0, Ordering::Relaxed);

            std::thread::spawn(move || unsafe {
                let mut msg = MSG::default();
                // Force message queue creation before publishing HOOK_THREAD_ID so
                // PostThreadMessageW from a hook callback never races queue init.
                let _ = PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE);

                let tid = GetCurrentThreadId();
                HOOK_THREAD_ID.store(tid, Ordering::Release);
                let our_pid = GetCurrentProcessId();

                let h_mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(low_level_mouse_proc), None, 0);
                let h_kbd =
                    SetWindowsHookExW(WH_KEYBOARD_LL, Some(low_level_keyboard_proc), None, 0);
                let mut watchdog_timer_id: usize = SetTimer(None, 0, WATCHDOG_INTERVAL_MS, None);

                while HOOK_ACTIVE.load(Ordering::Relaxed) {
                    let res = GetMessageW(&mut msg, None, 0, 0);
                    if res.0 <= 0 {
                        break;
                    }
                    if msg.message == WM_KVM_RESTORE_CURSOR {
                        restore_cursor_visibility();
                        continue;
                    }
                    if msg.message == WM_KVM_ENGAGE {
                        if perform_engage_leap() && watchdog_timer_id == 0 {
                            watchdog_timer_id = SetTimer(None, 0, WATCHDOG_INTERVAL_MS, None);
                        }
                        continue;
                    }
                    if msg.message == WM_TIMER
                        && watchdog_timer_id != 0
                        && msg.wParam.0 == watchdog_timer_id
                    {
                        if IS_TRAPPED.load(Ordering::Acquire) {
                            if let Some(last_input_tick) = query_last_input_tick() {
                                let hook_tick = HAS_LAST_HOOK_TICK
                                    .load(Ordering::Acquire)
                                    .then(|| LAST_HOOK_TICK_MS.load(Ordering::Acquire));
                                if let Some(gap_ms) = evaluate_capture_watchdog(
                                    true,
                                    hook_tick,
                                    last_input_tick,
                                    WATCHDOG_BLIND_THRESHOLD_MS,
                                ) {
                                    perform_watchdog_release(gap_ms);
                                }
                            }
                        } else if HAS_ACTIVE_LINK.load(Ordering::Acquire) {
                            check_timer_elevated_leap_block(our_pid);
                        }
                        continue;
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }

                if watchdog_timer_id != 0 {
                    let _ = KillTimer(None, watchdog_timer_id);
                }
                if let Ok(hook) = h_mouse {
                    let _ = UnhookWindowsHookEx(hook);
                }
                if let Ok(hook) = h_kbd {
                    let _ = UnhookWindowsHookEx(hook);
                }
                HOOK_THREAD_ID.store(0, Ordering::SeqCst);
                restore_cursor_visibility();
            });
            true
        }
    }

    pub fn stop_hooks(&self) {
        let was_active = HOOK_ACTIVE.swap(false, Ordering::SeqCst);
        if was_active || IS_TRAPPED.load(Ordering::Acquire) {
            self.emergency_release();
        }
        HAS_ACTIVE_LINK.store(false, Ordering::Release);
        EDGE_DWELL_SEAM_IDX.store(-1, Ordering::Relaxed);
        PENDING_ENGAGE_HAS_SEAM.store(false, Ordering::Release);
        // --- SPEC-25a ---
        DRAG_MODE_ENGAGE.store(false, Ordering::Release);
        super::dragdrop::set_drag_leap_in_progress(false);
        super::dragdrop::hide_edge_catcher();
        if let Ok(mut g) = drag_mode_engaged_slot().try_lock() {
            *g = None;
        }
        // --- end SPEC-25a ---
        if let Ok(mut g) = leap_blocked_callback_slot().try_lock() {
            *g = None;
        }

        #[cfg(windows)]
        unsafe {
            let tid = HOOK_THREAD_ID.load(Ordering::SeqCst);
            if tid != 0 {
                let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }
        #[cfg(target_os = "macos")]
        macos_tap::stop();
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn disarm_trapped_in_callback() {
    IS_TRAPPED.store(false, Ordering::SeqCst);
    CURSOR_CENTERED.store(false, Ordering::Release);
    ENGAGE_PENDING.store(false, Ordering::Release);
    EDGE_DWELL_START.store(0, Ordering::Relaxed);
    EDGE_DWELL_SEAM_IDX.store(-1, Ordering::Relaxed);
    MOUSE_BUTTONS_DOWN.store(0, Ordering::Relaxed);
    // --- SPEC-25a ---
    DRAG_MODE_ENGAGE.store(false, Ordering::Release);
    super::dragdrop::set_drag_leap_in_progress(false);
    super::dragdrop::hide_edge_catcher();
    // --- end SPEC-25a ---
}

#[cfg(windows)]
const SLOW_HOOK_THRESHOLD_US: u64 = 20_000;
#[cfg(windows)]
const HOOK_GAP_THRESHOLD_US: u64 = 250_000;

#[cfg(windows)]
#[inline]
fn record_hook_tick() {
    let tick = unsafe { GetTickCount() };
    LAST_HOOK_TICK_MS.store(tick, Ordering::Release);
    HAS_LAST_HOOK_TICK.store(true, Ordering::Release);
}

#[cfg(windows)]
fn record_hook_entry(kind: HookKind) -> Option<std::time::Instant> {
    // Unconditional so the capture watchdog tracks live hook cadence even when
    // debug logging is turned off.
    record_hook_tick();
    if !debug_log::is_enabled() {
        return None;
    }
    let now = debug_log::now_us();
    let slot = match kind {
        HookKind::Keyboard => &LAST_KBD_HOOK_US,
        HookKind::Mouse => &LAST_MOUSE_HOOK_US,
    };
    let prev = slot.swap(now, Ordering::Relaxed);
    if prev != 0 && IS_TRAPPED.load(Ordering::Acquire) {
        let gap_us = now.saturating_sub(prev);
        if gap_us > HOOK_GAP_THRESHOLD_US {
            debug_log::record_event(DebugEvent::HookGap {
                ts_us: now,
                kind,
                gap_us,
            });
        }
    }
    Some(std::time::Instant::now())
}

#[cfg(windows)]
fn record_if_slow_hook(kind: HookKind, started: Option<std::time::Instant>) {
    if let Some(t0) = started {
        let duration_us = t0.elapsed().as_micros() as u64;
        if duration_us > SLOW_HOOK_THRESHOLD_US {
            debug_log::record_event(DebugEvent::SlowHook {
                ts_us: debug_log::now_us(),
                kind,
                duration_us,
            });
        }
    }
}

#[cfg(windows)]
unsafe extern "system" fn low_level_mouse_proc(
    n_code: i32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    let started = record_hook_entry(HookKind::Mouse);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        handle_mouse_event(n_code, w_param, l_param)
    }));
    record_if_slow_hook(HookKind::Mouse, started);

    match outcome {
        Ok(Some(res)) => res,
        Ok(None) => CallNextHookEx(None, n_code, w_param, l_param),
        Err(_) => {
            disarm_trapped_in_callback();
            debug_log::record_event(DebugEvent::Release {
                ts_us: debug_log::now_us(),
                reason: "mouse_hook_panic",
            });
            post_hook_thread_message(WM_KVM_RESTORE_CURSOR);
            CallNextHookEx(None, n_code, w_param, l_param)
        }
    }
}

#[cfg(windows)]
unsafe fn handle_mouse_event(n_code: i32, w_param: WPARAM, l_param: LPARAM) -> Option<LRESULT> {
    if n_code < 0 || !HOOK_ACTIVE.load(Ordering::Relaxed) {
        return None;
    }

    let hook_struct = *(l_param.0 as *const MSLLHOOKSTRUCT);
    let pt = hook_struct.pt;
    let msg = w_param.0 as u32;
    let is_trapped_now = IS_TRAPPED.load(Ordering::Acquire);
    let is_physical = is_physical_mouse_hook_event(hook_struct.flags);
    if should_ignore_injected_mouse_hook_event(
        hook_struct.flags,
        super::dragdrop::is_local_drag_source_active(),
        is_trapped_now,
    ) || (!is_trapped_now && !is_physical)
    {
        return None;
    }

    if is_physical {
        match msg {
            WM_LBUTTONDOWN => {
                MOUSE_BUTTONS_DOWN.fetch_or(1, Ordering::Relaxed);
            }
            WM_LBUTTONUP => {
                MOUSE_BUTTONS_DOWN.fetch_and(!1, Ordering::Relaxed);
            }
            WM_RBUTTONDOWN => {
                MOUSE_BUTTONS_DOWN.fetch_or(2, Ordering::Relaxed);
            }
            WM_RBUTTONUP => {
                MOUSE_BUTTONS_DOWN.fetch_and(!2, Ordering::Relaxed);
            }
            WM_MBUTTONDOWN => {
                MOUSE_BUTTONS_DOWN.fetch_or(4, Ordering::Relaxed);
            }
            WM_MBUTTONUP => {
                MOUSE_BUTTONS_DOWN.fetch_and(!4, Ordering::Relaxed);
            }
            WM_XBUTTONDOWN => {
                let xbtn = ((hook_struct.mouseData >> 16) & 0xFFFF) as u16;
                let bit = if xbtn == 2 { 16 } else { 8 };
                MOUSE_BUTTONS_DOWN.fetch_or(bit, Ordering::Relaxed);
            }
            WM_XBUTTONUP => {
                let xbtn = ((hook_struct.mouseData >> 16) & 0xFFFF) as u16;
                let bit = if xbtn == 2 { 16 } else { 8 };
                MOUSE_BUTTONS_DOWN.fetch_and(!bit, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    if IS_TRAPPED.load(Ordering::Acquire) {
        if is_physical
            && button_from_mouse_up_msg(msg).is_some()
            && super::dragdrop::is_local_drag_source_active()
        {
            if let Some(btn) = button_from_mouse_up_msg(msg) {
                super::dragdrop::on_target_physical_mouse_up(btn);
            }
            return None;
        }
        if msg == WM_MOUSEMOVE {
            let center = (
                CLAMP_CENTER_X.load(Ordering::Relaxed),
                CLAMP_CENTER_Y.load(Ordering::Relaxed),
            );
            if let Some(delta) = trapped_mouse_delta(
                CURSOR_CENTERED.load(Ordering::Acquire),
                (pt.x, pt.y),
                center,
            ) {
                if let Ok(guard) = mouse_delta_sender().try_lock() {
                    if let Some(ref tx) = *guard {
                        let _ = tx.try_send(delta);
                    }
                }
            }
        } else {
            match msg {
                WM_LBUTTONDOWN => try_send_packet(KvmPacket::MouseButton {
                    button: 1,
                    is_down: true,
                }),
                WM_LBUTTONUP => try_send_packet(KvmPacket::MouseButton {
                    button: 1,
                    is_down: false,
                }),
                WM_RBUTTONDOWN => try_send_packet(KvmPacket::MouseButton {
                    button: 2,
                    is_down: true,
                }),
                WM_RBUTTONUP => try_send_packet(KvmPacket::MouseButton {
                    button: 2,
                    is_down: false,
                }),
                WM_MBUTTONDOWN => try_send_packet(KvmPacket::MouseButton {
                    button: 3,
                    is_down: true,
                }),
                WM_MBUTTONUP => try_send_packet(KvmPacket::MouseButton {
                    button: 3,
                    is_down: false,
                }),
                WM_MOUSEWHEEL => {
                    let delta = (hook_struct.mouseData >> 16) as i16 as i32;
                    try_send_packet(KvmPacket::MouseWheel { delta });
                }
                _ => {}
            }
        }
        return Some(LRESULT(1));
    }

    if msg != WM_MOUSEMOVE || !HAS_ACTIVE_LINK.load(Ordering::Acquire) {
        // --- SPEC-25a ---
        if is_physical {
            if let Some(btn) = button_from_mouse_up_msg(msg) {
                super::dragdrop::on_target_physical_mouse_up(btn);
            }
            if MOUSE_BUTTONS_DOWN.load(Ordering::Relaxed) == 0
                && (super::dragdrop::is_edge_catcher_visible()
                    || button_from_mouse_up_msg(msg).is_some())
            {
                super::dragdrop::on_hook_mouse_up();
            }
        }
        // --- end SPEC-25a ---
        return None;
    }

    let btns_down = MOUSE_BUTTONS_DOWN.load(Ordering::Relaxed);
    if btns_down != 0 {
        EDGE_DWELL_START.store(0, Ordering::Relaxed);
        EDGE_DWELL_SEAM_IDX.store(-1, Ordering::Relaxed);
        // --- SPEC-25a ---
        if super::dragdrop::drag_drop_enabled() {
            super::dragdrop::on_mouse_drag_probe((pt.x, pt.y), btns_down, false);
        } else if super::dragdrop::is_edge_catcher_visible() {
            super::dragdrop::hide_edge_catcher();
        }
        // --- end SPEC-25a ---
        return None;
    }
    if super::dragdrop::is_edge_catcher_visible() {
        super::dragdrop::hide_edge_catcher();
    }

    let pt_pair = (pt.x, pt.y);
    if try_evaluate_edge_leap(pt_pair, |neighbor| unsafe {
        !MonitorFromPoint(
            POINT {
                x: neighbor.0,
                y: neighbor.1,
            },
            MONITOR_DEFAULTTONULL,
        )
        .is_invalid()
    }) {
        return Some(LRESULT(1));
    }

    None
}

#[cfg(any(windows, target_os = "macos"))]
fn try_evaluate_edge_leap<F>(pt: (i32, i32), is_neighbor_occupied: F) -> bool
where
    F: FnOnce((i32, i32)) -> bool,
{
    let link_edge = Edge::from_u8(LINK_FROM_EDGE.load(Ordering::Relaxed));
    let deadzone = DEADZONE_PX.load(Ordering::Relaxed);
    let resistance_ms = RESISTANCE_MS.load(Ordering::Relaxed);

    struct EdgeLeapCandidate {
        mx: i32,
        my: i32,
        mw: i32,
        mh: i32,
        leap_side: Edge,
        entry_edge: Edge,
        relative_y: f32,
        seam_info: Option<(i32, String, i32)>,
    }

    let hit: Option<EdgeLeapCandidate> = {
        let Ok(layout) = controller_layout().try_lock() else {
            return false;
        };
        if let Some(origin) = layout.remote_origin {
            detect_controller_edge_seams(
                pt,
                &layout.main_monitors,
                &layout.linked_monitors,
                origin,
                &layout.seams,
                deadzone,
            )
            .map(|h| {
                let m = &layout.main_monitors[h.main_monitor_index];
                EdgeLeapCandidate {
                    mx: m.x,
                    my: m.y,
                    mw: m.width,
                    mh: m.height,
                    leap_side: h.seam.side,
                    entry_edge: h.seam.side.opposite(),
                    relative_y: h.relative_y,
                    seam_info: Some((h.seam_index as i32, h.target_monitor, h.along_px)),
                }
            })
        } else {
            detect_controller_edge(pt, &layout.main_monitors, link_edge, deadzone).map(
                |(idx, _)| {
                    let m = &layout.main_monitors[idx];
                    let clamped = (
                        pt.0.clamp(m.x, m.x + m.width - 1),
                        pt.1.clamp(m.y, m.y + m.height - 1),
                    );
                    let local_union = ScreenRect {
                        x: LOCAL_UNION_X.load(Ordering::Relaxed),
                        y: LOCAL_UNION_Y.load(Ordering::Relaxed),
                        width: LOCAL_UNION_W.load(Ordering::Relaxed),
                        height: LOCAL_UNION_H.load(Ordering::Relaxed),
                    };
                    let peer_union = ScreenRect {
                        x: 0,
                        y: 0,
                        width: PEER_UNION_W.load(Ordering::Relaxed),
                        height: PEER_UNION_H.load(Ordering::Relaxed),
                    };
                    let rel = map_leap_relative_y(
                        clamped,
                        &local_union,
                        &peer_union,
                        link_edge,
                        LINK_OFFSET_X.load(Ordering::Relaxed),
                        LINK_OFFSET_Y.load(Ordering::Relaxed),
                    );
                    EdgeLeapCandidate {
                        mx: m.x,
                        my: m.y,
                        mw: m.width,
                        mh: m.height,
                        leap_side: link_edge,
                        entry_edge: Edge::from_u8(LINK_TO_EDGE.load(Ordering::Relaxed)),
                        relative_y: rel,
                        seam_info: None,
                    }
                },
            )
        }
    };

    let Some(cand) = hit else {
        EDGE_DWELL_START.store(0, Ordering::Relaxed);
        EDGE_DWELL_SEAM_IDX.store(-1, Ordering::Relaxed);
        return false;
    };

    // Double-check live OS monitor topology on the adjacent pixel beyond the leap edge
    let neighbor = match cand.leap_side {
        // pt may lie past the screen edge; clamp onto the edge monitor first.
        Edge::Right => (
            cand.mx + cand.mw,
            pt.1.clamp(cand.my, cand.my + cand.mh - 1),
        ),
        Edge::Left => (cand.mx - 1, pt.1.clamp(cand.my, cand.my + cand.mh - 1)),
        Edge::Top => (pt.0.clamp(cand.mx, cand.mx + cand.mw - 1), cand.my - 1),
        Edge::Bottom => (
            pt.0.clamp(cand.mx, cand.mx + cand.mw - 1),
            cand.my + cand.mh,
        ),
    };
    if is_neighbor_occupied(neighbor) {
        EDGE_DWELL_START.store(0, Ordering::Relaxed);
        EDGE_DWELL_SEAM_IDX.store(-1, Ordering::Relaxed);
        return false;
    }

    let seam_idx = cand.seam_info.as_ref().map(|(i, _, _)| *i).unwrap_or(-1);
    let prev_seam = EDGE_DWELL_SEAM_IDX.swap(seam_idx, Ordering::Relaxed);
    if prev_seam != seam_idx {
        EDGE_DWELL_START.store(0, Ordering::Relaxed);
    }

    if check_dwell_resistance(resistance_ms) {
        request_engage_leap(
            cand.mx,
            cand.my,
            cand.mw,
            cand.mh,
            cand.leap_side,
            cand.entry_edge,
            cand.relative_y,
            cand.seam_info.map(|(_, t, a)| (t, a)),
        );
        return true;
    }

    false
}

fn check_dwell_resistance(resistance_ms: u64) -> bool {
    if resistance_ms == 0 {
        return true;
    }
    let now = now_millis();
    let start = EDGE_DWELL_START.load(Ordering::Relaxed);
    if start == 0 {
        EDGE_DWELL_START.store(now, Ordering::Relaxed);
        false
    } else if now.saturating_sub(start) >= resistance_ms {
        EDGE_DWELL_START.store(0, Ordering::Relaxed);
        true
    } else {
        false
    }
}

/// Callback-safe leap transition: updates atomics and wakes the platform hook
/// thread so cursor/injection work runs outside hook callbacks.
#[cfg(any(windows, target_os = "macos"))]
#[allow(clippy::too_many_arguments)]
fn request_engage_leap(
    mx: i32,
    my: i32,
    mw: i32,
    mh: i32,
    leap_side: Edge,
    entry_edge: Edge,
    relative_y: f32,
    seam_target: Option<(String, i32)>,
) {
    IS_TRAPPED.store(true, Ordering::SeqCst);
    CURSOR_CENTERED.store(false, Ordering::Release);

    TRAP_MON_X.store(mx, Ordering::Relaxed);
    TRAP_MON_Y.store(my, Ordering::Relaxed);
    TRAP_MON_W.store(mw, Ordering::Relaxed);
    TRAP_MON_H.store(mh, Ordering::Relaxed);

    let (center_x, center_y) = compute_trap_center(mx, my, mw, mh);
    CLAMP_CENTER_X.store(center_x, Ordering::Relaxed);
    CLAMP_CENTER_Y.store(center_y, Ordering::Relaxed);
    PENDING_ENGAGE_REL_Y.store(relative_y.to_bits(), Ordering::Relaxed);
    PENDING_ENGAGE_LEAP_SIDE.store(leap_side as u8, Ordering::Relaxed);
    PENDING_ENGAGE_ENTRY_EDGE.store(entry_edge as u8, Ordering::Relaxed);
    if let Some((target_mon, along_px)) = seam_target {
        if let Ok(mut guard) = pending_engage_target_mon().try_lock() {
            *guard = target_mon;
            PENDING_ENGAGE_ALONG_PX.store(along_px, Ordering::Relaxed);
            PENDING_ENGAGE_HAS_SEAM.store(true, Ordering::Release);
        } else {
            PENDING_ENGAGE_HAS_SEAM.store(false, Ordering::Release);
        }
    } else {
        PENDING_ENGAGE_HAS_SEAM.store(false, Ordering::Release);
    }
    ENGAGE_PENDING.store(true, Ordering::Release);

    EDGE_DWELL_START.store(0, Ordering::Relaxed);
    EDGE_DWELL_SEAM_IDX.store(-1, Ordering::Relaxed);
    KEY_CTRL_DOWN.store(false, Ordering::Relaxed);
    KEY_ALT_DOWN.store(false, Ordering::Relaxed);

    if debug_log::is_enabled() {
        debug_log::record_event(DebugEvent::Engage {
            ts_us: debug_log::now_us(),
            phase: "request",
            edge: leap_side,
            relative_y,
            hwnd: debug_log::foreground_hwnd_raw(),
        });
    }

    #[cfg(windows)]
    post_hook_thread_message(WM_KVM_ENGAGE);
    #[cfg(target_os = "macos")]
    macos_tap::signal_engage();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HandoffMethod {
    Shell,
    Next,
    Minimize,
}

impl HandoffMethod {
    fn as_str(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Next => "next",
            Self::Minimize => "minimize",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessElevation {
    Standard { integrity_rid: u32 },
    Elevated,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WindowCandidateInfo {
    hwnd: isize,
    pid: u32,
    visible: bool,
    cloaked: bool,
    minimized: bool,
    tool_window: bool,
    elevation: ProcessElevation,
}

fn is_safe_handoff_elevation(elevation: ProcessElevation) -> bool {
    matches!(
        elevation,
        ProcessElevation::Standard { integrity_rid }
            if integrity_rid <= SECURITY_MANDATORY_MEDIUM_RID
    )
}

fn is_eligible_handoff_target(info: WindowCandidateInfo, our_pid: u32) -> bool {
    info.hwnd != 0
        && info.pid != 0
        && info.pid != our_pid
        && info.visible
        && !info.cloaked
        && !info.minimized
        && !info.tool_window
        && is_safe_handoff_elevation(info.elevation)
}

#[cfg(test)]
fn select_next_handoff_target(
    windows_in_z_order: &[WindowCandidateInfo],
    our_pid: u32,
) -> Option<isize> {
    windows_in_z_order
        .iter()
        .copied()
        .find(|w| is_eligible_handoff_target(*w, our_pid))
        .map(|w| w.hwnd)
}

#[cfg(test)]
fn select_preferred_handoff_targets(
    shell: Option<WindowCandidateInfo>,
    windows_in_z_order: &[WindowCandidateInfo],
    our_pid: u32,
    fg_hwnd: isize,
) -> Vec<(HandoffMethod, isize)> {
    let mut targets = Vec::with_capacity(2);
    if let Some(sh) = shell {
        if sh.hwnd != 0
            && sh.hwnd != fg_hwnd
            && sh.pid != 0
            && sh.pid != our_pid
            && is_safe_handoff_elevation(sh.elevation)
        {
            targets.push((HandoffMethod::Shell, sh.hwnd));
        }
    }
    if let Some(next_hwnd) = select_next_handoff_target(windows_in_z_order, our_pid) {
        if next_hwnd != fg_hwnd && !targets.iter().any(|&(_, h)| h == next_hwnd) {
            targets.push((HandoffMethod::Next, next_hwnd));
        }
    }
    targets
}

fn should_block_leap_for_foreground(
    our_elevation: ProcessElevation,
    fg_elevation: ProcessElevation,
) -> bool {
    let (our_elevated, our_rid) = match our_elevation {
        ProcessElevation::Elevated => (true, None),
        ProcessElevation::Standard { integrity_rid } => (false, Some(integrity_rid)),
        ProcessElevation::Unknown => (false, None),
    };
    if our_elevated {
        return false;
    }
    match fg_elevation {
        ProcessElevation::Standard { integrity_rid } => match our_rid {
            Some(ours) => integrity_rid > ours,
            None => integrity_rid > SECURITY_MANDATORY_MEDIUM_RID,
        },
        ProcessElevation::Elevated | ProcessElevation::Unknown => true,
    }
}

#[cfg(any(windows, test))]
fn evaluate_capture_watchdog(
    is_trapped: bool,
    last_hook_tick: Option<u32>,
    last_input_tick: u32,
    threshold_ms: u32,
) -> Option<u32> {
    if !is_trapped {
        return None;
    }
    let hook_tick = last_hook_tick?;
    let delta = last_input_tick.wrapping_sub(hook_tick) as i32;
    if delta > threshold_ms as i32 {
        Some(delta as u32)
    } else {
        None
    }
}

#[cfg(windows)]
unsafe fn query_token_elevation(token: HANDLE) -> ProcessElevation {
    let mut te = TOKEN_ELEVATION::default();
    let mut ret_len = 0u32;
    if GetTokenInformation(
        token,
        TokenElevation,
        Some(&mut te as *mut _ as *mut std::ffi::c_void),
        std::mem::size_of::<TOKEN_ELEVATION>() as u32,
        &mut ret_len,
    )
    .is_err()
    {
        return ProcessElevation::Unknown;
    }
    if te.TokenIsElevated != 0 {
        return ProcessElevation::Elevated;
    }

    let mut needed = 0u32;
    let _ = GetTokenInformation(token, TokenIntegrityLevel, None, 0, &mut needed);
    if needed < std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32 {
        return ProcessElevation::Unknown;
    }
    let mut buf = vec![0u8; needed as usize];
    if GetTokenInformation(
        token,
        TokenIntegrityLevel,
        Some(buf.as_mut_ptr() as *mut std::ffi::c_void),
        needed,
        &mut needed,
    )
    .is_err()
    {
        return ProcessElevation::Unknown;
    }

    let tml = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
    let sid = tml.Label.Sid;
    if sid.is_invalid() {
        return ProcessElevation::Unknown;
    }
    let count_ptr = GetSidSubAuthorityCount(sid);
    if count_ptr.is_null() {
        return ProcessElevation::Unknown;
    }
    let count = *count_ptr as u32;
    if count == 0 {
        return ProcessElevation::Unknown;
    }
    let rid_ptr = GetSidSubAuthority(sid, count - 1);
    if rid_ptr.is_null() {
        return ProcessElevation::Unknown;
    }
    let rid = *rid_ptr;
    if rid >= SECURITY_MANDATORY_HIGH_RID {
        ProcessElevation::Elevated
    } else {
        ProcessElevation::Standard { integrity_rid: rid }
    }
}

#[cfg(windows)]
unsafe fn query_own_elevation() -> ProcessElevation {
    let mut token = HANDLE::default();
    if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
        return ProcessElevation::Unknown;
    }
    let elev = query_token_elevation(token);
    let _ = CloseHandle(token);
    elev
}

#[cfg(windows)]
unsafe fn query_process_info(pid: u32) -> (ProcessElevation, String) {
    if pid == 0 {
        return (ProcessElevation::Unknown, "unknown".to_string());
    }
    let Ok(proc_handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
        return (ProcessElevation::Unknown, format!("pid:{pid}"));
    };

    let mut path_buf = [0u16; 512];
    let mut path_len = path_buf.len() as u32;
    let proc_name = if QueryFullProcessImageNameW(
        proc_handle,
        PROCESS_NAME_FORMAT(0),
        windows::core::PWSTR(path_buf.as_mut_ptr()),
        &mut path_len,
    )
    .is_ok()
        && path_len > 0
    {
        let full = String::from_utf16_lossy(&path_buf[..path_len as usize]);
        std::path::Path::new(&full)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&full)
            .to_string()
    } else {
        format!("pid:{pid}")
    };

    let mut token = HANDLE::default();
    if OpenProcessToken(proc_handle, TOKEN_QUERY, &mut token).is_err() {
        let _ = CloseHandle(proc_handle);
        return (ProcessElevation::Unknown, proc_name);
    }

    let elev = query_token_elevation(token);
    let _ = CloseHandle(token);
    let _ = CloseHandle(proc_handle);
    (elev, proc_name)
}

#[cfg(windows)]
unsafe fn is_window_cloaked(hwnd: HWND) -> bool {
    let mut cloaked: u32 = 0;
    let res = DwmGetWindowAttribute(
        hwnd,
        DWMWA_CLOAKED,
        &mut cloaked as *mut u32 as *mut std::ffi::c_void,
        std::mem::size_of::<u32>() as u32,
    );
    res.is_ok() && cloaked != 0
}

#[cfg(windows)]
unsafe fn is_window_safe_foreground(hwnd: HWND, our_pid: u32) -> bool {
    if hwnd.0.is_null() {
        return true;
    }
    let mut pid = 0u32;
    let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == 0 || pid == our_pid {
        return false;
    }
    let (elev, _) = query_process_info(pid);
    is_safe_handoff_elevation(elev)
}

#[cfg(windows)]
unsafe fn find_next_handoff_windows(fg: HWND, our_pid: u32, limit: usize) -> Vec<HWND> {
    let mut results = Vec::new();
    let mut cur = fg;
    for _ in 0..1024 {
        let Ok(next) = GetWindow(cur, GW_HWNDNEXT) else {
            break;
        };
        if next.0.is_null() {
            break;
        }
        let mut pid = 0u32;
        let _ = GetWindowThreadProcessId(next, Some(&mut pid));
        let visible = IsWindowVisible(next).as_bool();
        let minimized = IsIconic(next).as_bool();
        let ex_style = GetWindowLongW(next, GWL_EXSTYLE) as u32;
        let tool_window = (ex_style & WS_EX_TOOLWINDOW.0) != 0;

        // Check cheap window attributes before DWM / OpenProcess token queries.
        if next.0 as isize != 0
            && pid != 0
            && pid != our_pid
            && visible
            && !minimized
            && !tool_window
        {
            let cloaked = is_window_cloaked(next);
            let elevation = if cloaked {
                ProcessElevation::Unknown
            } else {
                query_process_info(pid).0
            };
            let info = WindowCandidateInfo {
                hwnd: next.0 as isize,
                pid,
                visible,
                cloaked,
                minimized,
                tool_window,
                elevation,
            };
            if is_eligible_handoff_target(info, our_pid) {
                results.push(next);
                if results.len() >= limit {
                    break;
                }
            }
        }
        cur = next;
    }
    results
}

#[cfg(windows)]
unsafe fn query_last_input_tick() -> Option<u32> {
    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    if GetLastInputInfo(&mut info).as_bool() {
        Some(info.dwTime)
    } else {
        None
    }
}

#[cfg(windows)]
fn emit_leap_blocked_notice(proc_name: &str) {
    let now = now_millis();
    let prev = LAST_BLOCKED_NOTICE_MS.load(Ordering::Relaxed);
    if now.saturating_sub(prev) < BLOCKED_NOTICE_COOLDOWN_MS {
        return;
    }
    LAST_BLOCKED_NOTICE_MS.store(now, Ordering::Relaxed);
    if let Ok(guard) = leap_blocked_callback_slot().try_lock() {
        if let Some(ref cb) = *guard {
            cb(proc_name);
        }
    }
}

#[cfg(windows)]
unsafe fn block_leap_for_elevated_foreground(fg: HWND, proc_name: &str, log_enabled: bool) {
    disarm_trapped_in_callback();
    KEY_CTRL_DOWN.store(false, Ordering::Relaxed);
    KEY_ALT_DOWN.store(false, Ordering::Relaxed);
    if log_enabled {
        debug_log::record_event(DebugEvent::Note {
            ts_us: debug_log::now_us(),
            hwnd: fg.0 as isize,
            text: format!("LEAP_BLOCKED reason=elevated_foreground proc={proc_name}"),
        });
    }
    emit_leap_blocked_notice(proc_name);
}

#[cfg(windows)]
unsafe fn check_foreground_blocks_leap(our_pid: u32) -> Option<(HWND, String)> {
    let fg = GetForegroundWindow();
    if fg.0.is_null() {
        return None;
    }
    let mut fg_pid = 0u32;
    let _ = GetWindowThreadProcessId(fg, Some(&mut fg_pid));
    if fg_pid == our_pid {
        return None;
    }
    let our_elev = query_own_elevation();
    let (fg_elev, proc_name) = query_process_info(fg_pid);
    if should_block_leap_for_foreground(our_elev, fg_elev) {
        Some((fg, proc_name))
    } else {
        None
    }
}

#[cfg(any(windows, test))]
fn should_emit_timer_blocked_notice(
    has_active_link: bool,
    is_trapped: bool,
    elevated_foreground: bool,
    at_leap_edge: bool,
    now_ms: u64,
    last_emitted_ms: u64,
    cooldown_ms: u64,
) -> bool {
    has_active_link
        && !is_trapped
        && elevated_foreground
        && at_leap_edge
        && (last_emitted_ms == 0 || now_ms.saturating_sub(last_emitted_ms) >= cooldown_ms)
}

#[cfg(windows)]
fn is_point_at_active_leap_edge(pt: (i32, i32)) -> bool {
    let link_edge = Edge::from_u8(LINK_FROM_EDGE.load(Ordering::Relaxed));
    let deadzone = DEADZONE_PX.load(Ordering::Relaxed);
    let Ok(layout) = controller_layout().try_lock() else {
        return false;
    };
    if let Some(origin) = layout.remote_origin {
        detect_controller_edge_seams(
            pt,
            &layout.main_monitors,
            &layout.linked_monitors,
            origin,
            &layout.seams,
            deadzone,
        )
        .is_some()
    } else {
        detect_controller_edge(pt, &layout.main_monitors, link_edge, deadzone).is_some()
    }
}

#[cfg(windows)]
unsafe fn check_timer_elevated_leap_block(our_pid: u32) {
    let has_active_link = HAS_ACTIVE_LINK.load(Ordering::Acquire);
    let is_trapped = IS_TRAPPED.load(Ordering::Acquire);
    if !has_active_link || is_trapped {
        return;
    }
    let Some((fg, proc_name)) = check_foreground_blocks_leap(our_pid) else {
        return;
    };
    let mut pt = POINT::default();
    if GetCursorPos(&mut pt).is_err() {
        return;
    }
    let at_edge = is_point_at_active_leap_edge((pt.x, pt.y));
    let now = now_millis();
    let prev = LAST_TIMER_BLOCKED_NOTICE_MS.load(Ordering::Relaxed);
    if !should_emit_timer_blocked_notice(
        has_active_link,
        is_trapped,
        true,
        at_edge,
        now,
        prev,
        TIMER_BLOCKED_NOTICE_COOLDOWN_MS,
    ) {
        return;
    }
    LAST_TIMER_BLOCKED_NOTICE_MS.store(now, Ordering::Relaxed);
    if debug_log::is_enabled() {
        debug_log::record_event(DebugEvent::Note {
            ts_us: debug_log::now_us(),
            hwnd: fg.0 as isize,
            text: format!("LEAP_BLOCKED reason=elevated_foreground source=timer proc={proc_name}"),
        });
    }
    if let Ok(guard) = leap_blocked_callback_slot().try_lock() {
        if let Some(ref cb) = *guard {
            cb(&proc_name);
        }
    }
}

#[cfg(windows)]
unsafe fn perform_watchdog_release(gap_ms: u32) {
    if debug_log::is_enabled() {
        debug_log::record_event(DebugEvent::Note {
            ts_us: debug_log::now_us(),
            hwnd: debug_log::foreground_hwnd_raw(),
            text: format!("WATCHDOG_RELEASE gap_ms={gap_ms}"),
        });
        debug_log::record_event(DebugEvent::Release {
            ts_us: debug_log::now_us(),
            reason: "watchdog_release",
        });
    }
    disarm_trapped_in_callback();
    KEY_CTRL_DOWN.store(false, Ordering::Relaxed);
    KEY_ALT_DOWN.store(false, Ordering::Relaxed);
    if let Ok(mut tracker) = controller_tracker().try_lock() {
        let _ = tracker.release_all();
    }
    restore_cursor_visibility();
    super::dragdrop::cancel_active_target_drag_with_reason("emergency_release");
    super::dragdrop::hide_edge_catcher();
    try_send_packet(KvmPacket::EmergencyRelease);
    try_send_packet(KvmPacket::ReleaseAll);
}

#[cfg(windows)]
unsafe fn handoff_own_foreground_focus(log_enabled: bool) {
    let fg = GetForegroundWindow();
    if fg.0.is_null() {
        return;
    }
    let our_pid = GetCurrentProcessId();
    let mut fg_pid = 0u32;
    let _ = GetWindowThreadProcessId(fg, Some(&mut fg_pid));
    if fg_pid != our_pid {
        return;
    }

    let mut attempts: u32 = 0;
    let mut final_method = HandoffMethod::Minimize;
    let mut final_to = fg;
    let mut ok = false;

    // Prefer GetShellWindow() (desktop) first without AttachThreadInput so we
    // never risk selecting an elevated Z-order neighbour like Taskmgr.exe.
    let shell = GetShellWindow();
    if !shell.0.is_null() && shell != fg {
        let mut shell_pid = 0u32;
        let _ = GetWindowThreadProcessId(shell, Some(&mut shell_pid));
        if shell_pid != 0
            && shell_pid != our_pid
            && is_safe_handoff_elevation(query_process_info(shell_pid).0)
        {
            attempts += 1;
            let _ = SetForegroundWindow(shell);
            let after = GetForegroundWindow();
            if after != fg && is_window_safe_foreground(after, our_pid) {
                final_method = HandoffMethod::Shell;
                final_to = after;
                ok = true;
            }
        }
    }

    // Only if GetShellWindow() failed, walk Z-order for a visible, non-cloaked,
    // non-minimised, non-elevated top-level window that is not ours.
    if !ok {
        for next in find_next_handoff_windows(fg, our_pid, 8) {
            attempts += 1;
            let _ = SetForegroundWindow(next);
            let _ = BringWindowToTop(next);
            let after = GetForegroundWindow();
            if after != fg && is_window_safe_foreground(after, our_pid) {
                final_method = HandoffMethod::Next;
                final_to = after;
                ok = true;
                break;
            }
        }
    }

    if !ok {
        attempts += 1;
        let _ = ShowWindow(fg, SW_MINIMIZE);
        let after = GetForegroundWindow();
        final_method = HandoffMethod::Minimize;
        final_to = after;
        ok = after != fg;
    }

    if log_enabled {
        debug_log::record_event(DebugEvent::Note {
            ts_us: debug_log::now_us(),
            hwnd: final_to.0 as isize,
            text: format!(
                "FOCUS_HANDOFF from=0x{:X} to=0x{:X} method={} attempts={} ok={ok}",
                fg.0 as isize,
                final_to.0 as isize,
                final_method.as_str(),
                attempts,
            ),
        });
    }
}

#[cfg(windows)]
unsafe fn perform_engage_leap() -> bool {
    if !should_perform_engage(
        HOOK_ACTIVE.load(Ordering::Relaxed),
        IS_TRAPPED.load(Ordering::Acquire),
        ENGAGE_PENDING.swap(false, Ordering::AcqRel),
    ) {
        return false;
    }

    let log_enabled = debug_log::is_enabled();
    let our_pid = GetCurrentProcessId();

    if let Some((fg, proc_name)) = check_foreground_blocks_leap(our_pid) {
        // --- SPEC-25a ---
        DRAG_MODE_ENGAGE.store(false, Ordering::Release);
        super::dragdrop::set_drag_leap_in_progress(false);
        // --- end SPEC-25a ---
        block_leap_for_elevated_foreground(fg, &proc_name, log_enabled);
        return false;
    }

    handoff_own_foreground_focus(log_enabled);

    // Guard again in case minimizing our own window promoted an elevated window.
    if let Some((fg, proc_name)) = check_foreground_blocks_leap(our_pid) {
        // --- SPEC-25a ---
        DRAG_MODE_ENGAGE.store(false, Ordering::Release);
        super::dragdrop::set_drag_leap_in_progress(false);
        // --- end SPEC-25a ---
        block_leap_for_elevated_foreground(fg, &proc_name, log_enabled);
        return false;
    }

    let mx = TRAP_MON_X.load(Ordering::Relaxed);
    let my = TRAP_MON_Y.load(Ordering::Relaxed);
    let mw = TRAP_MON_W.load(Ordering::Relaxed);
    let mh = TRAP_MON_H.load(Ordering::Relaxed);
    let center_x = CLAMP_CENTER_X.load(Ordering::Relaxed);
    let center_y = CLAMP_CENTER_Y.load(Ordering::Relaxed);
    let relative_y = f32::from_bits(PENDING_ENGAGE_REL_Y.load(Ordering::Relaxed));
    let fg_hwnd = if log_enabled {
        debug_log::foreground_hwnd_raw()
    } else {
        0
    };

    let clip_rect = RECT {
        left: mx,
        top: my,
        right: mx + mw,
        bottom: my + mh,
    };
    let _ = ClipCursor(Some(&clip_rect));
    let _ = SetCursorPos(center_x, center_y);
    CURSOR_CENTERED.store(true, Ordering::Release);

    for _ in 0..16 {
        if ShowCursor(false) < 0 {
            break;
        }
    }

    let (tracker_before, released, send_input_us) = release_controller_held_keys(log_enabled);

    record_hook_tick();

    finalize_engage_leap(
        relative_y,
        fg_hwnd,
        log_enabled,
        tracker_before,
        released,
        send_input_us,
    );
    true
}

#[cfg(any(windows, target_os = "macos"))]
fn release_controller_held_keys(log_enabled: bool) -> (KeySnapshot, KeySnapshot, u64) {
    if let Ok(mut tracker) = controller_tracker().try_lock() {
        let before = if log_enabled {
            KeySnapshot::from_slice(tracker.held_keys())
        } else {
            KeySnapshot::lock_failed()
        };
        let keys = tracker.release_keys();
        let rel_snap = if log_enabled {
            KeySnapshot::from_slice(&keys)
        } else {
            KeySnapshot::lock_failed()
        };
        let t0 = log_enabled.then(std::time::Instant::now);
        for k in &keys {
            let _ = super::injector::inject_key(k.key_code, k.scan_code, false, k.extended);
        }
        let elapsed_us = t0.map(|t| t.elapsed().as_micros() as u64).unwrap_or(0);
        (before, rel_snap, elapsed_us)
    } else {
        (KeySnapshot::lock_failed(), KeySnapshot::lock_failed(), 0)
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn finalize_engage_leap(
    relative_y: f32,
    fg_hwnd: isize,
    log_enabled: bool,
    tracker_before: KeySnapshot,
    released: KeySnapshot,
    send_input_us: u64,
) {
    if log_enabled {
        debug_log::record_event(DebugEvent::EngagePerform {
            ts_us: debug_log::now_us(),
            edge: Edge::from_u8(PENDING_ENGAGE_LEAP_SIDE.load(Ordering::Relaxed)),
            relative_y,
            hwnd: fg_hwnd,
            tracker_before,
            released,
            send_input_us,
        });
    }

    let to_edge = Edge::from_u8(PENDING_ENGAGE_ENTRY_EDGE.load(Ordering::Relaxed));
    let dwell_ms = RESISTANCE_MS.load(Ordering::Relaxed) as u32;
    let deadzone_px = DEADZONE_PX.load(Ordering::Relaxed);
    let (target_monitor, along_px) = if PENDING_ENGAGE_HAS_SEAM.swap(false, Ordering::AcqRel) {
        let mon = pending_engage_target_mon()
            .try_lock()
            .ok()
            .map(|g| g.clone())
            .filter(|s| !s.is_empty());
        let along = mon
            .as_ref()
            .map(|_| PENDING_ENGAGE_ALONG_PX.load(Ordering::Relaxed));
        (mon, along)
    } else {
        (None, None)
    };
    try_send_packet(KvmPacket::CursorEnter {
        edge: to_edge.as_str().to_string(),
        relative_y,
        dwell_ms,
        deadzone_px,
        target_monitor,
        along_px,
    });
    // --- SPEC-25a ---
    if DRAG_MODE_ENGAGE.swap(false, Ordering::AcqRel) {
        let btn = DRAG_MODE_BUTTON.load(Ordering::Acquire);
        super::dragdrop::set_drag_leap_in_progress(false);
        let mut dispatched = false;
        if super::dragdrop::drag_drop_enabled() {
            if let Ok(g) = drag_mode_engaged_slot().try_lock() {
                if let Some(ref cb) = *g {
                    dispatched = true;
                    cb(btn);
                }
            }
        }
        debug_log::note(|| format!("DRAG_LEAP_DISPATCH_CB button={btn} dispatched={dispatched}"));
    }
    // --- end SPEC-25a ---
}

#[cfg(windows)]
unsafe extern "system" fn low_level_keyboard_proc(
    n_code: i32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    let started = record_hook_entry(HookKind::Keyboard);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        handle_keyboard_event(n_code, w_param, l_param)
    }));
    record_if_slow_hook(HookKind::Keyboard, started);

    match outcome {
        Ok(Some(res)) => res,
        Ok(None) => CallNextHookEx(None, n_code, w_param, l_param),
        Err(_) => {
            disarm_trapped_in_callback();
            debug_log::record_event(DebugEvent::Release {
                ts_us: debug_log::now_us(),
                reason: "kbd_hook_panic",
            });
            post_hook_thread_message(WM_KVM_RESTORE_CURSOR);
            CallNextHookEx(None, n_code, w_param, l_param)
        }
    }
}

#[cfg(windows)]
unsafe fn handle_keyboard_event(n_code: i32, w_param: WPARAM, l_param: LPARAM) -> Option<LRESULT> {
    if n_code < 0 {
        return None;
    }

    let kbd = *(l_param.0 as *const KBDLLHOOKSTRUCT);
    let vk_code = kbd.vkCode as u16;
    let scan_code = kbd.scanCode as u16;
    let is_down = w_param.0 as u32 == WM_KEYDOWN || w_param.0 as u32 == WM_SYSKEYDOWN;
    let extended = (kbd.flags.0 & LLKHF_EXTENDED.0) != 0;
    let injected = (kbd.flags.0 & (LLKHF_INJECTED.0 | 0x02)) != 0;
    let hook_active = HOOK_ACTIVE.load(Ordering::Relaxed);
    let trapped = IS_TRAPPED.load(Ordering::Acquire);
    let log_enabled = debug_log::is_enabled();
    let ts_us = if log_enabled { debug_log::now_us() } else { 0 };
    let hwnd = if log_enabled {
        debug_log::foreground_hwnd_raw()
    } else {
        0
    };

    if !hook_active {
        if log_enabled {
            debug_log::record_event(DebugEvent::Key {
                ts_us,
                vk: vk_code,
                scan_code,
                is_down,
                extended,
                injected,
                is_trapped: trapped,
                decision: KeyDecision::Pass,
                hwnd,
            });
        }
        return None;
    }

    if injected {
        if log_enabled {
            debug_log::record_event(DebugEvent::Key {
                ts_us,
                vk: vk_code,
                scan_code,
                is_down,
                extended,
                injected: true,
                is_trapped: trapped,
                decision: KeyDecision::Pass,
                hwnd,
            });
        }
        return None;
    }

    if vk_code == 0x11 || vk_code == 0xA2 || vk_code == 0xA3 {
        KEY_CTRL_DOWN.store(is_down, Ordering::Relaxed);
    } else if vk_code == 0x12 || vk_code == 0xA4 || vk_code == 0xA5 {
        KEY_ALT_DOWN.store(is_down, Ordering::Relaxed);
    }

    let is_scroll_lock = vk_code == 0x91;
    let is_pause = vk_code == 0x13;
    let is_ctrl_alt_esc = vk_code == 0x1B
        && KEY_CTRL_DOWN.load(Ordering::Relaxed)
        && KEY_ALT_DOWN.load(Ordering::Relaxed);

    if is_scroll_lock || is_pause || is_ctrl_alt_esc {
        disarm_trapped_in_callback();
        KEY_CTRL_DOWN.store(false, Ordering::Relaxed);
        KEY_ALT_DOWN.store(false, Ordering::Relaxed);
        if let Ok(mut tracker) = controller_tracker().try_lock() {
            let _ = tracker.release_all();
        }
        if log_enabled {
            debug_log::record_event(DebugEvent::Key {
                ts_us,
                vk: vk_code,
                scan_code,
                is_down,
                extended,
                injected: false,
                is_trapped: trapped,
                decision: KeyDecision::Block,
                hwnd,
            });
            debug_log::record_event(DebugEvent::Release {
                ts_us,
                reason: "emergency_hotkey",
            });
        }
        post_hook_thread_message(WM_KVM_RESTORE_CURSOR);
        super::dragdrop::cancel_active_target_drag_with_reason("emergency_release");
        super::dragdrop::hide_edge_catcher();

        try_send_packet(KvmPacket::EmergencyRelease);
        try_send_packet(KvmPacket::ReleaseAll);
        return Some(LRESULT(0));
    }

    // --- SPEC-25b ---
    if super::media_keys::is_media_key(vk_code) {
        let target = super::media_keys::current_media_key_target_for_vk(vk_code, trapped);
        let (decision, res) = match target {
            super::media_keys::MediaKeyTarget::Linked => {
                try_send_packet(KvmPacket::MediaKey {
                    vk: vk_code,
                    is_down,
                });
                (KeyDecision::Forward, Some(LRESULT(1)))
            }
            super::media_keys::MediaKeyTarget::Local => (KeyDecision::Pass, None),
        };
        if log_enabled {
            debug_log::record_event(DebugEvent::Key {
                ts_us,
                vk: vk_code,
                scan_code,
                is_down,
                extended,
                injected: false,
                is_trapped: trapped,
                decision,
                hwnd,
            });
        }
        return res;
    }
    // --- /SPEC-25b ---

    if trapped {
        if log_enabled {
            debug_log::record_event(DebugEvent::Key {
                ts_us,
                vk: vk_code,
                scan_code,
                is_down,
                extended,
                injected: false,
                is_trapped: true,
                decision: KeyDecision::Forward,
                hwnd,
            });
        }
        try_send_packet(KvmPacket::KeyEvent {
            key_code: vk_code,
            scan_code,
            is_down,
            extended,
        });
        return Some(LRESULT(1));
    }

    if log_enabled {
        debug_log::record_event(DebugEvent::Key {
            ts_us,
            vk: vk_code,
            scan_code,
            is_down,
            extended,
            injected: false,
            is_trapped: false,
            decision: KeyDecision::Pass,
            hwnd,
        });
    }

    if let Ok(mut tracker) = controller_tracker().try_lock() {
        tracker.record_key(vk_code, scan_code, is_down, extended, false);
    }

    None
}

#[cfg(any(target_os = "macos", test))]
mod macos_logic {
    use super::super::input_logic::monitor_at_point;
    use super::super::monitors::MonitorDisplay;

    pub const KCG_EVENT_LEFT_MOUSE_DOWN: u32 = 1;
    pub const KCG_EVENT_LEFT_MOUSE_UP: u32 = 2;
    pub const KCG_EVENT_RIGHT_MOUSE_DOWN: u32 = 3;
    pub const KCG_EVENT_RIGHT_MOUSE_UP: u32 = 4;
    pub const KCG_EVENT_MOUSE_MOVED: u32 = 5;
    pub const KCG_EVENT_LEFT_MOUSE_DRAGGED: u32 = 6;
    pub const KCG_EVENT_RIGHT_MOUSE_DRAGGED: u32 = 7;
    pub const KCG_EVENT_KEY_DOWN: u32 = 10;
    pub const KCG_EVENT_KEY_UP: u32 = 11;
    pub const KCG_EVENT_FLAGS_CHANGED: u32 = 12;
    pub const KCG_EVENT_SCROLL_WHEEL: u32 = 22;
    pub const KCG_EVENT_OTHER_MOUSE_DOWN: u32 = 25;
    pub const KCG_EVENT_OTHER_MOUSE_UP: u32 = 26;
    pub const KCG_EVENT_OTHER_MOUSE_DRAGGED: u32 = 27;
    pub const KCG_EVENT_TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
    pub const KCG_EVENT_TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;

    pub const KCG_FLAG_ALPHA_SHIFT: u64 = 0x0001_0000;
    pub const KCG_FLAG_SHIFT: u64 = 0x0002_0000;
    pub const KCG_FLAG_CONTROL: u64 = 0x0004_0000;
    pub const KCG_FLAG_OPTION: u64 = 0x0008_0000;
    pub const KCG_FLAG_COMMAND: u64 = 0x0010_0000;

    pub const KCG_FLAG_DEV_LCTRL: u64 = 0x0000_0001;
    pub const KCG_FLAG_DEV_LSHIFT: u64 = 0x0000_0002;
    pub const KCG_FLAG_DEV_RSHIFT: u64 = 0x0000_0004;
    pub const KCG_FLAG_DEV_LCMD: u64 = 0x0000_0008;
    pub const KCG_FLAG_DEV_RCMD: u64 = 0x0000_0010;
    pub const KCG_FLAG_DEV_LOPT: u64 = 0x0000_0020;
    pub const KCG_FLAG_DEV_ROPT: u64 = 0x0000_0040;
    pub const KCG_FLAG_DEV_RCTRL: u64 = 0x0000_2000;

    /// Maps macOS `CGKeyCode` (`kVK_*`) to `(vk_code, ps2_set1_scan_code, extended)`, symmetric
    /// with `SPEC-28` (`Command -> Win`, `Option -> Alt`, `Control -> Ctrl`).
    pub fn macos_cg_keycode_to_win_key(cg_keycode: u16) -> Option<(u16, u16, bool)> {
        match cg_keycode {
            // Letters (kVK_ANSI_A .. kVK_ANSI_Z)
            0x00 => Some((0x41, 0x1E, false)), // A
            0x0B => Some((0x42, 0x30, false)), // B
            0x08 => Some((0x43, 0x2E, false)), // C
            0x02 => Some((0x44, 0x20, false)), // D
            0x0E => Some((0x45, 0x12, false)), // E
            0x03 => Some((0x46, 0x21, false)), // F
            0x05 => Some((0x47, 0x22, false)), // G
            0x04 => Some((0x48, 0x23, false)), // H
            0x22 => Some((0x49, 0x17, false)), // I
            0x26 => Some((0x4A, 0x24, false)), // J
            0x28 => Some((0x4B, 0x25, false)), // K
            0x25 => Some((0x4C, 0x26, false)), // L
            0x2E => Some((0x4D, 0x32, false)), // M
            0x2D => Some((0x4E, 0x31, false)), // N
            0x1F => Some((0x4F, 0x18, false)), // O
            0x23 => Some((0x50, 0x19, false)), // P
            0x0C => Some((0x51, 0x10, false)), // Q
            0x0F => Some((0x52, 0x13, false)), // R
            0x01 => Some((0x53, 0x1F, false)), // S
            0x11 => Some((0x54, 0x14, false)), // T
            0x20 => Some((0x55, 0x16, false)), // U
            0x09 => Some((0x56, 0x2F, false)), // V
            0x0D => Some((0x57, 0x11, false)), // W
            0x07 => Some((0x58, 0x2D, false)), // X
            0x10 => Some((0x59, 0x15, false)), // Y
            0x06 => Some((0x5A, 0x2C, false)), // Z

            // Digits
            0x1D => Some((0x30, 0x0B, false)), // 0
            0x12 => Some((0x31, 0x02, false)), // 1
            0x13 => Some((0x32, 0x03, false)), // 2
            0x14 => Some((0x33, 0x04, false)), // 3
            0x15 => Some((0x34, 0x05, false)), // 4
            0x17 => Some((0x35, 0x06, false)), // 5
            0x16 => Some((0x36, 0x07, false)), // 6
            0x1A => Some((0x37, 0x08, false)), // 7
            0x1C => Some((0x38, 0x09, false)), // 8
            0x19 => Some((0x39, 0x0A, false)), // 9

            // Editing, whitespace, navigation
            0x33 => Some((0x08, 0x0E, false)), // kVK_Delete -> VK_BACK
            0x30 => Some((0x09, 0x0F, false)), // kVK_Tab -> VK_TAB
            0x24 => Some((0x0D, 0x1C, false)), // kVK_Return -> VK_RETURN
            0x4C => Some((0x0D, 0x1C, true)),  // kVK_ANSI_KeypadEnter -> VK_RETURN extended
            0x35 => Some((0x1B, 0x01, false)), // kVK_Escape -> VK_ESCAPE
            0x31 => Some((0x20, 0x39, false)), // kVK_Space -> VK_SPACE
            0x74 => Some((0x21, 0x49, true)),  // kVK_PageUp -> VK_PRIOR
            0x79 => Some((0x22, 0x51, true)),  // kVK_PageDown -> VK_NEXT
            0x77 => Some((0x23, 0x4F, true)),  // kVK_End -> VK_END
            0x73 => Some((0x24, 0x47, true)),  // kVK_Home -> VK_HOME
            0x7B => Some((0x25, 0x4B, true)),  // kVK_LeftArrow -> VK_LEFT
            0x7E => Some((0x26, 0x48, true)),  // kVK_UpArrow -> VK_UP
            0x7C => Some((0x27, 0x4D, true)),  // kVK_RightArrow -> VK_RIGHT
            0x7D => Some((0x28, 0x50, true)),  // kVK_DownArrow -> VK_DOWN
            0x72 => Some((0x2D, 0x52, true)),  // kVK_Help -> VK_INSERT
            0x75 => Some((0x2E, 0x53, true)),  // kVK_ForwardDelete -> VK_DELETE
            0x6E => Some((0x5D, 0x5D, true)),  // kVK_ContextualMenu -> VK_APPS

            // Modifiers (1:1 Command->Win, Option->Alt, Control->Ctrl)
            0x38 => Some((0xA0, 0x2A, false)), // kVK_Shift -> VK_LSHIFT
            0x3C => Some((0xA1, 0x36, false)), // kVK_RightShift -> VK_RSHIFT
            0x3B => Some((0xA2, 0x1D, false)), // kVK_Control -> VK_LCONTROL
            0x3E => Some((0xA3, 0x1D, true)),  // kVK_RightControl -> VK_RCONTROL
            0x3A => Some((0xA4, 0x38, false)), // kVK_Option -> VK_LMENU
            0x3D => Some((0xA5, 0x38, true)),  // kVK_RightOption -> VK_RMENU
            0x37 => Some((0x5B, 0x5B, true)),  // kVK_Command -> VK_LWIN
            0x36 => Some((0x5C, 0x5C, true)),  // kVK_RightCommand -> VK_RWIN
            0x39 => Some((0x14, 0x3A, false)), // kVK_CapsLock -> VK_CAPITAL

            // Function keys F1..F12 (Set 1 scancodes), F13..F20 (VK only so SendInput uses wVk)
            0x7A => Some((0x70, 0x3B, false)), // F1
            0x78 => Some((0x71, 0x3C, false)), // F2
            0x63 => Some((0x72, 0x3D, false)), // F3
            0x76 => Some((0x73, 0x3E, false)), // F4
            0x60 => Some((0x74, 0x3F, false)), // F5
            0x61 => Some((0x75, 0x40, false)), // F6
            0x62 => Some((0x76, 0x41, false)), // F7
            0x64 => Some((0x77, 0x42, false)), // F8
            0x65 => Some((0x78, 0x43, false)), // F9
            0x6D => Some((0x79, 0x44, false)), // F10
            0x67 => Some((0x7A, 0x57, false)), // F11
            0x6F => Some((0x7B, 0x58, false)), // F12
            0x69 => Some((0x7C, 0x00, false)), // F13
            0x6B => Some((0x7D, 0x00, false)), // F14
            0x71 => Some((0x7E, 0x00, false)), // F15
            0x6A => Some((0x7F, 0x00, false)), // F16
            0x40 => Some((0x80, 0x00, false)), // F17
            0x4F => Some((0x81, 0x00, false)), // F18
            0x50 => Some((0x82, 0x00, false)), // F19
            0x5A => Some((0x83, 0x00, false)), // F20

            // Numpad
            0x52 => Some((0x60, 0x52, false)), // VK_NUMPAD0
            0x53 => Some((0x61, 0x4F, false)), // VK_NUMPAD1
            0x54 => Some((0x62, 0x50, false)), // VK_NUMPAD2
            0x55 => Some((0x63, 0x51, false)), // VK_NUMPAD3
            0x56 => Some((0x64, 0x4B, false)), // VK_NUMPAD4
            0x57 => Some((0x65, 0x4C, false)), // VK_NUMPAD5
            0x58 => Some((0x66, 0x4D, false)), // VK_NUMPAD6
            0x59 => Some((0x67, 0x47, false)), // VK_NUMPAD7
            0x5B => Some((0x68, 0x48, false)), // VK_NUMPAD8
            0x5C => Some((0x69, 0x49, false)), // VK_NUMPAD9
            0x43 => Some((0x6A, 0x37, false)), // VK_MULTIPLY
            0x45 => Some((0x6B, 0x4E, false)), // VK_ADD
            0x4E => Some((0x6D, 0x4A, false)), // VK_SUBTRACT
            0x41 => Some((0x6E, 0x53, false)), // VK_DECIMAL
            0x4B => Some((0x6F, 0x35, true)),  // VK_DIVIDE
            0x47 => Some((0x90, 0x45, true)),  // kVK_ANSI_KeypadClear -> VK_NUMLOCK
            0x51 => Some((0xBB, 0x0D, false)), // kVK_ANSI_KeypadEquals -> VK_OEM_PLUS

            // ANSI / ISO OEM punctuation
            0x29 => Some((0xBA, 0x27, false)), // ;:
            0x18 => Some((0xBB, 0x0D, false)), // =+
            0x2B => Some((0xBC, 0x33, false)), // ,<
            0x1B => Some((0xBD, 0x0C, false)), // -_
            0x2F => Some((0xBE, 0x34, false)), // .>
            0x2C => Some((0xBF, 0x35, false)), // /?
            0x32 => Some((0xC0, 0x29, false)), // `~
            0x21 => Some((0xDB, 0x1A, false)), // [{
            0x2A => Some((0xDC, 0x2B, false)), // \|
            0x1E => Some((0xDD, 0x1B, false)), // ]}
            0x27 => Some((0xDE, 0x28, false)), // '"
            0x0A => Some((0xE2, 0x56, false)), // kVK_ISO_Section -> VK_OEM_102
            _ => None,
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ModifierAction {
        None,
        Key {
            vk: u16,
            scan: u16,
            is_down: bool,
            extended: bool,
        },
        TogglePulse {
            vk: u16,
            scan: u16,
        },
    }

    #[derive(Debug, Default)]
    pub struct MacTapModifierTracker {
        lshift: bool,
        rshift: bool,
        lctrl: bool,
        rctrl: bool,
        lopt: bool,
        ropt: bool,
        lcmd: bool,
        rcmd: bool,
        caps_lock: bool,
    }

    impl MacTapModifierTracker {
        pub fn clear(&mut self) {
            *self = Self::default();
        }

        pub fn ctrl_and_option_down(&self) -> bool {
            (self.lctrl || self.rctrl) && (self.lopt || self.ropt)
        }

        pub fn on_flags_changed(&mut self, cg_keycode: u16, raw_flags: u64) -> ModifierAction {
            if cg_keycode == 0x39 {
                let alpha = (raw_flags & KCG_FLAG_ALPHA_SHIFT) != 0;
                if alpha != self.caps_lock {
                    self.caps_lock = alpha;
                    return ModifierAction::TogglePulse {
                        vk: 0x14,
                        scan: 0x3A,
                    };
                }
                return ModifierAction::None;
            }

            let Some((vk, scan, extended)) = macos_cg_keycode_to_win_key(cg_keycode) else {
                return ModifierAction::None;
            };

            let (dev_pair, dev_bit, generic_bit, slot) = match cg_keycode {
                0x38 => (
                    KCG_FLAG_DEV_LSHIFT | KCG_FLAG_DEV_RSHIFT,
                    KCG_FLAG_DEV_LSHIFT,
                    KCG_FLAG_SHIFT,
                    &mut self.lshift,
                ),
                0x3C => (
                    KCG_FLAG_DEV_LSHIFT | KCG_FLAG_DEV_RSHIFT,
                    KCG_FLAG_DEV_RSHIFT,
                    KCG_FLAG_SHIFT,
                    &mut self.rshift,
                ),
                0x3B => (
                    KCG_FLAG_DEV_LCTRL | KCG_FLAG_DEV_RCTRL,
                    KCG_FLAG_DEV_LCTRL,
                    KCG_FLAG_CONTROL,
                    &mut self.lctrl,
                ),
                0x3E => (
                    KCG_FLAG_DEV_LCTRL | KCG_FLAG_DEV_RCTRL,
                    KCG_FLAG_DEV_RCTRL,
                    KCG_FLAG_CONTROL,
                    &mut self.rctrl,
                ),
                0x3A => (
                    KCG_FLAG_DEV_LOPT | KCG_FLAG_DEV_ROPT,
                    KCG_FLAG_DEV_LOPT,
                    KCG_FLAG_OPTION,
                    &mut self.lopt,
                ),
                0x3D => (
                    KCG_FLAG_DEV_LOPT | KCG_FLAG_DEV_ROPT,
                    KCG_FLAG_DEV_ROPT,
                    KCG_FLAG_OPTION,
                    &mut self.ropt,
                ),
                0x37 => (
                    KCG_FLAG_DEV_LCMD | KCG_FLAG_DEV_RCMD,
                    KCG_FLAG_DEV_LCMD,
                    KCG_FLAG_COMMAND,
                    &mut self.lcmd,
                ),
                0x36 => (
                    KCG_FLAG_DEV_LCMD | KCG_FLAG_DEV_RCMD,
                    KCG_FLAG_DEV_RCMD,
                    KCG_FLAG_COMMAND,
                    &mut self.rcmd,
                ),
                _ => return ModifierAction::None,
            };

            let is_down = if (raw_flags & dev_pair) != 0 {
                (raw_flags & dev_bit) != 0
            } else {
                (raw_flags & generic_bit) != 0
            };

            if is_down == *slot {
                return ModifierAction::None;
            }
            *slot = is_down;
            ModifierAction::Key {
                vk,
                scan,
                is_down,
                extended,
            }
        }
    }

    pub fn is_emergency_release_combo(vk_code: u16, ctrl_down: bool, opt_down: bool) -> bool {
        vk_code == 0x91 || vk_code == 0x13 || (vk_code == 0x1B && ctrl_down && opt_down)
    }

    #[derive(Debug, Default)]
    pub struct MacTapDeltaTracker {
        center: (f64, f64),
        leap_edge_pt: (f64, f64),
        last_anchor: (f64, f64),
        settling: bool,
        acc_x: f64,
        acc_y: f64,
    }

    impl MacTapDeltaTracker {
        pub fn engage(&mut self, center: (f64, f64), leap_edge_pt: (f64, f64)) {
            self.center = center;
            self.leap_edge_pt = leap_edge_pt;
            self.last_anchor = center;
            self.settling = true;
            self.acc_x = 0.0;
            self.acc_y = 0.0;
        }

        pub fn reset(&mut self) {
            self.settling = false;
            self.acc_x = 0.0;
            self.acc_y = 0.0;
        }

        pub fn on_moved(
            &mut self,
            pt: (f64, f64),
            elapsed_since_engage_ms: u64,
        ) -> Option<(i32, i32, bool)> {
            if self.settling {
                let d_edge =
                    (pt.0 - self.leap_edge_pt.0).powi(2) + (pt.1 - self.leap_edge_pt.1).powi(2);
                let d_center = (pt.0 - self.center.0).powi(2) + (pt.1 - self.center.1).powi(2);
                if elapsed_since_engage_ms <= 25 && d_edge < d_center && d_center > 16.0 {
                    return None;
                }
                self.settling = false;
                self.last_anchor = self.center;
            }

            let raw_dx = pt.0 - self.last_anchor.0;
            let raw_dy = pt.1 - self.last_anchor.1;
            self.acc_x += raw_dx;
            self.acc_y += raw_dy;

            let dx = self.acc_x.trunc() as i32;
            let dy = self.acc_y.trunc() as i32;
            self.acc_x -= dx as f64;
            self.acc_y -= dy as f64;

            let should_warp = dx != 0
                || dy != 0
                || (pt.0 - self.center.0).abs() >= 1.0
                || (pt.1 - self.center.1).abs() >= 1.0;
            self.last_anchor = if should_warp { self.center } else { pt };
            Some((dx, dy, should_warp))
        }
    }

    #[derive(Debug, Default)]
    pub struct MacTapScrollAccumulator {
        _reserved: (),
    }

    impl MacTapScrollAccumulator {
        pub fn push(
            &mut self,
            is_continuous: bool,
            point_delta_axis1: i64,
            discrete_delta_axis1: i64,
        ) -> i32 {
            if is_continuous {
                let clamped = point_delta_axis1.clamp(-10_000, 10_000) as i32;
                clamped.saturating_mul(3)
            } else {
                let clamped = discrete_delta_axis1.clamp(-127, 127) as i32;
                clamped.saturating_mul(120)
            }
        }
    }

    pub fn cg_button_to_kvm(event_type: u32, button_number: i64) -> Option<(u8, bool)> {
        match event_type {
            KCG_EVENT_LEFT_MOUSE_DOWN => Some((1, true)),
            KCG_EVENT_LEFT_MOUSE_UP => Some((1, false)),
            KCG_EVENT_RIGHT_MOUSE_DOWN => Some((2, true)),
            KCG_EVENT_RIGHT_MOUSE_UP => Some((2, false)),
            KCG_EVENT_OTHER_MOUSE_DOWN if button_number == 2 => Some((3, true)),
            KCG_EVENT_OTHER_MOUSE_UP if button_number == 2 => Some((3, false)),
            _ => None,
        }
    }

    pub fn is_point_on_local_display(pt: (i32, i32), monitors: &[MonitorDisplay]) -> bool {
        monitor_at_point(monitors, pt.0, pt.1).is_some()
    }
}

#[cfg(target_os = "macos")]
mod macos_tap {
    use super::super::debug_log::{self, DebugEvent, KeyDecision};
    use super::super::injector::{
        check_accessibility_permission, check_input_monitoring_permission,
        is_self_injected_cg_user_data,
    };
    use super::super::protocol::KvmPacket;
    use super::macos_logic::{
        cg_button_to_kvm, is_emergency_release_combo, is_point_on_local_display,
        macos_cg_keycode_to_win_key, MacTapDeltaTracker, MacTapModifierTracker,
        MacTapScrollAccumulator, ModifierAction, KCG_EVENT_FLAGS_CHANGED, KCG_EVENT_KEY_DOWN,
        KCG_EVENT_KEY_UP, KCG_EVENT_LEFT_MOUSE_DOWN, KCG_EVENT_LEFT_MOUSE_DRAGGED,
        KCG_EVENT_LEFT_MOUSE_UP, KCG_EVENT_MOUSE_MOVED, KCG_EVENT_OTHER_MOUSE_DOWN,
        KCG_EVENT_OTHER_MOUSE_DRAGGED, KCG_EVENT_OTHER_MOUSE_UP, KCG_EVENT_RIGHT_MOUSE_DOWN,
        KCG_EVENT_RIGHT_MOUSE_DRAGGED, KCG_EVENT_RIGHT_MOUSE_UP, KCG_EVENT_SCROLL_WHEEL,
        KCG_EVENT_TAP_DISABLED_BY_TIMEOUT, KCG_EVENT_TAP_DISABLED_BY_USER_INPUT, KCG_FLAG_CONTROL,
        KCG_FLAG_OPTION,
    };
    use super::{
        controller_tracker, disarm_trapped_in_callback, finalize_engage_leap, local_monitors,
        mouse_delta_sender, release_controller_held_keys, should_perform_engage,
        try_evaluate_edge_leap, try_send_packet, CLAMP_CENTER_X, CLAMP_CENTER_Y, CURSOR_CENTERED,
        EDGE_DWELL_SEAM_IDX, EDGE_DWELL_START, ENGAGE_PENDING, HAS_ACTIVE_LINK, HOOK_ACTIVE,
        IS_TRAPPED, KEY_ALT_DOWN, KEY_CTRL_DOWN, MOUSE_BUTTONS_DOWN, PENDING_ENGAGE_REL_Y,
    };
    use std::ffi::c_void;
    use std::ptr;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{mpsc, Mutex, OnceLock};
    use std::time::Instant;

    type CFTypeRef = *const c_void;
    type CFAllocatorRef = *const c_void;
    type CFRunLoopRef = *mut c_void;
    type CFRunLoopSourceRef = *mut c_void;
    type CFRunLoopMode = *const c_void;
    type CFMachPortRef = *mut c_void;
    type CGEventTapProxy = *mut c_void;
    type CGEventRef = *mut c_void;

    const K_CG_SESSION_EVENT_TAP: u32 = 1;
    const K_CG_HEAD_INSERT_EVENT_TAP: u32 = 0;
    const K_CG_EVENT_TAP_OPTION_DEFAULT: u32 = 0;
    const K_CG_MOUSE_EVENT_BUTTON_NUMBER: u32 = 3;
    const K_CG_KEYBOARD_EVENT_KEYCODE: u32 = 9;
    const K_CG_SCROLL_WHEEL_EVENT_DELTA_AXIS1: u32 = 11;
    const K_CG_EVENT_SOURCE_USER_DATA: u32 = 42;
    const K_CG_SCROLL_WHEEL_EVENT_IS_CONTINUOUS: u32 = 88;
    const K_CG_SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS1: u32 = 96;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGPoint {
        x: f64,
        y: f64,
    }

    #[repr(C)]
    struct CFRunLoopSourceContext {
        version: isize,
        info: *mut c_void,
        retain: Option<extern "C" fn(*const c_void) -> *const c_void>,
        release: Option<extern "C" fn(*const c_void)>,
        copy_description: Option<extern "C" fn(*const c_void) -> *const c_void>,
        equal: Option<extern "C" fn(*const c_void, *const c_void) -> u8>,
        hash: Option<extern "C" fn(*const c_void) -> usize>,
        schedule: Option<extern "C" fn(*mut c_void, CFRunLoopRef, CFRunLoopMode)>,
        cancel: Option<extern "C" fn(*mut c_void, CFRunLoopRef, CFRunLoopMode)>,
        perform: Option<extern "C" fn(*mut c_void)>,
    }

    type CGEventTapCallBack = unsafe extern "C" fn(
        proxy: CGEventTapProxy,
        event_type: u32,
        event: CGEventRef,
        user_info: *mut c_void,
    ) -> CGEventRef;

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        static kCFRunLoopCommonModes: CFRunLoopMode;
        fn CFRelease(cf: CFTypeRef);
        fn CFMachPortInvalidate(port: CFMachPortRef);
        fn CFMachPortCreateRunLoopSource(
            allocator: CFAllocatorRef,
            port: CFMachPortRef,
            order: isize,
        ) -> CFRunLoopSourceRef;
        fn CFRunLoopGetCurrent() -> CFRunLoopRef;
        fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFRunLoopMode);
        fn CFRunLoopRemoveSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFRunLoopMode);
        fn CFRunLoopRun();
        fn CFRunLoopStop(rl: CFRunLoopRef);
        fn CFRunLoopWakeUp(rl: CFRunLoopRef);
        fn CFRunLoopSourceCreate(
            allocator: CFAllocatorRef,
            order: isize,
            context: *mut CFRunLoopSourceContext,
        ) -> CFRunLoopSourceRef;
        fn CFRunLoopSourceSignal(source: CFRunLoopSourceRef);
        fn CFRunLoopSourceInvalidate(source: CFRunLoopSourceRef);
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGEventTapCreate(
            tap: u32,
            place: u32,
            options: u32,
            events_of_interest: u64,
            callback: CGEventTapCallBack,
            user_info: *mut c_void,
        ) -> CFMachPortRef;
        fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
        fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
        fn CGEventGetFlags(event: CGEventRef) -> u64;
        fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
        fn CGMainDisplayID() -> u32;
        fn CGDisplayHideCursor(display: u32) -> i32;
        fn CGDisplayShowCursor(display: u32) -> i32;
        fn CGWarpMouseCursorPosition(new_cursor_position: CGPoint) -> i32;
    }

    struct TapHandle {
        run_loop: usize,
        tap_port: usize,
        engage_src: usize,
        restore_src: usize,
    }

    static TAP_HANDLE: Mutex<Option<TapHandle>> = Mutex::new(None);
    static MACOS_CURSOR_HIDDEN: AtomicBool = AtomicBool::new(false);
    static PENDING_RETURN_WARP: Mutex<Option<(i32, i32)>> = Mutex::new(None);
    static LEAP_EDGE_X_BITS: AtomicU64 = AtomicU64::new(0);
    static LEAP_EDGE_Y_BITS: AtomicU64 = AtomicU64::new(0);
    static ENGAGE_TIMESTAMP: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    static DELTA_TRACKER: OnceLock<Mutex<MacTapDeltaTracker>> = OnceLock::new();
    static MOD_TRACKER: OnceLock<Mutex<MacTapModifierTracker>> = OnceLock::new();
    static SCROLL_ACC: OnceLock<Mutex<MacTapScrollAccumulator>> = OnceLock::new();

    fn engage_timestamp() -> &'static Mutex<Option<Instant>> {
        ENGAGE_TIMESTAMP.get_or_init(|| Mutex::new(None))
    }

    fn delta_tracker() -> &'static Mutex<MacTapDeltaTracker> {
        DELTA_TRACKER.get_or_init(|| Mutex::new(MacTapDeltaTracker::default()))
    }

    fn mod_tracker() -> &'static Mutex<MacTapModifierTracker> {
        MOD_TRACKER.get_or_init(|| Mutex::new(MacTapModifierTracker::default()))
    }

    fn scroll_acc() -> &'static Mutex<MacTapScrollAccumulator> {
        SCROLL_ACC.get_or_init(|| Mutex::new(MacTapScrollAccumulator::default()))
    }

    fn source_context(perform: extern "C" fn(*mut c_void)) -> CFRunLoopSourceContext {
        CFRunLoopSourceContext {
            version: 0,
            info: ptr::null_mut(),
            retain: None,
            release: None,
            copy_description: None,
            equal: None,
            hash: None,
            schedule: None,
            cancel: None,
            perform: Some(perform),
        }
    }

    pub fn restore_cursor_direct() {
        if MACOS_CURSOR_HIDDEN.swap(false, Ordering::AcqRel) {
            unsafe {
                let _ = CGDisplayShowCursor(CGMainDisplayID());
            }
        }
        if let Ok(mut dt) = delta_tracker().try_lock() {
            dt.reset();
        }
    }

    extern "C" fn perform_engage_source(_info: *mut c_void) {
        if !should_perform_engage(
            HOOK_ACTIVE.load(Ordering::Relaxed),
            IS_TRAPPED.load(Ordering::Acquire),
            ENGAGE_PENDING.swap(false, Ordering::AcqRel),
        ) {
            return;
        }
        let log_enabled = debug_log::is_enabled();
        let center_x = CLAMP_CENTER_X.load(Ordering::Relaxed);
        let center_y = CLAMP_CENTER_Y.load(Ordering::Relaxed);
        let leap_x = f64::from_bits(LEAP_EDGE_X_BITS.load(Ordering::Acquire));
        let leap_y = f64::from_bits(LEAP_EDGE_Y_BITS.load(Ordering::Acquire));

        if let Ok(mut dt) = delta_tracker().lock() {
            dt.engage((center_x as f64, center_y as f64), (leap_x, leap_y));
        }
        if let Ok(mut ts) = engage_timestamp().lock() {
            *ts = Some(Instant::now());
        }
        unsafe {
            if !MACOS_CURSOR_HIDDEN.swap(true, Ordering::AcqRel) {
                let _ = CGDisplayHideCursor(CGMainDisplayID());
            }
            let _ = CGWarpMouseCursorPosition(CGPoint {
                x: center_x as f64,
                y: center_y as f64,
            });
        }
        CURSOR_CENTERED.store(true, Ordering::Release);

        let (tracker_before, released, send_input_us) = release_controller_held_keys(log_enabled);
        if let Ok(mut mt) = mod_tracker().lock() {
            mt.clear();
        }
        let relative_y = f32::from_bits(PENDING_ENGAGE_REL_Y.load(Ordering::Relaxed));
        finalize_engage_leap(
            relative_y,
            0,
            log_enabled,
            tracker_before,
            released,
            send_input_us,
        );
    }

    extern "C" fn perform_restore_source(_info: *mut c_void) {
        restore_cursor_direct();
        let warp = PENDING_RETURN_WARP.lock().ok().and_then(|mut g| g.take());
        if let Some((rx, ry)) = warp {
            unsafe {
                let _ = CGWarpMouseCursorPosition(CGPoint {
                    x: rx as f64,
                    y: ry as f64,
                });
            }
        }
    }

    pub fn signal_engage() {
        if let Ok(guard) = TAP_HANDLE.try_lock() {
            if let Some(ref h) = *guard {
                unsafe {
                    CFRunLoopSourceSignal(h.engage_src as CFRunLoopSourceRef);
                    CFRunLoopWakeUp(h.run_loop as CFRunLoopRef);
                }
            }
        }
    }

    pub fn signal_restore() {
        if let Ok(guard) = TAP_HANDLE.try_lock() {
            if let Some(ref h) = *guard {
                unsafe {
                    CFRunLoopSourceSignal(h.restore_src as CFRunLoopSourceRef);
                    CFRunLoopWakeUp(h.run_loop as CFRunLoopRef);
                }
            }
        }
    }

    pub fn schedule_return_warp(rx: i32, ry: i32) {
        if let Ok(mut g) = PENDING_RETURN_WARP.lock() {
            *g = Some((rx, ry));
        }
        signal_restore();
    }

    unsafe extern "C" fn event_tap_callback(
        _proxy: CGEventTapProxy,
        event_type: u32,
        event: CGEventRef,
        _user_info: *mut c_void,
    ) -> CGEventRef {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            handle_tap_event(event_type, event)
        }));
        match outcome {
            Ok(res) => res,
            Err(_) => {
                disarm_trapped_in_callback();
                if let Ok(mut dt) = delta_tracker().try_lock() {
                    dt.reset();
                }
                debug_log::record_event(DebugEvent::Release {
                    ts_us: debug_log::now_us(),
                    reason: "macos_tap_panic",
                });
                signal_restore();
                event
            }
        }
    }

    unsafe fn handle_tap_event(event_type: u32, event: CGEventRef) -> CGEventRef {
        if event_type == KCG_EVENT_TAP_DISABLED_BY_TIMEOUT
            || event_type == KCG_EVENT_TAP_DISABLED_BY_USER_INPUT
        {
            if HOOK_ACTIVE.load(Ordering::Acquire) {
                if let Ok(guard) = TAP_HANDLE.try_lock() {
                    if let Some(ref h) = *guard {
                        CGEventTapEnable(h.tap_port as CFMachPortRef, true);
                    }
                }
            }
            return event;
        }

        if event.is_null() || !HOOK_ACTIVE.load(Ordering::Relaxed) {
            return event;
        }

        let user_data = CGEventGetIntegerValueField(event, K_CG_EVENT_SOURCE_USER_DATA);
        if is_self_injected_cg_user_data(user_data) {
            return event;
        }

        let button_number = CGEventGetIntegerValueField(event, K_CG_MOUSE_EVENT_BUTTON_NUMBER);
        if let Some((btn, is_down)) = cg_button_to_kvm(event_type, button_number) {
            let bit = 1u8 << (btn - 1);
            if is_down {
                MOUSE_BUTTONS_DOWN.fetch_or(bit, Ordering::Relaxed);
            } else {
                MOUSE_BUTTONS_DOWN.fetch_and(!bit, Ordering::Relaxed);
            }
        }

        let trapped = IS_TRAPPED.load(Ordering::Acquire);

        if event_type == KCG_EVENT_FLAGS_CHANGED {
            let cg_code = CGEventGetIntegerValueField(event, K_CG_KEYBOARD_EVENT_KEYCODE) as u16;
            let raw_flags = CGEventGetFlags(event);
            KEY_CTRL_DOWN.store((raw_flags & KCG_FLAG_CONTROL) != 0, Ordering::Relaxed);
            KEY_ALT_DOWN.store((raw_flags & KCG_FLAG_OPTION) != 0, Ordering::Relaxed);

            let action = mod_tracker()
                .try_lock()
                .map(|mut m| m.on_flags_changed(cg_code, raw_flags))
                .unwrap_or(ModifierAction::None);

            if trapped {
                match action {
                    ModifierAction::Key {
                        vk,
                        scan,
                        is_down,
                        extended,
                    } => {
                        try_send_packet(KvmPacket::KeyEvent {
                            key_code: vk,
                            scan_code: scan,
                            is_down,
                            extended,
                        });
                    }
                    ModifierAction::TogglePulse { vk, scan } => {
                        try_send_packet(KvmPacket::KeyEvent {
                            key_code: vk,
                            scan_code: scan,
                            is_down: true,
                            extended: false,
                        });
                        try_send_packet(KvmPacket::KeyEvent {
                            key_code: vk,
                            scan_code: scan,
                            is_down: false,
                            extended: false,
                        });
                    }
                    ModifierAction::None => {}
                }
                return ptr::null_mut();
            }

            if let ModifierAction::Key {
                vk,
                scan,
                is_down,
                extended,
            } = action
            {
                if let Ok(mut tracker) = controller_tracker().try_lock() {
                    tracker.record_key(vk, scan, is_down, extended, false);
                }
            }
            return event;
        }

        if event_type == KCG_EVENT_KEY_DOWN || event_type == KCG_EVENT_KEY_UP {
            let is_down = event_type == KCG_EVENT_KEY_DOWN;
            let cg_code = CGEventGetIntegerValueField(event, K_CG_KEYBOARD_EVENT_KEYCODE) as u16;
            let Some((vk_code, scan_code, extended)) = macos_cg_keycode_to_win_key(cg_code) else {
                return if trapped { ptr::null_mut() } else { event };
            };
            let log_enabled = debug_log::is_enabled();
            let ts_us = if log_enabled { debug_log::now_us() } else { 0 };

            let ctrl_down = KEY_CTRL_DOWN.load(Ordering::Relaxed);
            let opt_down = KEY_ALT_DOWN.load(Ordering::Relaxed);
            if is_down && is_emergency_release_combo(vk_code, ctrl_down, opt_down) {
                disarm_trapped_in_callback();
                KEY_CTRL_DOWN.store(false, Ordering::Relaxed);
                KEY_ALT_DOWN.store(false, Ordering::Relaxed);
                if let Ok(mut mt) = mod_tracker().try_lock() {
                    mt.clear();
                }
                if let Ok(mut dt) = delta_tracker().try_lock() {
                    dt.reset();
                }
                if let Ok(mut tracker) = controller_tracker().try_lock() {
                    let _ = tracker.release_all();
                }
                if log_enabled {
                    debug_log::record_event(DebugEvent::Key {
                        ts_us,
                        vk: vk_code,
                        scan_code,
                        is_down,
                        extended,
                        injected: false,
                        is_trapped: trapped,
                        decision: KeyDecision::Block,
                        hwnd: 0,
                    });
                    debug_log::record_event(DebugEvent::Release {
                        ts_us,
                        reason: "emergency_hotkey",
                    });
                }
                signal_restore();
                try_send_packet(KvmPacket::EmergencyRelease);
                try_send_packet(KvmPacket::ReleaseAll);
                return ptr::null_mut();
            }

            if trapped {
                if log_enabled {
                    debug_log::record_event(DebugEvent::Key {
                        ts_us,
                        vk: vk_code,
                        scan_code,
                        is_down,
                        extended,
                        injected: false,
                        is_trapped: true,
                        decision: KeyDecision::Forward,
                        hwnd: 0,
                    });
                }
                try_send_packet(KvmPacket::KeyEvent {
                    key_code: vk_code,
                    scan_code,
                    is_down,
                    extended,
                });
                return ptr::null_mut();
            }

            if log_enabled {
                debug_log::record_event(DebugEvent::Key {
                    ts_us,
                    vk: vk_code,
                    scan_code,
                    is_down,
                    extended,
                    injected: false,
                    is_trapped: false,
                    decision: KeyDecision::Pass,
                    hwnd: 0,
                });
            }
            if let Ok(mut tracker) = controller_tracker().try_lock() {
                tracker.record_key(vk_code, scan_code, is_down, extended, false);
            }
            return event;
        }

        if trapped {
            match event_type {
                KCG_EVENT_MOUSE_MOVED
                | KCG_EVENT_LEFT_MOUSE_DRAGGED
                | KCG_EVENT_RIGHT_MOUSE_DRAGGED
                | KCG_EVENT_OTHER_MOUSE_DRAGGED => {
                    if CURSOR_CENTERED.load(Ordering::Acquire) {
                        let pt_f = CGEventGetLocation(event);
                        let elapsed = engage_timestamp()
                            .try_lock()
                            .ok()
                            .and_then(|g| g.map(|t| t.elapsed().as_millis() as u64))
                            .unwrap_or(100);
                        if let Ok(mut dt) = delta_tracker().try_lock() {
                            if let Some((dx, dy, should_warp)) =
                                dt.on_moved((pt_f.x, pt_f.y), elapsed)
                            {
                                if should_warp {
                                    let _ = CGWarpMouseCursorPosition(CGPoint {
                                        x: CLAMP_CENTER_X.load(Ordering::Relaxed) as f64,
                                        y: CLAMP_CENTER_Y.load(Ordering::Relaxed) as f64,
                                    });
                                }
                                if dx != 0 || dy != 0 {
                                    if let Ok(guard) = mouse_delta_sender().try_lock() {
                                        if let Some(ref tx) = *guard {
                                            let _ = tx.try_send((dx, dy));
                                        }
                                    }
                                }
                            }
                        }
                    }
                    return ptr::null_mut();
                }
                KCG_EVENT_LEFT_MOUSE_DOWN
                | KCG_EVENT_LEFT_MOUSE_UP
                | KCG_EVENT_RIGHT_MOUSE_DOWN
                | KCG_EVENT_RIGHT_MOUSE_UP
                | KCG_EVENT_OTHER_MOUSE_DOWN
                | KCG_EVENT_OTHER_MOUSE_UP => {
                    if let Some((button, is_down)) = cg_button_to_kvm(event_type, button_number) {
                        try_send_packet(KvmPacket::MouseButton { button, is_down });
                    }
                    return ptr::null_mut();
                }
                KCG_EVENT_SCROLL_WHEEL => {
                    let is_continuous =
                        CGEventGetIntegerValueField(event, K_CG_SCROLL_WHEEL_EVENT_IS_CONTINUOUS)
                            != 0;
                    let pt_delta = CGEventGetIntegerValueField(
                        event,
                        K_CG_SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS1,
                    );
                    let disc_delta =
                        CGEventGetIntegerValueField(event, K_CG_SCROLL_WHEEL_EVENT_DELTA_AXIS1);
                    let delta = scroll_acc()
                        .try_lock()
                        .map(|mut s| s.push(is_continuous, pt_delta, disc_delta))
                        .unwrap_or(0);
                    if delta != 0 {
                        try_send_packet(KvmPacket::MouseWheel { delta });
                    }
                    return ptr::null_mut();
                }
                _ => return event,
            }
        }

        if event_type != KCG_EVENT_MOUSE_MOVED || !HAS_ACTIVE_LINK.load(Ordering::Acquire) {
            return event;
        }

        if MOUSE_BUTTONS_DOWN.load(Ordering::Relaxed) != 0 {
            EDGE_DWELL_START.store(0, Ordering::Relaxed);
            EDGE_DWELL_SEAM_IDX.store(-1, Ordering::Relaxed);
            return event;
        }

        let pt_f = CGEventGetLocation(event);
        LEAP_EDGE_X_BITS.store(pt_f.x.to_bits(), Ordering::Release);
        LEAP_EDGE_Y_BITS.store(pt_f.y.to_bits(), Ordering::Release);
        let pt_i = (pt_f.x.round() as i32, pt_f.y.round() as i32);
        if try_evaluate_edge_leap(pt_i, |neighbor| {
            local_monitors()
                .try_lock()
                .is_ok_and(|mons| is_point_on_local_display(neighbor, &mons))
        }) {
            return ptr::null_mut();
        }

        event
    }

    fn run_tap_loop(ready_tx: mpsc::SyncSender<bool>) {
        let mask: u64 = (1u64 << KCG_EVENT_LEFT_MOUSE_DOWN)
            | (1u64 << KCG_EVENT_LEFT_MOUSE_UP)
            | (1u64 << KCG_EVENT_RIGHT_MOUSE_DOWN)
            | (1u64 << KCG_EVENT_RIGHT_MOUSE_UP)
            | (1u64 << KCG_EVENT_MOUSE_MOVED)
            | (1u64 << KCG_EVENT_LEFT_MOUSE_DRAGGED)
            | (1u64 << KCG_EVENT_RIGHT_MOUSE_DRAGGED)
            | (1u64 << KCG_EVENT_KEY_DOWN)
            | (1u64 << KCG_EVENT_KEY_UP)
            | (1u64 << KCG_EVENT_FLAGS_CHANGED)
            | (1u64 << KCG_EVENT_SCROLL_WHEEL)
            | (1u64 << KCG_EVENT_OTHER_MOUSE_DOWN)
            | (1u64 << KCG_EVENT_OTHER_MOUSE_UP)
            | (1u64 << KCG_EVENT_OTHER_MOUSE_DRAGGED);

        unsafe {
            let tap = CGEventTapCreate(
                K_CG_SESSION_EVENT_TAP,
                K_CG_HEAD_INSERT_EVENT_TAP,
                K_CG_EVENT_TAP_OPTION_DEFAULT,
                mask,
                event_tap_callback,
                ptr::null_mut(),
            );
            if tap.is_null() {
                let _ = ready_tx.send(false);
                return;
            }

            let tap_src = CFMachPortCreateRunLoopSource(ptr::null(), tap, 0);
            let mut engage_ctx = source_context(perform_engage_source);
            let engage_src = CFRunLoopSourceCreate(ptr::null(), 0, &mut engage_ctx);
            let mut restore_ctx = source_context(perform_restore_source);
            let restore_src = CFRunLoopSourceCreate(ptr::null(), 0, &mut restore_ctx);
            if tap_src.is_null() || engage_src.is_null() || restore_src.is_null() {
                if !tap_src.is_null() {
                    CFRelease(tap_src as CFTypeRef);
                }
                if !engage_src.is_null() {
                    CFRunLoopSourceInvalidate(engage_src);
                    CFRelease(engage_src as CFTypeRef);
                }
                if !restore_src.is_null() {
                    CFRunLoopSourceInvalidate(restore_src);
                    CFRelease(restore_src as CFTypeRef);
                }
                CFMachPortInvalidate(tap);
                CFRelease(tap as CFTypeRef);
                let _ = ready_tx.send(false);
                return;
            }

            let rl = CFRunLoopGetCurrent();
            CFRunLoopAddSource(rl, tap_src, kCFRunLoopCommonModes);
            CFRunLoopAddSource(rl, engage_src, kCFRunLoopCommonModes);
            CFRunLoopAddSource(rl, restore_src, kCFRunLoopCommonModes);
            CGEventTapEnable(tap, true);

            if let Ok(mut guard) = TAP_HANDLE.lock() {
                *guard = Some(TapHandle {
                    run_loop: rl as usize,
                    tap_port: tap as usize,
                    engage_src: engage_src as usize,
                    restore_src: restore_src as usize,
                });
            }

            KEY_CTRL_DOWN.store(false, Ordering::Relaxed);
            KEY_ALT_DOWN.store(false, Ordering::Relaxed);
            MOUSE_BUTTONS_DOWN.store(0, Ordering::Relaxed);
            if let Ok(mut mt) = mod_tracker().lock() {
                mt.clear();
            }
            if let Ok(mut dt) = delta_tracker().lock() {
                dt.reset();
            }
            HOOK_ACTIVE.store(true, Ordering::SeqCst);
            let _ = ready_tx.send(true);

            CFRunLoopRun();

            if let Ok(mut guard) = TAP_HANDLE.lock() {
                if guard.as_ref().is_some_and(|h| h.run_loop == rl as usize) {
                    *guard = None;
                }
            }

            CGEventTapEnable(tap, false);
            CFRunLoopRemoveSource(rl, tap_src, kCFRunLoopCommonModes);
            CFRunLoopRemoveSource(rl, engage_src, kCFRunLoopCommonModes);
            CFRunLoopRemoveSource(rl, restore_src, kCFRunLoopCommonModes);
            CFRunLoopSourceInvalidate(engage_src);
            CFRunLoopSourceInvalidate(restore_src);
            CFMachPortInvalidate(tap);
            CFRelease(tap_src as CFTypeRef);
            CFRelease(engage_src as CFTypeRef);
            CFRelease(restore_src as CFTypeRef);
            CFRelease(tap as CFTypeRef);
            restore_cursor_direct();
        }
    }

    pub fn start() -> bool {
        if HOOK_ACTIVE.load(Ordering::Acquire)
            && TAP_HANDLE.lock().ok().is_some_and(|g| g.is_some())
        {
            return true;
        }
        let prompt = !cfg!(test);
        if !check_accessibility_permission(prompt) {
            HOOK_ACTIVE.store(false, Ordering::SeqCst);
            HAS_ACTIVE_LINK.store(false, Ordering::Release);
            return false;
        }
        let (ready_tx, ready_rx) = mpsc::sync_channel::<bool>(1);
        if std::thread::Builder::new()
            .name("kvm-macos-event-tap".into())
            .spawn(move || run_tap_loop(ready_tx))
            .is_err()
        {
            HOOK_ACTIVE.store(false, Ordering::SeqCst);
            HAS_ACTIVE_LINK.store(false, Ordering::Release);
            return false;
        }
        let started = ready_rx.recv().unwrap_or(false);
        if !started {
            if prompt {
                let _ = check_input_monitoring_permission(true);
            }
            HOOK_ACTIVE.store(false, Ordering::SeqCst);
            HAS_ACTIVE_LINK.store(false, Ordering::Release);
        }
        started
    }

    pub fn stop() {
        if let Ok(guard) = TAP_HANDLE.lock() {
            if let Some(ref h) = *guard {
                unsafe {
                    CFRunLoopStop(h.run_loop as CFRunLoopRef);
                    CFRunLoopWakeUp(h.run_loop as CFRunLoopRef);
                }
            }
        }
        restore_cursor_direct();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_trap_center_handles_origin_and_offset_monitors() {
        assert_eq!(compute_trap_center(0, 0, 1920, 1080), (960, 540));
        assert_eq!(compute_trap_center(-1920, 100, 1920, 1080), (-960, 640));
    }

    #[test]
    fn trapped_mouse_delta_suppresses_until_centered_and_skips_zero() {
        assert_eq!(trapped_mouse_delta(false, (1920, 540), (960, 540)), None);
        assert_eq!(trapped_mouse_delta(true, (960, 540), (960, 540)), None);
        assert_eq!(
            trapped_mouse_delta(true, (975, 532), (960, 540)),
            Some((15, -8))
        );
    }

    #[test]
    fn should_perform_engage_requires_active_trapped_and_pending() {
        assert!(should_perform_engage(true, true, true));
        assert!(!should_perform_engage(false, true, true));
        assert!(!should_perform_engage(true, false, true));
        assert!(!should_perform_engage(true, true, false));
    }

    fn candidate(hwnd: isize, pid: u32, elevation: ProcessElevation) -> WindowCandidateInfo {
        WindowCandidateInfo {
            hwnd,
            pid,
            visible: true,
            cloaked: false,
            minimized: false,
            tool_window: false,
            elevation,
        }
    }

    #[test]
    fn select_next_handoff_target_skips_elevated_unknown_cloaked_and_ineligible_windows() {
        let our_pid = 1000;
        let standard = ProcessElevation::Standard {
            integrity_rid: SECURITY_MANDATORY_MEDIUM_RID,
        };
        let list = [
            candidate(0x10, our_pid, standard),
            WindowCandidateInfo {
                visible: false,
                ..candidate(0x20, 2000, standard)
            },
            WindowCandidateInfo {
                cloaked: true,
                ..candidate(0x25, 2001, standard)
            },
            WindowCandidateInfo {
                minimized: true,
                ..candidate(0x30, 2002, standard)
            },
            WindowCandidateInfo {
                tool_window: true,
                ..candidate(0x40, 2003, standard)
            },
            candidate(0x45, 2004, ProcessElevation::Elevated),
            candidate(0x46, 2005, ProcessElevation::Unknown),
            candidate(
                0x47,
                2006,
                ProcessElevation::Standard {
                    integrity_rid: 0x2100, // Medium Plus (UIAccess)
                },
            ),
            candidate(0x50, 3000, standard),
            candidate(0x60, 4000, standard),
        ];
        assert_eq!(select_next_handoff_target(&list, our_pid), Some(0x50));
    }

    #[test]
    fn select_next_handoff_target_returns_none_when_only_elevated_or_ineligible_exist() {
        let our_pid = 1000;
        let standard = ProcessElevation::Standard {
            integrity_rid: SECURITY_MANDATORY_MEDIUM_RID,
        };
        let list = [
            candidate(0, 2000, standard),
            candidate(0x10, our_pid, standard),
            candidate(0x20, 3000, ProcessElevation::Elevated),
            candidate(0x30, 4000, ProcessElevation::Unknown),
        ];
        assert_eq!(select_next_handoff_target(&list, our_pid), None);
    }

    #[test]
    fn select_preferred_handoff_targets_prefers_shell_first_and_skips_unsafe_shell() {
        let our_pid = 1000;
        let fg_hwnd = 0x05;
        let standard = ProcessElevation::Standard {
            integrity_rid: SECURITY_MANDATORY_MEDIUM_RID,
        };
        let shell = candidate(0x20, 1500, standard);
        let z_order = [
            candidate(0x10, 2000, ProcessElevation::Elevated),
            candidate(0x30, 3000, standard),
        ];
        assert_eq!(
            select_preferred_handoff_targets(Some(shell), &z_order, our_pid, fg_hwnd),
            vec![(HandoffMethod::Shell, 0x20), (HandoffMethod::Next, 0x30)]
        );

        let elevated_shell = candidate(0x20, 1500, ProcessElevation::Elevated);
        assert_eq!(
            select_preferred_handoff_targets(Some(elevated_shell), &z_order, our_pid, fg_hwnd),
            vec![(HandoffMethod::Next, 0x30)]
        );
    }

    #[test]
    fn pre_leap_guard_blocks_elevated_and_unknown_when_not_elevated() {
        let med = ProcessElevation::Standard {
            integrity_rid: SECURITY_MANDATORY_MEDIUM_RID,
        };
        let low = ProcessElevation::Standard {
            integrity_rid: 0x1000,
        };
        let med_plus = ProcessElevation::Standard {
            integrity_rid: 0x2100,
        };

        assert!(!should_block_leap_for_foreground(med, med));
        assert!(!should_block_leap_for_foreground(med, low));
        assert!(should_block_leap_for_foreground(med, med_plus));
        assert!(should_block_leap_for_foreground(
            med,
            ProcessElevation::Elevated
        ));
        assert!(should_block_leap_for_foreground(
            med,
            ProcessElevation::Unknown
        ));

        // If AnyLeap itself is elevated, admin foreground windows do not blind hooks.
        assert!(!should_block_leap_for_foreground(
            ProcessElevation::Elevated,
            ProcessElevation::Elevated
        ));
        assert!(!should_block_leap_for_foreground(
            ProcessElevation::Elevated,
            ProcessElevation::Unknown
        ));
    }

    #[test]
    fn capture_watchdog_ignores_idle_and_detects_blinded_input() {
        let th = WATCHDOG_BLIND_THRESHOLD_MS;
        assert_eq!(evaluate_capture_watchdog(false, Some(1000), 5000, th), None);
        assert_eq!(evaluate_capture_watchdog(true, None, 5000, th), None);

        // Normal idle: GetLastInputInfo().dwTime remains at last hook timestamp.
        assert_eq!(
            evaluate_capture_watchdog(true, Some(10_000), 10_000, th),
            None
        );
        // Hook tick recorded right after GetLastInputInfo() updated.
        assert_eq!(
            evaluate_capture_watchdog(true, Some(10_005), 10_000, th),
            None
        );
        // Boundary: exactly 1000 ms is not > 1000 ms.
        assert_eq!(
            evaluate_capture_watchdog(true, Some(10_000), 11_000, th),
            None
        );

        // System saw input > 1000 ms after last hook callback -> trigger release.
        assert_eq!(
            evaluate_capture_watchdog(true, Some(10_000), 11_001, th),
            Some(1001)
        );
        assert_eq!(
            evaluate_capture_watchdog(true, Some(10_000), 12_350, th),
            Some(2350)
        );

        // 32-bit GetTickCount wraparound safety.
        assert_eq!(
            evaluate_capture_watchdog(true, Some(u32::MAX - 400), 700, th),
            Some(1101)
        );
        assert_eq!(
            evaluate_capture_watchdog(true, Some(10), u32::MAX - 5, th),
            None
        );
    }

    #[test]
    fn timer_blocked_notice_checks_conditions_and_5s_throttle() {
        let cd = TIMER_BLOCKED_NOTICE_COOLDOWN_MS;
        assert_eq!(cd, 5000);

        // First trigger emits even when now_ms < cooldown (last_emitted_ms == 0).
        assert!(should_emit_timer_blocked_notice(
            true, false, true, true, 250, 0, cd
        ));

        // Throttled for 5 seconds after last emission.
        assert!(!should_emit_timer_blocked_notice(
            true, false, true, true, 500, 250, cd
        ));
        assert!(!should_emit_timer_blocked_notice(
            true, false, true, true, 5249, 250, cd
        ));

        // Emits again once 5000 ms have elapsed.
        assert!(should_emit_timer_blocked_notice(
            true, false, true, true, 5250, 250, cd
        ));

        // Suppressed if link inactive, already trapped, foreground not elevated, or cursor not at edge.
        assert!(!should_emit_timer_blocked_notice(
            false, false, true, true, 10_000, 0, cd
        ));
        assert!(!should_emit_timer_blocked_notice(
            true, true, true, true, 10_000, 0, cd
        ));
        assert!(!should_emit_timer_blocked_notice(
            true, false, false, true, 10_000, 0, cd
        ));
        assert!(!should_emit_timer_blocked_notice(
            true, false, true, false, 10_000, 0, cd
        ));
    }

    // --- SPEC-25a ---
    #[test]
    fn drag_leap_edge_helpers_and_injected_guard() {
        assert_eq!(drag_leap_edge_tolerance(2), 4);
        assert_eq!(drag_leap_edge_tolerance(0), 4);

        const LLMHF_INJECTED: u32 = 1;
        assert!(should_ignore_injected_mouse_hook_event(
            LLMHF_INJECTED,
            true,
            false
        ));
        assert!(!should_ignore_injected_mouse_hook_event(0, true, false));
        assert!(!should_ignore_injected_mouse_hook_event(
            LLMHF_INJECTED,
            false,
            false
        ));
        assert!(!should_ignore_injected_mouse_hook_event(
            LLMHF_INJECTED,
            true,
            true
        ));

        assert!(is_physical_mouse_hook_event(0));
        assert!(!is_physical_mouse_hook_event(LLMHF_INJECTED));
        assert!(!is_physical_mouse_hook_event(0x0000_0002));
        assert!(!is_physical_mouse_hook_event(0x0000_0003));

        assert_eq!(button_from_mouse_up_msg(0x0202), Some(1)); // WM_LBUTTONUP
        assert_eq!(button_from_mouse_up_msg(0x0205), Some(2)); // WM_RBUTTONUP
        assert_eq!(button_from_mouse_up_msg(0x0208), Some(3)); // WM_MBUTTONUP
        assert_eq!(button_from_mouse_up_msg(0x0200), None);

        assert!(!HookController::can_engage_drag_leap(
            false, true, false, true, 1
        ));
        assert!(HookController::can_engage_drag_leap(
            true, true, false, true, 1
        ));
        assert!(!HookController::can_engage_drag_leap(
            true, true, true, true, 1
        ));
        assert!(!HookController::can_engage_drag_leap(
            true, true, false, true, 0
        ));
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    #[test]
    fn start_hooks_on_non_windows_leaves_hooks_inactive() {
        let controller = HookController::new();
        let (pkt_tx, _pkt_rx) = mpsc::channel(4);
        let (delta_tx, _delta_rx) = mpsc::channel(4);
        let started = controller.start_hooks(TopologyConfig::default(), pkt_tx, delta_tx, None);
        assert!(!started);
        assert!(!controller.is_active());
        assert_eq!(controller.active_link_edge(), None);
        controller.stop_hooks();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_restore_cursor_and_stop_without_active_tap_are_safe() {
        let controller = HookController::new();
        restore_cursor_visibility();
        controller.stop_hooks();
        assert!(!controller.is_active());
        assert!(!controller.is_trapped());
    }

    #[test]
    fn macos_reverse_keymap_round_trips_spec28_with_set1_scancodes() {
        use macos_logic::macos_cg_keycode_to_win_key;

        // Letters, digits, return vs keypad enter, backspace vs forward delete
        assert_eq!(macos_cg_keycode_to_win_key(0x00), Some((0x41, 0x1E, false))); // A
        assert_eq!(macos_cg_keycode_to_win_key(0x06), Some((0x5A, 0x2C, false))); // Z
        assert_eq!(macos_cg_keycode_to_win_key(0x1D), Some((0x30, 0x0B, false))); // 0
        assert_eq!(macos_cg_keycode_to_win_key(0x24), Some((0x0D, 0x1C, false))); // Return
        assert_eq!(macos_cg_keycode_to_win_key(0x4C), Some((0x0D, 0x1C, true))); // KeypadEnter
        assert_eq!(macos_cg_keycode_to_win_key(0x33), Some((0x08, 0x0E, false))); // Backspace
        assert_eq!(macos_cg_keycode_to_win_key(0x75), Some((0x2E, 0x53, true))); // ForwardDelete
        assert_eq!(macos_cg_keycode_to_win_key(0x7B), Some((0x25, 0x4B, true))); // Left
        assert_eq!(macos_cg_keycode_to_win_key(0x7E), Some((0x26, 0x48, true))); // Up

        // 1:1 modifiers with accurate Set 1 scancodes + extended
        assert_eq!(macos_cg_keycode_to_win_key(0x38), Some((0xA0, 0x2A, false))); // LShift
        assert_eq!(macos_cg_keycode_to_win_key(0x3C), Some((0xA1, 0x36, false))); // RShift
        assert_eq!(macos_cg_keycode_to_win_key(0x3B), Some((0xA2, 0x1D, false))); // LControl
        assert_eq!(macos_cg_keycode_to_win_key(0x3E), Some((0xA3, 0x1D, true))); // RControl
        assert_eq!(macos_cg_keycode_to_win_key(0x3A), Some((0xA4, 0x38, false))); // LOption -> LAlt
        assert_eq!(macos_cg_keycode_to_win_key(0x3D), Some((0xA5, 0x38, true))); // ROption -> RAlt
        assert_eq!(macos_cg_keycode_to_win_key(0x37), Some((0x5B, 0x5B, true))); // LCommand -> LWin
        assert_eq!(macos_cg_keycode_to_win_key(0x36), Some((0x5C, 0x5C, true))); // RCommand -> RWin
        assert_eq!(macos_cg_keycode_to_win_key(0x39), Some((0x14, 0x3A, false))); // CapsLock
        assert_eq!(macos_cg_keycode_to_win_key(0x69), Some((0x7C, 0x00, false))); // F13 uses VK

        // Unknown / consumer code
        assert_eq!(macos_cg_keycode_to_win_key(0xFF), None);
    }

    #[test]
    fn macos_modifier_tracker_handles_sides_fallback_capslock_and_emergency() {
        use macos_logic::{
            is_emergency_release_combo, MacTapModifierTracker, ModifierAction,
            KCG_FLAG_ALPHA_SHIFT, KCG_FLAG_CONTROL, KCG_FLAG_DEV_LCTRL, KCG_FLAG_DEV_LOPT,
            KCG_FLAG_DEV_LSHIFT, KCG_FLAG_DEV_RSHIFT, KCG_FLAG_OPTION, KCG_FLAG_SHIFT,
        };

        let mut mods = MacTapModifierTracker::default();
        // LShift down with device mask
        assert_eq!(
            mods.on_flags_changed(0x38, KCG_FLAG_SHIFT | KCG_FLAG_DEV_LSHIFT),
            ModifierAction::Key {
                vk: 0xA0,
                scan: 0x2A,
                is_down: true,
                extended: false,
            }
        );
        // RShift down while LShift stays down
        assert_eq!(
            mods.on_flags_changed(
                0x3C,
                KCG_FLAG_SHIFT | KCG_FLAG_DEV_LSHIFT | KCG_FLAG_DEV_RSHIFT
            ),
            ModifierAction::Key {
                vk: 0xA1,
                scan: 0x36,
                is_down: true,
                extended: false,
            }
        );
        // Release LShift only -> RShift stays down!
        assert_eq!(
            mods.on_flags_changed(0x38, KCG_FLAG_SHIFT | KCG_FLAG_DEV_RSHIFT),
            ModifierAction::Key {
                vk: 0xA0,
                scan: 0x2A,
                is_down: false,
                extended: false,
            }
        );
        // Release RShift via generic-flag fallback (no lower device bits)
        assert_eq!(
            mods.on_flags_changed(0x3C, 0),
            ModifierAction::Key {
                vk: 0xA1,
                scan: 0x36,
                is_down: false,
                extended: false,
            }
        );

        // CapsLock AlphaShift flip emits toggle pulse
        assert_eq!(
            mods.on_flags_changed(0x39, KCG_FLAG_ALPHA_SHIFT),
            ModifierAction::TogglePulse {
                vk: 0x14,
                scan: 0x3A,
            }
        );
        assert_eq!(
            mods.on_flags_changed(0x39, KCG_FLAG_ALPHA_SHIFT),
            ModifierAction::None
        );
        assert_eq!(
            mods.on_flags_changed(0x39, 0),
            ModifierAction::TogglePulse {
                vk: 0x14,
                scan: 0x3A,
            }
        );

        // Ctrl + Option + Esc emergency release vs Scroll/Pause
        assert_eq!(
            mods.on_flags_changed(0x3B, KCG_FLAG_CONTROL | KCG_FLAG_DEV_LCTRL),
            ModifierAction::Key {
                vk: 0xA2,
                scan: 0x1D,
                is_down: true,
                extended: false,
            }
        );
        assert_eq!(
            mods.on_flags_changed(
                0x3A,
                KCG_FLAG_CONTROL | KCG_FLAG_DEV_LCTRL | KCG_FLAG_OPTION | KCG_FLAG_DEV_LOPT
            ),
            ModifierAction::Key {
                vk: 0xA4,
                scan: 0x38,
                is_down: true,
                extended: false,
            }
        );
        assert!(mods.ctrl_and_option_down());
        assert!(is_emergency_release_combo(0x1B, true, true));
        assert!(!is_emergency_release_combo(0x1B, false, true)); // Cmd+Opt+Esc does NOT trigger
        assert!(is_emergency_release_combo(0x91, false, false)); // Scroll Lock
        assert!(is_emergency_release_combo(0x13, false, false)); // Pause
    }

    #[test]
    fn macos_delta_tracker_drops_prewarp_edge_event_and_accumulates_fractions() {
        use macos_logic::MacTapDeltaTracker;

        let mut dt = MacTapDeltaTracker::default();
        dt.engage((864.0, 558.0), (1727.0, 500.0));

        // Pre-warp buffered edge event within 25ms is dropped cleanly
        assert_eq!(dt.on_moved((1727.0, 502.0), 5), None);

        // First settled sub-pixel micro moves accumulate cleanly without double counting
        assert_eq!(dt.on_moved((864.4, 558.4), 30), Some((0, 0, false)));
        assert_eq!(dt.on_moved((864.8, 558.8), 32), Some((0, 0, false)));
        // Third step reaches 1.2 cumulative and warps back to center
        assert_eq!(dt.on_moved((865.2, 559.2), 34), Some((1, 1, true)));
        // Next move relative to warped center carries 0.2 remainder
        assert_eq!(dt.on_moved((864.9, 558.9), 36), Some((1, 1, true)));
    }

    #[test]
    fn macos_scroll_and_button_mapping() {
        use macos_logic::{
            cg_button_to_kvm, is_point_on_local_display, MacTapScrollAccumulator,
            KCG_EVENT_LEFT_MOUSE_DOWN, KCG_EVENT_LEFT_MOUSE_UP, KCG_EVENT_OTHER_MOUSE_DOWN,
            KCG_EVENT_OTHER_MOUSE_UP, KCG_EVENT_RIGHT_MOUSE_DOWN,
        };

        let mut scroll = MacTapScrollAccumulator::default();
        assert_eq!(scroll.push(true, 10, 0), 30);
        assert_eq!(scroll.push(true, 40, 0), 120);
        assert_eq!(scroll.push(false, 0, 1), 120);
        assert_eq!(scroll.push(false, 0, -2), -240);

        assert_eq!(
            cg_button_to_kvm(KCG_EVENT_LEFT_MOUSE_DOWN, 0),
            Some((1, true))
        );
        assert_eq!(
            cg_button_to_kvm(KCG_EVENT_LEFT_MOUSE_UP, 0),
            Some((1, false))
        );
        assert_eq!(
            cg_button_to_kvm(KCG_EVENT_RIGHT_MOUSE_DOWN, 1),
            Some((2, true))
        );
        assert_eq!(
            cg_button_to_kvm(KCG_EVENT_OTHER_MOUSE_DOWN, 2),
            Some((3, true))
        );
        assert_eq!(
            cg_button_to_kvm(KCG_EVENT_OTHER_MOUSE_UP, 2),
            Some((3, false))
        );
        assert_eq!(cg_button_to_kvm(KCG_EVENT_OTHER_MOUSE_DOWN, 3), None);

        let mons = vec![MonitorDisplay {
            id: "cg-1".into(),
            device_name: "cg-1".into(),
            x: 0,
            y: 0,
            width: 1728,
            height: 1117,
            is_primary: true,
        }];
        assert!(is_point_on_local_display((1727, 500), &mons));
        assert!(!is_point_on_local_display((1728, 500), &mons));
    }
    // --- end SPEC-25a ---
}
