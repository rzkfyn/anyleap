#[cfg(windows)]
use windows::Win32::Foundation::{HANDLE, POINT};
#[cfg(windows)]
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, DESKTOP_ACCESS_FLAGS,
    DESKTOP_CONTROL_FLAGS, UOI_NAME,
};
#[cfg(windows)]
use windows::Win32::UI::Input::KeyboardAndMouse::*;
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorInfo, GetCursorPos, SetCursorPos, CURSORINFO, CURSOR_SHOWING, CURSOR_SUPPRESSED,
};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

pub const CURSOR_FLAG_SHOWING: u32 = 0x0000_0001;
pub const CURSOR_FLAG_SUPPRESSED: u32 = 0x0000_0002;
#[cfg(windows)]
const DESKTOP_READOBJECTS_ACCESS: u32 = 0x0001;

#[cfg(any(target_os = "macos", test))]
pub const ANYLEAP_CG_USER_DATA: i64 = 0x414E_594C_4B56_4D31;

#[cfg(any(target_os = "macos", test))]
pub fn is_self_injected_cg_user_data(user_data: i64) -> bool {
    user_data == ANYLEAP_CG_USER_DATA
}

/// Returns whether the process holds macOS Accessibility trust required for `CGEventPost`.
/// On non-macOS targets, always returns `true`.
pub fn check_accessibility_permission(prompt: bool) -> bool {
    #[cfg(target_os = "macos")]
    {
        macos_input::check_accessibility_permission(prompt)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = prompt;
        true
    }
}

/// Returns whether the process holds macOS Input Monitoring access for global event taps.
/// On non-macOS targets, always returns `true`.
pub fn check_input_monitoring_permission(prompt: bool) -> bool {
    #[cfg(target_os = "macos")]
    {
        macos_input::check_input_monitoring_permission(prompt)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = prompt;
        true
    }
}

static POST_UNLOCK_NUDGE_PENDING: AtomicBool = AtomicBool::new(false);
static LAST_LOGGED_CURSOR: Mutex<Option<(Option<u32>, Option<usize>)>> = Mutex::new(None);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorVisibilityInfo {
    pub flags: u32,
    pub showing: bool,
    pub suppressed: bool,
    pub hcursor_raw: usize,
}

impl CursorVisibilityInfo {
    pub fn from_flags(flags: u32) -> Self {
        Self::from_flags_and_handle(flags, 0)
    }

    pub fn from_flags_and_handle(flags: u32, hcursor_raw: usize) -> Self {
        Self {
            flags,
            showing: (flags & CURSOR_FLAG_SHOWING) != 0,
            suppressed: (flags & CURSOR_FLAG_SUPPRESSED) != 0,
            hcursor_raw,
        }
    }

    pub fn hcursor_null(&self) -> bool {
        self.hcursor_raw == 0
    }
}

/// Pure decision retained for unit clarity: checks if `GetCursorInfo` itself
/// reports hidden/suppressed/unknown. Post-unlock nudges are sent
/// unconditionally because Win32 can report `CURSOR_SHOWING` with a stale
/// cursor shape after leaving the secure desktop.
pub fn cursor_visibility_needs_nudge(info: Option<CursorVisibilityInfo>) -> bool {
    match info {
        Some(i) => !i.showing || i.suppressed || i.hcursor_null(),
        None => true,
    }
}

/// Pure change-detection helper for `CURSOR_INFO` logging: returns `true` only
/// on the first observation or when `(flags, hcursor_raw)` changes.
pub fn should_log_cursor_info(
    last_seen: &mut Option<(Option<u32>, Option<usize>)>,
    current: Option<CursorVisibilityInfo>,
) -> bool {
    let key = match current {
        Some(i) => (Some(i.flags), Some(i.hcursor_raw)),
        None => (None, None),
    };
    if last_seen.as_ref() == Some(&key) {
        false
    } else {
        *last_seen = Some(key);
        true
    }
}

pub fn mark_post_unlock_nudge_pending() {
    POST_UNLOCK_NUDGE_PENDING.store(true, Ordering::Release);
}

pub fn take_post_unlock_nudge_pending() -> bool {
    POST_UNLOCK_NUDGE_PENDING.swap(false, Ordering::AcqRel)
}

/// One-shot nudge on the first receiver mouse delta after WTS unlock.
/// Fast path is a single atomic load; only acquires `SendInput` once per unlock.
pub fn maybe_nudge_first_delta_after_unlock() {
    if POST_UNLOCK_NUDGE_PENDING.load(Ordering::Acquire) && take_post_unlock_nudge_pending() {
        super::hooks::restore_cursor_visibility();
        inject_cursor_nudge_1px();
    }
}

/// Sends a real 1-px relative move followed by the inverse -1-px relative move
/// (net zero displacement) so Windows dispatches `WM_SETCURSOR` and refreshes
/// the hardware cursor sprite without visible drift. Never call inside a hook callback.
pub fn inject_cursor_nudge_1px() {
    inject_mouse_delta(1, 0);
    inject_mouse_delta(-1, 0);
}

pub fn current_input_desktop_name() -> String {
    #[cfg(windows)]
    unsafe {
        if let Ok(hdesk) = OpenInputDesktop(
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_ACCESS_FLAGS(DESKTOP_READOBJECTS_ACCESS),
        ) {
            let mut buf = [0u16; 128];
            let mut needed = 0u32;
            let res = GetUserObjectInformationW(
                HANDLE(hdesk.0),
                UOI_NAME,
                Some(buf.as_mut_ptr() as *mut _),
                (buf.len() * std::mem::size_of::<u16>()) as u32,
                Some(&mut needed),
            );
            let _ = CloseDesktop(hdesk);
            if res.is_ok() {
                let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
                return String::from_utf16_lossy(&buf[..len]);
            }
            return "unknown".to_string();
        }
        "open_err".to_string()
    }
    #[cfg(not(windows))]
    {
        "non_windows".to_string()
    }
}

pub fn query_cursor_visibility() -> Option<CursorVisibilityInfo> {
    #[cfg(windows)]
    unsafe {
        let mut ci = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        if GetCursorInfo(&mut ci).is_ok() {
            let flags = ci.flags.0;
            return Some(CursorVisibilityInfo {
                flags,
                showing: (flags & CURSOR_SHOWING.0) != 0,
                suppressed: (flags & CURSOR_SUPPRESSED.0) != 0,
                hcursor_raw: ci.hCursor.0 as usize,
            });
        }
    }
    None
}

/// Logs `GetCursorInfo` state (only when `flags` or `hCursor` changes) and
/// unconditionally restores the local cursor visibility counter plus a 1-px
/// net-zero `SendInput` move (`+1, -1`). Call only on `CursorEnter` or WTS unlock
/// transitions — never per mouse delta and never from inside a hook callback.
pub fn ensure_cursor_visible_with_nudge(context: &str) {
    let before = query_cursor_visibility();
    let changed = LAST_LOGGED_CURSOR
        .lock()
        .map(|mut guard| should_log_cursor_info(&mut guard, before))
        .unwrap_or(false);
    if changed {
        let desktop = current_input_desktop_name();
        super::debug_log::note(|| {
            match before {
            Some(i) => format!(
                "CURSOR_INFO ctx={context} flags=0x{:X} showing={} suppressed={} hCursor=0x{:X} hcursor_null={} desktop=\"{desktop}\"",
                i.flags,
                i.showing,
                i.suppressed,
                i.hcursor_raw,
                i.hcursor_null(),
            ),
            None => format!("CURSOR_INFO ctx={context} query_err=1 desktop=\"{desktop}\""),
        }
        });
    }
    super::hooks::restore_cursor_visibility();
    inject_cursor_nudge_1px();
}

#[cfg(target_os = "linux")]
pub fn prepare_linux_remote_control(app_data_dir: &std::path::Path) -> Result<(), String> {
    linux_input::prepare_linux_remote_control(app_data_dir)
}

pub fn get_cursor_pos() -> Option<(i32, i32)> {
    #[cfg(windows)]
    unsafe {
        let mut pt = POINT::default();
        if GetCursorPos(&mut pt).is_ok() {
            return Some((pt.x, pt.y));
        }
    }
    #[cfg(target_os = "macos")]
    {
        return macos_input::get_cursor_pos();
    }
    #[cfg(target_os = "linux")]
    {
        return linux_input::get_cursor_pos();
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    None
}

pub fn inject_mouse_delta(dx: i32, dy: i32) {
    #[cfg(windows)]
    unsafe {
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_MOVE,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
    }
    #[cfg(target_os = "macos")]
    {
        macos_input::inject_mouse_delta(dx, dy);
    }
    #[cfg(target_os = "linux")]
    {
        linux_input::inject_mouse_delta(dx, dy);
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        let _ = (dx, dy);
    }
}

pub fn set_cursor_position(x: i32, y: i32) {
    #[cfg(windows)]
    unsafe {
        let _ = SetCursorPos(x, y);
    }
    #[cfg(target_os = "macos")]
    {
        macos_input::set_cursor_position(x, y);
    }
    #[cfg(target_os = "linux")]
    {
        linux_input::set_cursor_position(x, y);
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        let _ = (x, y);
    }
}

pub fn inject_mouse_button(button: u8, is_down: bool) -> (u32, u32) {
    #[cfg(windows)]
    unsafe {
        let flag = match (button, is_down) {
            (1, true) => MOUSEEVENTF_LEFTDOWN,
            (1, false) => MOUSEEVENTF_LEFTUP,
            (2, true) => MOUSEEVENTF_RIGHTDOWN,
            (2, false) => MOUSEEVENTF_RIGHTUP,
            (3, true) => MOUSEEVENTF_MIDDLEDOWN,
            (3, false) => MOUSEEVENTF_MIDDLEUP,
            _ => return (0, 0),
        };

        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: 0,
                    dwFlags: flag,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        let sent = SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
        let err = if sent == 0 {
            windows::Win32::Foundation::GetLastError().0
        } else {
            0
        };
        (sent, err)
    }
    #[cfg(target_os = "macos")]
    {
        macos_input::inject_mouse_button(button, is_down)
    }
    #[cfg(target_os = "linux")]
    {
        linux_input::inject_mouse_button(button, is_down)
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        let _ = (button, is_down);
        (0, 0)
    }
}

pub fn inject_mouse_wheel(delta: i32) {
    #[cfg(windows)]
    unsafe {
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: delta as u32,
                    dwFlags: MOUSEEVENTF_WHEEL,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
    }
    #[cfg(target_os = "macos")]
    {
        macos_input::inject_mouse_wheel(delta);
    }
    #[cfg(target_os = "linux")]
    {
        linux_input::inject_mouse_wheel(delta);
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        let _ = delta;
    }
}

pub fn inject_key(key_code: u16, scan_code: u16, is_down: bool, extended: bool) -> (u32, u32) {
    #[cfg(windows)]
    unsafe {
        let mut flags = KEYBD_EVENT_FLAGS(0);
        if !is_down {
            flags |= KEYEVENTF_KEYUP;
        }
        if extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        if scan_code > 0 {
            flags |= KEYEVENTF_SCANCODE;
        }

        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(key_code),
                    wScan: scan_code,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        let sent = SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
        let err = if sent == 0 {
            windows::Win32::Foundation::GetLastError().0
        } else {
            0
        };
        (sent, err)
    }
    #[cfg(target_os = "macos")]
    {
        macos_input::inject_key(key_code, scan_code, is_down, extended)
    }
    #[cfg(target_os = "linux")]
    {
        linux_input::inject_key(key_code, scan_code, is_down, extended)
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        let _ = (key_code, scan_code, is_down, extended);
        (0, 0)
    }
}

#[cfg(any(target_os = "macos", test))]
mod macos_logic {
    pub const KCG_EVENT_LEFT_MOUSE_DOWN: u32 = 1;
    pub const KCG_EVENT_LEFT_MOUSE_UP: u32 = 2;
    pub const KCG_EVENT_RIGHT_MOUSE_DOWN: u32 = 3;
    pub const KCG_EVENT_RIGHT_MOUSE_UP: u32 = 4;
    pub const KCG_EVENT_MOUSE_MOVED: u32 = 5;
    pub const KCG_EVENT_LEFT_MOUSE_DRAGGED: u32 = 6;
    pub const KCG_EVENT_RIGHT_MOUSE_DRAGGED: u32 = 7;
    pub const KCG_EVENT_OTHER_MOUSE_DOWN: u32 = 25;
    pub const KCG_EVENT_OTHER_MOUSE_UP: u32 = 26;
    pub const KCG_EVENT_OTHER_MOUSE_DRAGGED: u32 = 27;

    pub const KCG_MOUSE_BUTTON_LEFT: u32 = 0;
    pub const KCG_MOUSE_BUTTON_RIGHT: u32 = 1;
    pub const KCG_MOUSE_BUTTON_CENTER: u32 = 2;

    pub const KCG_EVENT_FLAG_MASK_ALPHA_SHIFT: u64 = 0x0001_0000;
    pub const KCG_EVENT_FLAG_MASK_SHIFT: u64 = 0x0002_0000;
    pub const KCG_EVENT_FLAG_MASK_CONTROL: u64 = 0x0004_0000;
    pub const KCG_EVENT_FLAG_MASK_ALTERNATE: u64 = 0x0008_0000;
    pub const KCG_EVENT_FLAG_MASK_COMMAND: u64 = 0x0010_0000;
    pub const KCG_EVENT_FLAG_MASK_NUMERIC_PAD: u64 = 0x0020_0000;

    const DOUBLE_CLICK_WINDOW_MS: u64 = 500;
    const DOUBLE_CLICK_MAX_DIST_PT: i32 = 4;

    /// Returns `(CGEventType, CGMouseButton)` for pointer motion given a bitmask of held buttons
    /// (bit 0 = Left, bit 1 = Right, bit 2 = Middle).
    pub fn macos_mouse_move_event_type(buttons_mask: u8) -> (u32, u32) {
        if (buttons_mask & 1) != 0 {
            (KCG_EVENT_LEFT_MOUSE_DRAGGED, KCG_MOUSE_BUTTON_LEFT)
        } else if (buttons_mask & 2) != 0 {
            (KCG_EVENT_RIGHT_MOUSE_DRAGGED, KCG_MOUSE_BUTTON_RIGHT)
        } else if (buttons_mask & 4) != 0 {
            (KCG_EVENT_OTHER_MOUSE_DRAGGED, KCG_MOUSE_BUTTON_CENTER)
        } else {
            (KCG_EVENT_MOUSE_MOVED, KCG_MOUSE_BUTTON_LEFT)
        }
    }

    /// Converts Win32 `WHEEL_DELTA` (`120` per detent) into CoreGraphics pixel scroll units.
    pub fn scale_macos_wheel_delta_px(delta: i32) -> i32 {
        if delta == 0 {
            return 0;
        }
        let scaled = ((delta as f64) * (40.0 / 120.0)).round() as i32;
        if scaled == 0 {
            delta.signum()
        } else {
            scaled
        }
    }

    /// Tracks consecutive clicks so injected double/triple clicks receive incrementing `ClickState`.
    #[derive(Debug, Default)]
    pub struct ClickStateTracker {
        last_button: u8,
        last_pos: (i32, i32),
        last_down_ms: Option<u64>,
        streak: i64,
        active_down_button: u8,
    }

    impl ClickStateTracker {
        pub fn on_button(
            &mut self,
            button: u8,
            is_down: bool,
            pos: (i32, i32),
            now_ms: u64,
        ) -> i64 {
            if is_down {
                let within_time = self
                    .last_down_ms
                    .is_some_and(|prev| now_ms.saturating_sub(prev) <= DOUBLE_CLICK_WINDOW_MS);
                let dist = (pos.0 - self.last_pos.0)
                    .abs()
                    .max((pos.1 - self.last_pos.1).abs());
                if self.last_button == button && within_time && dist <= DOUBLE_CLICK_MAX_DIST_PT {
                    self.streak = self.streak.saturating_add(1).max(1);
                } else {
                    self.streak = 1;
                }
                self.last_button = button;
                self.last_pos = pos;
                self.last_down_ms = Some(now_ms);
                self.active_down_button = button;
                self.streak
            } else if self.active_down_button == button {
                self.active_down_button = 0;
                self.streak.max(1)
            } else {
                1
            }
        }
    }

    /// Tracks injected macOS modifier state across left/right keys and CapsLock toggle.
    #[derive(Debug, Default)]
    pub struct MacModifierState {
        lshift: bool,
        rshift: bool,
        lctrl: bool,
        rctrl: bool,
        lalt: bool,
        ralt: bool,
        lcmd: bool,
        rcmd: bool,
        alpha_shift: bool,
    }

    impl MacModifierState {
        pub fn update(&mut self, cg_keycode: u16, is_down: bool) {
            match cg_keycode {
                0x38 => self.lshift = is_down,                           // kVK_Shift
                0x3C => self.rshift = is_down,                           // kVK_RightShift
                0x3B => self.lctrl = is_down,                            // kVK_Control
                0x3E => self.rctrl = is_down,                            // kVK_RightControl
                0x3A => self.lalt = is_down,                             // kVK_Option
                0x3D => self.ralt = is_down,                             // kVK_RightOption
                0x37 => self.lcmd = is_down,                             // kVK_Command
                0x36 => self.rcmd = is_down,                             // kVK_RightCommand
                0x39 if is_down => self.alpha_shift = !self.alpha_shift, // kVK_CapsLock
                _ => {}
            }
        }

        pub fn flags(&self) -> u64 {
            let mut f = 0u64;
            if self.alpha_shift {
                f |= KCG_EVENT_FLAG_MASK_ALPHA_SHIFT;
            }
            if self.lshift || self.rshift {
                f |= KCG_EVENT_FLAG_MASK_SHIFT;
            }
            if self.lctrl || self.rctrl {
                f |= KCG_EVENT_FLAG_MASK_CONTROL;
            }
            if self.lalt || self.ralt {
                f |= KCG_EVENT_FLAG_MASK_ALTERNATE;
            }
            if self.lcmd || self.rcmd {
                f |= KCG_EVENT_FLAG_MASK_COMMAND;
            }
            f
        }

        pub fn flags_for_key(&self, cg_keycode: u16) -> u64 {
            let mut f = self.flags();
            if is_macos_keypad_or_arrow(cg_keycode) {
                f |= KCG_EVENT_FLAG_MASK_NUMERIC_PAD;
            }
            f
        }
    }

    fn is_macos_keypad_or_arrow(cg_keycode: u16) -> bool {
        matches!(
            cg_keycode,
            0x41 | 0x43 | 0x45 | 0x47 | 0x4B | 0x4C | 0x4E | 0x51 | 0x52..=0x59 | 0x5B | 0x5C | 0x7B..=0x7E
        )
    }

    /// Maps Win32 `VK_*`, PS/2 Set 1 `scan_code`, and `extended` flag to macOS `CGKeyCode` (`kVK_*`),
    /// using 1:1 modifier mapping (`Win -> Command`, `Alt -> Option`, `Ctrl -> Control`).
    pub fn win_key_to_macos_cg_keycode(
        key_code: u16,
        scan_code: u16,
        extended: bool,
    ) -> Option<u16> {
        let mapped = match key_code {
            // Letters (kVK_ANSI_A .. kVK_ANSI_Z)
            0x41 => Some(0x00), // A
            0x42 => Some(0x0B), // B
            0x43 => Some(0x08), // C
            0x44 => Some(0x02), // D
            0x45 => Some(0x0E), // E
            0x46 => Some(0x03), // F
            0x47 => Some(0x05), // G
            0x48 => Some(0x04), // H
            0x49 => Some(0x22), // I
            0x4A => Some(0x26), // J
            0x4B => Some(0x28), // K
            0x4C => Some(0x25), // L
            0x4D => Some(0x2E), // M
            0x4E => Some(0x2D), // N
            0x4F => Some(0x1F), // O
            0x50 => Some(0x23), // P
            0x51 => Some(0x0C), // Q
            0x52 => Some(0x0F), // R
            0x53 => Some(0x01), // S
            0x54 => Some(0x11), // T
            0x55 => Some(0x20), // U
            0x56 => Some(0x09), // V
            0x57 => Some(0x0D), // W
            0x58 => Some(0x07), // X
            0x59 => Some(0x10), // Y
            0x5A => Some(0x06), // Z

            // Digits (kVK_ANSI_0 .. kVK_ANSI_9)
            0x30 => Some(0x1D), // 0
            0x31 => Some(0x12), // 1
            0x32 => Some(0x13), // 2
            0x33 => Some(0x14), // 3
            0x34 => Some(0x15), // 4
            0x35 => Some(0x17), // 5
            0x36 => Some(0x16), // 6
            0x37 => Some(0x1A), // 7
            0x38 => Some(0x1C), // 8
            0x39 => Some(0x19), // 9

            // Editing, whitespace, navigation
            0x08 => Some(0x33), // VK_BACK -> kVK_Delete
            0x09 => Some(0x30), // VK_TAB -> kVK_Tab
            0x0D => Some(if extended { 0x4C } else { 0x24 }), // VK_RETURN -> KeypadEnter / Return
            0x1B => Some(0x35), // VK_ESCAPE -> kVK_Escape
            0x20 => Some(0x31), // VK_SPACE -> kVK_Space
            0x21 => Some(0x74), // VK_PRIOR -> kVK_PageUp
            0x22 => Some(0x79), // VK_NEXT -> kVK_PageDown
            0x23 => Some(0x77), // VK_END -> kVK_End
            0x24 => Some(0x73), // VK_HOME -> kVK_Home
            0x25 => Some(0x7B), // VK_LEFT -> kVK_LeftArrow
            0x26 => Some(0x7E), // VK_UP -> kVK_UpArrow
            0x27 => Some(0x7C), // VK_RIGHT -> kVK_RightArrow
            0x28 => Some(0x7D), // VK_DOWN -> kVK_DownArrow
            0x2D => Some(0x72), // VK_INSERT -> kVK_Help
            0x2E => Some(0x75), // VK_DELETE -> kVK_ForwardDelete
            0x5D => Some(0x6E), // VK_APPS -> kVK_ContextualMenu

            // Modifiers (1:1 Win->Command, Alt->Option, Ctrl->Control)
            0x10 => Some(if scan_code == 0x36 { 0x3C } else { 0x38 }), // VK_SHIFT
            0xA0 => Some(0x38),                                        // VK_LSHIFT -> kVK_Shift
            0xA1 => Some(0x3C), // VK_RSHIFT -> kVK_RightShift
            0x11 => Some(if extended { 0x3E } else { 0x3B }), // VK_CONTROL
            0xA2 => Some(0x3B), // VK_LCONTROL -> kVK_Control
            0xA3 => Some(0x3E), // VK_RCONTROL -> kVK_RightControl
            0x12 => Some(if extended { 0x3D } else { 0x3A }), // VK_MENU
            0xA4 => Some(0x3A), // VK_LMENU -> kVK_Option
            0xA5 => Some(0x3D), // VK_RMENU -> kVK_RightOption
            0x5B => Some(0x37), // VK_LWIN -> kVK_Command
            0x5C => Some(0x36), // VK_RWIN -> kVK_RightCommand
            0x14 => Some(0x39), // VK_CAPITAL -> kVK_CapsLock

            // Function keys F1..F20
            0x70 => Some(0x7A), // F1
            0x71 => Some(0x78), // F2
            0x72 => Some(0x63), // F3
            0x73 => Some(0x76), // F4
            0x74 => Some(0x60), // F5
            0x75 => Some(0x61), // F6
            0x76 => Some(0x62), // F7
            0x77 => Some(0x64), // F8
            0x78 => Some(0x65), // F9
            0x79 => Some(0x6D), // F10
            0x7A => Some(0x67), // F11
            0x7B => Some(0x6F), // F12
            0x7C => Some(0x69), // F13
            0x7D => Some(0x6B), // F14
            0x7E => Some(0x71), // F15
            0x7F => Some(0x6A), // F16
            0x80 => Some(0x40), // F17
            0x81 => Some(0x4F), // F18
            0x82 => Some(0x50), // F19
            0x83 => Some(0x5A), // F20

            // Numpad
            0x60 => Some(0x52), // VK_NUMPAD0
            0x61 => Some(0x53), // VK_NUMPAD1
            0x62 => Some(0x54), // VK_NUMPAD2
            0x63 => Some(0x55), // VK_NUMPAD3
            0x64 => Some(0x56), // VK_NUMPAD4
            0x65 => Some(0x57), // VK_NUMPAD5
            0x66 => Some(0x58), // VK_NUMPAD6
            0x67 => Some(0x59), // VK_NUMPAD7
            0x68 => Some(0x5B), // VK_NUMPAD8
            0x69 => Some(0x5C), // VK_NUMPAD9
            0x6A => Some(0x43), // VK_MULTIPLY
            0x6B => Some(0x45), // VK_ADD
            0x6D => Some(0x4E), // VK_SUBTRACT
            0x6E => Some(0x41), // VK_DECIMAL
            0x6F => Some(0x4B), // VK_DIVIDE
            0x90 => Some(0x47), // VK_NUMLOCK -> kVK_ANSI_KeypadClear

            // ANSI / ISO OEM punctuation
            0xBA => Some(0x29), // VK_OEM_1 (;:)
            0xBB => Some(0x18), // VK_OEM_PLUS (=+)
            0xBC => Some(0x2B), // VK_OEM_COMMA (,<)
            0xBD => Some(0x1B), // VK_OEM_MINUS (-_)
            0xBE => Some(0x2F), // VK_OEM_PERIOD (.>)
            0xBF => Some(0x2C), // VK_OEM_2 (/?)
            0xC0 => Some(0x32), // VK_OEM_3 (`~)
            0xDB => Some(0x21), // VK_OEM_4 ([{)
            0xDC => Some(0x2A), // VK_OEM_5 (\|)
            0xDD => Some(0x1E), // VK_OEM_6 (]})
            0xDE => Some(0x27), // VK_OEM_7 ('")
            0xE2 => Some(0x0A), // VK_OEM_102 -> kVK_ISO_Section
            0 => None,
            _ => None,
        };
        if mapped.is_some() {
            return mapped;
        }
        if key_code == 0 && scan_code > 0 {
            return win_scan_code_to_macos_cg_keycode(scan_code, extended);
        }
        None
    }

    pub fn win_scan_code_to_macos_cg_keycode(scan_code: u16, extended: bool) -> Option<u16> {
        if extended {
            return match scan_code {
                0x1C => Some(0x4C), // Numpad Enter
                0x1D => Some(0x3E), // Right Ctrl
                0x35 => Some(0x4B), // Numpad /
                0x38 => Some(0x3D), // Right Option
                0x47 => Some(0x73), // Home
                0x48 => Some(0x7E), // Up
                0x49 => Some(0x74), // PageUp
                0x4B => Some(0x7B), // Left
                0x4D => Some(0x7C), // Right
                0x4F => Some(0x77), // End
                0x50 => Some(0x7D), // Down
                0x51 => Some(0x79), // PageDown
                0x52 => Some(0x72), // Insert -> Help
                0x53 => Some(0x75), // ForwardDelete
                0x5B => Some(0x37), // Left Command
                0x5C => Some(0x36), // Right Command
                0x5D => Some(0x6E), // ContextMenu
                _ => None,
            };
        }
        match scan_code {
            0x01 => Some(0x35), // Escape
            0x02 => Some(0x12), // 1
            0x03 => Some(0x13), // 2
            0x04 => Some(0x14), // 3
            0x05 => Some(0x15), // 4
            0x06 => Some(0x17), // 5
            0x07 => Some(0x16), // 6
            0x08 => Some(0x1A), // 7
            0x09 => Some(0x1C), // 8
            0x0A => Some(0x19), // 9
            0x0B => Some(0x1D), // 0
            0x0C => Some(0x1B), // -
            0x0D => Some(0x18), // =
            0x0E => Some(0x33), // Backspace
            0x0F => Some(0x30), // Tab
            0x10 => Some(0x0C), // Q
            0x11 => Some(0x0D), // W
            0x12 => Some(0x0E), // E
            0x13 => Some(0x0F), // R
            0x14 => Some(0x11), // T
            0x15 => Some(0x10), // Y
            0x16 => Some(0x20), // U
            0x17 => Some(0x22), // I
            0x18 => Some(0x1F), // O
            0x19 => Some(0x23), // P
            0x1A => Some(0x21), // [
            0x1B => Some(0x1E), // ]
            0x1C => Some(0x24), // Return
            0x1D => Some(0x3B), // Left Ctrl
            0x1E => Some(0x00), // A
            0x1F => Some(0x01), // S
            0x20 => Some(0x02), // D
            0x21 => Some(0x03), // F
            0x22 => Some(0x05), // G
            0x23 => Some(0x04), // H
            0x24 => Some(0x26), // J
            0x25 => Some(0x28), // K
            0x26 => Some(0x25), // L
            0x27 => Some(0x29), // ;
            0x28 => Some(0x27), // '
            0x29 => Some(0x32), // `
            0x2A => Some(0x38), // Left Shift
            0x2B => Some(0x2A), // backslash
            0x2C => Some(0x06), // Z
            0x2D => Some(0x07), // X
            0x2E => Some(0x08), // C
            0x2F => Some(0x09), // V
            0x30 => Some(0x0B), // B
            0x31 => Some(0x2D), // N
            0x32 => Some(0x2E), // M
            0x33 => Some(0x2B), // ,
            0x34 => Some(0x2F), // .
            0x35 => Some(0x2C), // /
            0x36 => Some(0x3C), // Right Shift
            0x37 => Some(0x43), // Numpad *
            0x38 => Some(0x3A), // Left Alt
            0x39 => Some(0x31), // Space
            0x3A => Some(0x39), // CapsLock
            0x3B => Some(0x7A), // F1
            0x3C => Some(0x78), // F2
            0x3D => Some(0x63), // F3
            0x3E => Some(0x76), // F4
            0x3F => Some(0x60), // F5
            0x40 => Some(0x61), // F6
            0x41 => Some(0x62), // F7
            0x42 => Some(0x64), // F8
            0x43 => Some(0x65), // F9
            0x44 => Some(0x6D), // F10
            0x47 => Some(0x59), // Numpad 7
            0x48 => Some(0x5B), // Numpad 8
            0x49 => Some(0x5C), // Numpad 9
            0x4A => Some(0x4E), // Numpad -
            0x4B => Some(0x56), // Numpad 4
            0x4C => Some(0x57), // Numpad 5
            0x4D => Some(0x58), // Numpad 6
            0x4E => Some(0x45), // Numpad +
            0x4F => Some(0x53), // Numpad 1
            0x50 => Some(0x54), // Numpad 2
            0x51 => Some(0x55), // Numpad 3
            0x52 => Some(0x52), // Numpad 0
            0x53 => Some(0x41), // Numpad .
            0x56 => Some(0x0A), // ISO section
            0x57 => Some(0x67), // F11
            0x58 => Some(0x6F), // F12
            _ => None,
        }
    }
}

#[cfg(target_os = "macos")]
mod macos_input {
    use super::macos_logic::{
        macos_mouse_move_event_type, scale_macos_wheel_delta_px, win_key_to_macos_cg_keycode,
        ClickStateTracker, MacModifierState, KCG_EVENT_LEFT_MOUSE_DOWN, KCG_EVENT_LEFT_MOUSE_UP,
        KCG_EVENT_OTHER_MOUSE_DOWN, KCG_EVENT_OTHER_MOUSE_UP, KCG_EVENT_RIGHT_MOUSE_DOWN,
        KCG_EVENT_RIGHT_MOUSE_UP, KCG_MOUSE_BUTTON_CENTER, KCG_MOUSE_BUTTON_LEFT,
        KCG_MOUSE_BUTTON_RIGHT,
    };
    use std::ffi::c_void;
    use std::ptr;
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::sync::{Mutex, OnceLock};
    use std::time::Instant;

    type CFTypeRef = *const c_void;
    type CFDictionaryRef = *const c_void;
    type CFStringRef = *const c_void;
    type CFBooleanRef = *const c_void;
    type CFAllocatorRef = *const c_void;
    type CGEventSourceRef = *mut c_void;
    type CGEventRef = *mut c_void;
    type CGEventType = u32;
    type CGMouseButton = u32;
    type CGScrollEventUnit = u32;
    type CGEventTapLocation = u32;
    type CGEventField = u32;
    type CGEventFlags = u64;
    type CGKeyCode = u16;
    type CGError = i32;

    const K_CG_HID_EVENT_TAP: CGEventTapLocation = 0;
    const K_CG_SCROLL_EVENT_UNIT_PIXEL: CGScrollEventUnit = 0;
    const K_CG_EVENT_SOURCE_STATE_PRIVATE: i32 = -1;
    const K_CG_MOUSE_EVENT_CLICK_STATE: CGEventField = 1;
    const K_CG_MOUSE_EVENT_DELTA_X: CGEventField = 4;
    const K_CG_MOUSE_EVENT_DELTA_Y: CGEventField = 5;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGPoint {
        x: f64,
        y: f64,
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        static kCFBooleanTrue: CFBooleanRef;
        static kCFTypeDictionaryKeyCallBacks: c_void;
        static kCFTypeDictionaryValueCallBacks: c_void;
        fn CFDictionaryCreate(
            allocator: CFAllocatorRef,
            keys: *const *const c_void,
            values: *const *const c_void,
            num_values: isize,
            key_callbacks: *const c_void,
            value_callbacks: *const c_void,
        ) -> CFDictionaryRef;
        fn CFRelease(cf: CFTypeRef);
    }

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        static kAXTrustedCheckOptionPrompt: CFStringRef;
        fn AXIsProcessTrusted() -> u8;
        fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> u8;
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGEventSourceCreate(state_id: i32) -> CGEventSourceRef;
        fn CGEventSourceSetLocalEventsSuppressionInterval(source: CGEventSourceRef, seconds: f64);
        fn CGEventSourceSetUserData(source: CGEventSourceRef, user_data: i64);
        fn CGPreflightListenEventAccess() -> bool;
        fn CGRequestListenEventAccess() -> bool;
        fn CGEventCreate(source: CGEventSourceRef) -> CGEventRef;
        fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
        fn CGWarpMouseCursorPosition(new_cursor_position: CGPoint) -> CGError;
        fn CGAssociateMouseAndMouseCursorPosition(connected: u32) -> CGError;
        fn CGEventCreateMouseEvent(
            source: CGEventSourceRef,
            mouse_type: CGEventType,
            mouse_cursor_position: CGPoint,
            mouse_button: CGMouseButton,
        ) -> CGEventRef;
        fn CGEventCreateKeyboardEvent(
            source: CGEventSourceRef,
            virtual_key: CGKeyCode,
            key_down: bool,
        ) -> CGEventRef;
        fn CGEventCreateScrollWheelEvent2(
            source: CGEventSourceRef,
            units: CGScrollEventUnit,
            wheel_count: u32,
            wheel1: i32,
            wheel2: i32,
            wheel3: i32,
        ) -> CGEventRef;
        fn CGEventSetFlags(event: CGEventRef, flags: CGEventFlags);
        fn CGEventSetIntegerValueField(event: CGEventRef, field: CGEventField, value: i64);
        fn CGEventPost(tap: CGEventTapLocation, event: CGEventRef);
    }

    struct EventSourcePtr(CGEventSourceRef);
    unsafe impl Send for EventSourcePtr {}
    unsafe impl Sync for EventSourcePtr {}

    static EVENT_SOURCE: OnceLock<EventSourcePtr> = OnceLock::new();
    static HELD_MOUSE_BUTTONS: AtomicU8 = AtomicU8::new(0);
    static LAST_CURSOR_POS: Mutex<Option<(i32, i32)>> = Mutex::new(None);
    static MODIFIERS: OnceLock<Mutex<MacModifierState>> = OnceLock::new();
    static CLICK_TRACKER: OnceLock<Mutex<ClickStateTracker>> = OnceLock::new();
    static EPOCH: OnceLock<Instant> = OnceLock::new();

    fn event_source() -> CGEventSourceRef {
        EVENT_SOURCE
            .get_or_init(|| unsafe {
                let src = CGEventSourceCreate(K_CG_EVENT_SOURCE_STATE_PRIVATE);
                if !src.is_null() {
                    CGEventSourceSetLocalEventsSuppressionInterval(src, 0.0);
                    CGEventSourceSetUserData(src, super::ANYLEAP_CG_USER_DATA);
                }
                EventSourcePtr(src)
            })
            .0
    }

    fn modifiers() -> &'static Mutex<MacModifierState> {
        MODIFIERS.get_or_init(|| Mutex::new(MacModifierState::default()))
    }

    fn click_tracker() -> &'static Mutex<ClickStateTracker> {
        CLICK_TRACKER.get_or_init(|| Mutex::new(ClickStateTracker::default()))
    }

    fn now_ms() -> u64 {
        EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
    }

    pub fn check_accessibility_permission(prompt: bool) -> bool {
        unsafe {
            if AXIsProcessTrusted() != 0 {
                return true;
            }
            if !prompt {
                return false;
            }
            let keys = [kAXTrustedCheckOptionPrompt as *const c_void];
            let values = [kCFBooleanTrue as *const c_void];
            let dict = CFDictionaryCreate(
                ptr::null(),
                keys.as_ptr(),
                values.as_ptr(),
                1,
                &kCFTypeDictionaryKeyCallBacks as *const c_void,
                &kCFTypeDictionaryValueCallBacks as *const c_void,
            );
            if dict.is_null() {
                return false;
            }
            let trusted = AXIsProcessTrustedWithOptions(dict) != 0;
            CFRelease(dict);
            trusted
        }
    }

    pub fn check_input_monitoring_permission(prompt: bool) -> bool {
        unsafe {
            if CGPreflightListenEventAccess() {
                return true;
            }
            if !prompt {
                return false;
            }
            CGRequestListenEventAccess()
        }
    }

    pub fn get_cursor_pos() -> Option<(i32, i32)> {
        unsafe {
            let ev = CGEventCreate(ptr::null_mut());
            if ev.is_null() {
                return None;
            }
            let pt = CGEventGetLocation(ev);
            CFRelease(ev as CFTypeRef);
            Some((pt.x.round() as i32, pt.y.round() as i32))
        }
    }

    pub fn set_cursor_position(x: i32, y: i32) {
        let (dx, dy) = if let Ok(mut guard) = LAST_CURSOR_POS.lock() {
            let delta = match *guard {
                Some((px, py)) => (x.saturating_sub(px), y.saturating_sub(py)),
                None => (0, 0),
            };
            *guard = Some((x, y));
            delta
        } else {
            (0, 0)
        };
        let buttons = HELD_MOUSE_BUTTONS.load(Ordering::Acquire);
        let (ev_type, cg_btn) = macos_mouse_move_event_type(buttons);
        let flags = modifiers().lock().map(|m| m.flags()).unwrap_or(0);
        let pt = CGPoint {
            x: x as f64,
            y: y as f64,
        };
        unsafe {
            let _ = CGAssociateMouseAndMouseCursorPosition(1);
            let _ = CGWarpMouseCursorPosition(pt);
            let src = event_source();
            let ev = CGEventCreateMouseEvent(src, ev_type, pt, cg_btn);
            if !ev.is_null() {
                CGEventSetIntegerValueField(ev, K_CG_MOUSE_EVENT_DELTA_X, dx as i64);
                CGEventSetIntegerValueField(ev, K_CG_MOUSE_EVENT_DELTA_Y, dy as i64);
                CGEventSetFlags(ev, flags);
                CGEventPost(K_CG_HID_EVENT_TAP, ev);
                CFRelease(ev as CFTypeRef);
            }
        }
    }

    pub fn inject_mouse_delta(dx: i32, dy: i32) {
        if dx == 0 && dy == 0 {
            return;
        }
        if let Some((cx, cy)) = get_cursor_pos() {
            set_cursor_position(cx.saturating_add(dx), cy.saturating_add(dy));
        }
    }

    pub fn inject_mouse_button(button: u8, is_down: bool) -> (u32, u32) {
        let (bit, ev_type, cg_btn) = match (button, is_down) {
            (1, true) => (1u8, KCG_EVENT_LEFT_MOUSE_DOWN, KCG_MOUSE_BUTTON_LEFT),
            (1, false) => (1u8, KCG_EVENT_LEFT_MOUSE_UP, KCG_MOUSE_BUTTON_LEFT),
            (2, true) => (2u8, KCG_EVENT_RIGHT_MOUSE_DOWN, KCG_MOUSE_BUTTON_RIGHT),
            (2, false) => (2u8, KCG_EVENT_RIGHT_MOUSE_UP, KCG_MOUSE_BUTTON_RIGHT),
            (3, true) => (4u8, KCG_EVENT_OTHER_MOUSE_DOWN, KCG_MOUSE_BUTTON_CENTER),
            (3, false) => (4u8, KCG_EVENT_OTHER_MOUSE_UP, KCG_MOUSE_BUTTON_CENTER),
            _ => return (0, 0),
        };
        if is_down {
            HELD_MOUSE_BUTTONS.fetch_or(bit, Ordering::AcqRel);
        } else {
            HELD_MOUSE_BUTTONS.fetch_and(!bit, Ordering::AcqRel);
        }

        let pos = get_cursor_pos().unwrap_or((0, 0));
        let click_state = click_tracker()
            .lock()
            .map(|mut ct| ct.on_button(button, is_down, pos, now_ms()))
            .unwrap_or(1);
        let flags = modifiers().lock().map(|m| m.flags()).unwrap_or(0);
        let pt = CGPoint {
            x: pos.0 as f64,
            y: pos.1 as f64,
        };
        unsafe {
            let ev = CGEventCreateMouseEvent(event_source(), ev_type, pt, cg_btn);
            if ev.is_null() {
                return (0, 1);
            }
            CGEventSetIntegerValueField(ev, K_CG_MOUSE_EVENT_CLICK_STATE, click_state);
            CGEventSetFlags(ev, flags);
            CGEventPost(K_CG_HID_EVENT_TAP, ev);
            CFRelease(ev as CFTypeRef);
            (1, 0)
        }
    }

    pub fn inject_mouse_wheel(delta: i32) {
        let wheel_px = scale_macos_wheel_delta_px(delta);
        if wheel_px == 0 {
            return;
        }
        unsafe {
            let ev = CGEventCreateScrollWheelEvent2(
                event_source(),
                K_CG_SCROLL_EVENT_UNIT_PIXEL,
                1,
                wheel_px,
                0,
                0,
            );
            if !ev.is_null() {
                CGEventPost(K_CG_HID_EVENT_TAP, ev);
                CFRelease(ev as CFTypeRef);
            }
        }
    }

    pub fn inject_key(key_code: u16, scan_code: u16, is_down: bool, extended: bool) -> (u32, u32) {
        let Some(cg_code) = win_key_to_macos_cg_keycode(key_code, scan_code, extended) else {
            return (0, 0);
        };
        let flags = modifiers()
            .lock()
            .map(|mut m| {
                m.update(cg_code, is_down);
                m.flags_for_key(cg_code)
            })
            .unwrap_or(0);
        unsafe {
            let ev = CGEventCreateKeyboardEvent(event_source(), cg_code, is_down);
            if ev.is_null() {
                return (0, 1);
            }
            CGEventSetFlags(ev, flags);
            CGEventPost(K_CG_HID_EVENT_TAP, ev);
            CFRelease(ev as CFTypeRef);
            (1, 0)
        }
    }
}

#[cfg(any(target_os = "linux", test))]
mod linux_logic {
    use crate::kvm::protocol::ScreenRect;

    const EVDEV_KEY_A: u16 = 30;
    const EVDEV_KEY_LEFTSHIFT: u16 = 42;
    const EVDEV_KEY_RIGHTSHIFT: u16 = 54;
    const EVDEV_KEY_LEFTCTRL: u16 = 29;
    const EVDEV_KEY_RIGHTCTRL: u16 = 97;
    const EVDEV_KEY_LEFTALT: u16 = 56;
    const EVDEV_KEY_RIGHTALT: u16 = 100;
    const EVDEV_KEY_LEFTMETA: u16 = 125;
    const EVDEV_KEY_RIGHTMETA: u16 = 126;
    const EVDEV_KEY_CAPSLOCK: u16 = 58;

    const XKB_MOD_SHIFT: u32 = 1 << 0;
    const XKB_MOD_LOCK: u32 = 1 << 1;
    const XKB_MOD_CONTROL: u32 = 1 << 2;
    const XKB_MOD_ALT: u32 = 1 << 3;
    const XKB_MOD_SUPER: u32 = 1 << 6;

    const US_EVDEV_XKB_KEYMAP: &[u8] = concat!(
        "xkb_keymap {\n",
        "\txkb_keycodes  { include \"evdev+aliases(qwerty)\" };\n",
        "\txkb_types     { include \"complete\" };\n",
        "\txkb_compat    { include \"complete\" };\n",
        "\txkb_symbols   { include \"pc+us+inet(evdev)\" };\n",
        "\txkb_geometry  { include \"pc(pc104)\" };\n",
        "};\n\0"
    )
    .as_bytes();

    /// Returns a NUL-terminated standard US-evdev XKB keymap byte slice for `zwp_virtual_keyboard_v1::keymap`.
    pub fn build_us_evdev_xkb_keymap() -> &'static [u8] {
        US_EVDEV_XKB_KEYMAP
    }

    /// Translate Win32 `(vk_code, scan_code, extended)` into a Linux `evdev` keycode (`<linux/input-event-codes.h>`).
    /// Modifiers map 1:1 (`Win -> Meta/Super`, `Alt -> Alt`, `Ctrl -> Ctrl`).
    pub fn win_key_to_linux_evdev(key_code: u16, scan_code: u16, extended: bool) -> Option<u16> {
        let vk_mapped = match key_code {
            // Letters (US QWERTY ANSI -> evdev hardware keycodes)
            0x41 => Some(EVDEV_KEY_A),
            0x42 => Some(48),
            0x43 => Some(46),
            0x44 => Some(32),
            0x45 => Some(18),
            0x46 => Some(33),
            0x47 => Some(34),
            0x48 => Some(35),
            0x49 => Some(23),
            0x4A => Some(36),
            0x4B => Some(37),
            0x4C => Some(38),
            0x4D => Some(50),
            0x4E => Some(49),
            0x4F => Some(24),
            0x50 => Some(25),
            0x51 => Some(16),
            0x52 => Some(19),
            0x53 => Some(31),
            0x54 => Some(20),
            0x55 => Some(22),
            0x56 => Some(47),
            0x57 => Some(17),
            0x58 => Some(45),
            0x59 => Some(21),
            0x5A => Some(44),

            // Top-row digits '1'..'0'
            0x31 => Some(2),
            0x32 => Some(3),
            0x33 => Some(4),
            0x34 => Some(5),
            0x35 => Some(6),
            0x36 => Some(7),
            0x37 => Some(8),
            0x38 => Some(9),
            0x39 => Some(10),
            0x30 => Some(11),

            // Whitespace / core controls
            0x08 => Some(14), // VK_BACK -> KEY_BACKSPACE
            0x09 => Some(15), // VK_TAB -> KEY_TAB
            0x0D => Some(if extended { 96 } else { 28 }), // VK_RETURN -> KEY_KPENTER / KEY_ENTER
            0x13 => Some(119), // VK_PAUSE -> KEY_PAUSE
            0x14 => Some(EVDEV_KEY_CAPSLOCK),
            0x1B => Some(1),  // VK_ESCAPE -> KEY_ESC
            0x20 => Some(57), // VK_SPACE -> KEY_SPACE

            // Navigation & editing
            0x21 => Some(104), // VK_PRIOR -> KEY_PAGEUP
            0x22 => Some(109), // VK_NEXT -> KEY_PAGEDOWN
            0x23 => Some(107), // VK_END -> KEY_END
            0x24 => Some(102), // VK_HOME -> KEY_HOME
            0x25 => Some(105), // VK_LEFT -> KEY_LEFT
            0x26 => Some(103), // VK_UP -> KEY_UP
            0x27 => Some(106), // VK_RIGHT -> KEY_RIGHT
            0x28 => Some(108), // VK_DOWN -> KEY_DOWN
            0x2C => Some(99),  // VK_SNAPSHOT -> KEY_SYSRQ
            0x2D => Some(110), // VK_INSERT -> KEY_INSERT
            0x2E => Some(111), // VK_DELETE -> KEY_DELETE

            // System & 1:1 modifiers
            0x5B => Some(EVDEV_KEY_LEFTMETA),
            0x5C => Some(EVDEV_KEY_RIGHTMETA),
            0x5D => Some(127), // VK_APPS -> KEY_COMPOSE
            0x5F => Some(142), // VK_SLEEP -> KEY_SLEEP
            0x10 | 0xA0 => Some(EVDEV_KEY_LEFTSHIFT),
            0xA1 => Some(EVDEV_KEY_RIGHTSHIFT),
            0x11 => Some(if extended {
                EVDEV_KEY_RIGHTCTRL
            } else {
                EVDEV_KEY_LEFTCTRL
            }),
            0xA2 => Some(EVDEV_KEY_LEFTCTRL),
            0xA3 => Some(EVDEV_KEY_RIGHTCTRL),
            0x12 => Some(if extended {
                EVDEV_KEY_RIGHTALT
            } else {
                EVDEV_KEY_LEFTALT
            }),
            0xA4 => Some(EVDEV_KEY_LEFTALT),
            0xA5 => Some(EVDEV_KEY_RIGHTALT),

            // Locks
            0x90 => Some(69), // VK_NUMLOCK -> KEY_NUMLOCK
            0x91 => Some(70), // VK_SCROLL -> KEY_SCROLLLOCK

            // Numpad
            0x60 => Some(82),  // VK_NUMPAD0
            0x61 => Some(79),  // VK_NUMPAD1
            0x62 => Some(80),  // VK_NUMPAD2
            0x63 => Some(81),  // VK_NUMPAD3
            0x64 => Some(75),  // VK_NUMPAD4
            0x65 => Some(76),  // VK_NUMPAD5
            0x66 => Some(77),  // VK_NUMPAD6
            0x67 => Some(71),  // VK_NUMPAD7
            0x68 => Some(72),  // VK_NUMPAD8
            0x69 => Some(73),  // VK_NUMPAD9
            0x6A => Some(55),  // VK_MULTIPLY -> KEY_KPASTERISK
            0x6B => Some(78),  // VK_ADD -> KEY_KPPLUS
            0x6C => Some(121), // VK_SEPARATOR -> KEY_KPCOMMA
            0x6D => Some(74),  // VK_SUBTRACT -> KEY_KPMINUS
            0x6E => Some(83),  // VK_DECIMAL -> KEY_KPDOT
            0x6F => Some(98),  // VK_DIVIDE -> KEY_KPSLASH

            // Function keys F1..F24
            0x70..=0x79 => Some(59 + (key_code - 0x70)), // F1..F10
            0x7A => Some(87),                            // F11
            0x7B => Some(88),                            // F12
            0x7C..=0x87 => Some(183 + (key_code - 0x7C)), // F13..F24

            // Browser / Media keys
            0xA6 => Some(158), // KEY_BACK
            0xA7 => Some(159), // KEY_FORWARD
            0xA8 => Some(173), // KEY_REFRESH
            0xA9 => Some(128), // KEY_STOP
            0xAA => Some(217), // KEY_SEARCH
            0xAB => Some(156), // KEY_BOOKMARKS
            0xAC => Some(172), // KEY_HOMEPAGE
            0xAD => Some(113), // KEY_MUTE
            0xAE => Some(114), // KEY_VOLUMEDOWN
            0xAF => Some(115), // KEY_VOLUMEUP
            0xB0 => Some(163), // KEY_NEXTSONG
            0xB1 => Some(165), // KEY_PREVIOUSSONG
            0xB2 => Some(166), // KEY_STOPCD
            0xB3 => Some(164), // KEY_PLAYPAUSE

            // US OEM punctuation
            0xBA => Some(39), // ;:
            0xBB => Some(13), // =+
            0xBC => Some(51), // ,<
            0xBD => Some(12), // -_
            0xBE => Some(52), // .>
            0xBF => Some(53), // /?
            0xC0 => Some(41), // `~
            0xDB => Some(26), // [{
            0xDC => Some(43), // \|
            0xDD => Some(27), // ]}
            0xDE => Some(40), // '"
            0xE2 => Some(86), // ISO <> / KEY_102ND

            _ => None,
        };

        vk_mapped.or_else(|| set1_scancode_to_linux_evdev(scan_code, extended))
    }

    fn set1_scancode_to_linux_evdev(scan_code: u16, extended: bool) -> Option<u16> {
        if scan_code == 0 {
            return None;
        }
        if extended {
            match scan_code {
                0x1C => Some(96), // KPENTER
                0x1D => Some(EVDEV_KEY_RIGHTCTRL),
                0x35 => Some(98), // KPSLASH
                0x37 => Some(99), // SYSRQ
                0x38 => Some(EVDEV_KEY_RIGHTALT),
                0x47 => Some(102), // HOME
                0x48 => Some(103), // UP
                0x49 => Some(104), // PAGEUP
                0x4B => Some(105), // LEFT
                0x4D => Some(106), // RIGHT
                0x4F => Some(107), // END
                0x50 => Some(108), // DOWN
                0x51 => Some(109), // PAGEDOWN
                0x52 => Some(110), // INSERT
                0x53 => Some(111), // DELETE
                0x5B => Some(EVDEV_KEY_LEFTMETA),
                0x5C => Some(EVDEV_KEY_RIGHTMETA),
                0x5D => Some(127), // COMPOSE
                _ => None,
            }
        } else if (0x01..=0x58).contains(&scan_code) {
            Some(scan_code)
        } else {
            None
        }
    }

    /// Tracks simultaneous L/R modifiers and CapsLock latch state so
    /// `zwp_virtual_keyboard_v1::modifiers(depressed, 0, locked, 0)` stays accurate.
    #[derive(Debug, Default)]
    pub struct LinuxModifierTracker {
        left_shift: bool,
        right_shift: bool,
        left_ctrl: bool,
        right_ctrl: bool,
        left_alt: bool,
        right_alt: bool,
        left_super: bool,
        right_super: bool,
        caps_locked: bool,
    }

    impl LinuxModifierTracker {
        pub fn update(&mut self, evdev_code: u16, is_down: bool) -> (u32, u32) {
            match evdev_code {
                EVDEV_KEY_LEFTSHIFT => self.left_shift = is_down,
                EVDEV_KEY_RIGHTSHIFT => self.right_shift = is_down,
                EVDEV_KEY_LEFTCTRL => self.left_ctrl = is_down,
                EVDEV_KEY_RIGHTCTRL => self.right_ctrl = is_down,
                EVDEV_KEY_LEFTALT => self.left_alt = is_down,
                EVDEV_KEY_RIGHTALT => self.right_alt = is_down,
                EVDEV_KEY_LEFTMETA => self.left_super = is_down,
                EVDEV_KEY_RIGHTMETA => self.right_super = is_down,
                EVDEV_KEY_CAPSLOCK if is_down => {
                    self.caps_locked = !self.caps_locked;
                }
                _ => {}
            }
            self.masks()
        }

        pub fn clear(&mut self) -> (u32, u32) {
            *self = Self::default();
            (0, 0)
        }

        fn masks(&self) -> (u32, u32) {
            let mut depressed = 0u32;
            if self.left_shift || self.right_shift {
                depressed |= XKB_MOD_SHIFT;
            }
            if self.left_ctrl || self.right_ctrl {
                depressed |= XKB_MOD_CONTROL;
            }
            if self.left_alt || self.right_alt {
                depressed |= XKB_MOD_ALT;
            }
            if self.left_super || self.right_super {
                depressed |= XKB_MOD_SUPER;
            }
            let locked = if self.caps_locked { XKB_MOD_LOCK } else { 0 };
            (depressed, locked)
        }
    }

    /// Normalize global compositor `(x, y)` inside `union` into unsigned
    /// `(rx, ry, width_extent, height_extent)` for `zwlr_virtual_pointer_v1::motion_absolute`.
    pub fn normalize_linux_abs_pointer(
        x: i32,
        y: i32,
        union: &ScreenRect,
    ) -> Option<(u32, u32, u32, u32)> {
        if union.width <= 0 || union.height <= 0 {
            return None;
        }
        let rx = (x - union.x).clamp(0, union.width - 1) as u32;
        let ry = (y - union.y).clamp(0, union.height - 1) as u32;
        Some((rx, ry, union.width as u32, union.height as u32))
    }

    /// Accumulates fractional scroll detents and normalizes direction for Wayland (`wl_pointer::axis`) vs X11.
    #[derive(Debug, Default)]
    pub struct LinuxScrollAccumulator {
        wayland_remainder: i32,
        x11_remainder: i32,
    }

    impl LinuxScrollAccumulator {
        /// Win32 `+120` (up) -> negative Wayland axis delta (`-15.0`) and negative discrete detent (`-1`).
        pub fn push_wayland(&mut self, delta: i32) -> (f64, i32) {
            if delta == 0 {
                return (0.0, 0);
            }
            let axis_value = -(delta as f64) * 15.0 / 120.0;
            self.wayland_remainder += -delta;
            let discrete = self.wayland_remainder / 120;
            self.wayland_remainder -= discrete * 120;
            (axis_value, discrete)
        }

        /// Win32 `+120` (up) -> positive click count (`+1` for X11 button 4); `-120` (down) -> `-1` (button 5).
        pub fn push_x11_clicks(&mut self, delta: i32) -> i32 {
            if delta == 0 {
                return 0;
            }
            self.x11_remainder += delta;
            let clicks = self.x11_remainder / 120;
            self.x11_remainder -= clicks * 120;
            clicks
        }
    }

    /// Software cursor position tracker for Wayland backends (which do not allow querying global cursor position).
    #[derive(Debug, Default)]
    pub struct SoftCursorTracker {
        pos: Option<(i32, i32)>,
    }

    impl SoftCursorTracker {
        pub fn set(&mut self, x: i32, y: i32) {
            self.pos = Some((x, y));
        }

        pub fn get(&self) -> Option<(i32, i32)> {
            self.pos
        }

        pub fn apply_delta(
            &mut self,
            dx: i32,
            dy: i32,
            union: Option<&ScreenRect>,
        ) -> Option<(i32, i32)> {
            let (base_x, base_y) = self.pos.unwrap_or((0, 0));
            let mut next_x = base_x.saturating_add(dx);
            let mut next_y = base_y.saturating_add(dy);
            if let Some(rect) = union {
                if rect.width > 0 && rect.height > 0 {
                    next_x = next_x.clamp(rect.x, rect.x + rect.width - 1);
                    next_y = next_y.clamp(rect.y, rect.y + rect.height - 1);
                }
            }
            self.pos = Some((next_x, next_y));
            self.pos
        }
    }
}

#[cfg(target_os = "linux")]
mod linux_input {
    use super::linux_logic::{
        build_us_evdev_xkb_keymap, normalize_linux_abs_pointer, win_key_to_linux_evdev,
        LinuxModifierTracker, LinuxScrollAccumulator, SoftCursorTracker,
    };
    use crate::kvm::input_logic::union_rect;
    use crate::kvm::monitors::get_local_monitors;
    use crate::kvm::protocol::ScreenRect;
    use std::path::{Path, PathBuf};
    use std::sync::{mpsc, Mutex};
    use std::time::{Duration, Instant};

    const PORTAL_TOKEN_FILE: &str = "kvm_wayland_portal_token.txt";
    const EVDEV_BTN_LEFT: u32 = 0x110;
    const EVDEV_BTN_RIGHT: u32 = 0x111;
    const EVDEV_BTN_MIDDLE: u32 = 0x112;

    enum WlrCmd {
        MotionAbs { x: i32, y: i32, union: ScreenRect },
        MotionDelta { dx: i32, dy: i32 },
        Button { evdev_code: u32, is_down: bool },
        Wheel { delta: i32 },
        Key { evdev: u16, is_down: bool },
    }

    enum PortalCmd {
        MotionAbs {
            x: i32,
            y: i32,
            dx: i32,
            dy: i32,
            union: ScreenRect,
        },
        MotionDelta {
            dx: i32,
            dy: i32,
        },
        Button {
            evdev_code: u32,
            is_down: bool,
        },
        Wheel {
            delta: i32,
        },
        Key {
            evdev: u16,
            is_down: bool,
        },
    }

    struct X11Backend {
        conn: x11rb::rust_connection::RustConnection,
        root: x11rb::protocol::xproto::Window,
        scroll: LinuxScrollAccumulator,
    }

    enum LinuxBackend {
        Wlroots {
            tx: mpsc::Sender<WlrCmd>,
            soft_cursor: SoftCursorTracker,
        },
        Portal {
            tx: tokio::sync::mpsc::UnboundedSender<PortalCmd>,
            soft_cursor: SoftCursorTracker,
        },
        X11(X11Backend),
    }

    static LINUX_BACKEND: Mutex<Option<LinuxBackend>> = Mutex::new(None);
    static PORTAL_DATA_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);
    static LAST_UNION: Mutex<Option<(Instant, ScreenRect)>> = Mutex::new(None);

    fn map_button_to_evdev(button: u8) -> Option<u32> {
        match button {
            1 => Some(EVDEV_BTN_LEFT),
            2 => Some(EVDEV_BTN_RIGHT),
            3 => Some(EVDEV_BTN_MIDDLE),
            _ => None,
        }
    }

    fn cached_monitor_union(force_refresh: bool) -> Option<ScreenRect> {
        if !force_refresh {
            if let Ok(guard) = LAST_UNION.lock() {
                if let Some((captured_at, ref rect)) = *guard {
                    if captured_at.elapsed() < Duration::from_secs(5)
                        && rect.width > 0
                        && rect.height > 0
                    {
                        return Some(rect.clone());
                    }
                }
            }
        }
        let monitors = get_local_monitors();
        let rect = union_rect(&monitors);
        if rect.width <= 0 || rect.height <= 0 {
            return None;
        }
        if let Ok(mut guard) = LAST_UNION.lock() {
            *guard = Some((Instant::now(), rect.clone()));
        }
        Some(rect)
    }

    struct WlrWaylandState;

    impl
        wayland_client::Dispatch<
            wayland_client::protocol::wl_registry::WlRegistry,
            wayland_client::globals::GlobalListContents,
        > for WlrWaylandState
    {
        fn event(
            _state: &mut Self,
            _proxy: &wayland_client::protocol::wl_registry::WlRegistry,
            _event: wayland_client::protocol::wl_registry::Event,
            _data: &wayland_client::globals::GlobalListContents,
            _conn: &wayland_client::Connection,
            _qh: &wayland_client::QueueHandle<Self>,
        ) {
        }
    }

    impl wayland_client::Dispatch<wayland_client::protocol::wl_seat::WlSeat, ()> for WlrWaylandState {
        fn event(
            _state: &mut Self,
            _proxy: &wayland_client::protocol::wl_seat::WlSeat,
            _event: wayland_client::protocol::wl_seat::Event,
            _data: &(),
            _conn: &wayland_client::Connection,
            _qh: &wayland_client::QueueHandle<Self>,
        ) {
        }
    }

    impl
        wayland_client::Dispatch<
            wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
            (),
        > for WlrWaylandState
    {
        fn event(
            _state: &mut Self,
            _proxy: &wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
            _event: wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::Event,
            _data: &(),
            _conn: &wayland_client::Connection,
            _qh: &wayland_client::QueueHandle<Self>,
        ) {
        }
    }

    impl
        wayland_client::Dispatch<
            wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
            (),
        > for WlrWaylandState
    {
        fn event(
            _state: &mut Self,
            _proxy: &wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
            _event: wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::Event,
            _data: &(),
            _conn: &wayland_client::Connection,
            _qh: &wayland_client::QueueHandle<Self>,
        ) {
        }
    }

    impl
        wayland_client::Dispatch<
            wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
            (),
        > for WlrWaylandState
    {
        fn event(
            _state: &mut Self,
            _proxy: &wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
            _event: wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::Event,
            _data: &(),
            _conn: &wayland_client::Connection,
            _qh: &wayland_client::QueueHandle<Self>,
        ) {
        }
    }

    impl
        wayland_client::Dispatch<
            wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
            (),
        > for WlrWaylandState
    {
        fn event(
            _state: &mut Self,
            _proxy: &wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
            _event: wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::Event,
            _data: &(),
            _conn: &wayland_client::Connection,
            _qh: &wayland_client::QueueHandle<Self>,
        ) {
        }
    }

    fn connect_wlr_wayland() -> Result<LinuxBackend, String> {
        use std::io::{Seek, SeekFrom, Write};
        use std::os::unix::io::AsFd;
        use wayland_client::globals::registry_queue_init;
        use wayland_client::protocol::{wl_keyboard, wl_pointer, wl_seat};
        use wayland_client::Connection;
        use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
            zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
            zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
        };
        use wayland_protocols_wlr::virtual_pointer::v1::client::{
            zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
            zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
        };

        if std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return Err("WAYLAND_DISPLAY not set".into());
        }

        let (init_tx, init_rx) = mpsc::sync_channel::<Result<(), String>>(1);
        let (cmd_tx, cmd_rx) = mpsc::channel::<WlrCmd>();

        std::thread::Builder::new()
            .name("kvm-linux-wl".into())
            .spawn(move || {
                let setup = (|| -> Result<_, String> {
                    let conn = Connection::connect_to_env()
                        .map_err(|e| format!("Wayland connect failed: {e}"))?;
                    let (globals, mut queue) = registry_queue_init::<WlrWaylandState>(&conn)
                        .map_err(|e| format!("Wayland registry failed: {e}"))?;
                    let qh = queue.handle();

                    let seat: wl_seat::WlSeat = globals
                        .bind(&qh, 1..=9, ())
                        .map_err(|e| format!("wl_seat unavailable: {e}"))?;
                    let ptr_mgr: ZwlrVirtualPointerManagerV1 = globals
                        .bind(&qh, 1..=2, ())
                        .map_err(|e| format!("zwlr_virtual_pointer_manager_v1 unavailable: {e}"))?;
                    let kbd_mgr: ZwpVirtualKeyboardManagerV1 = globals
                        .bind(&qh, 1..=1, ())
                        .map_err(|e| format!("zwp_virtual_keyboard_manager_v1 unavailable: {e}"))?;

                    let vptr: ZwlrVirtualPointerV1 =
                        ptr_mgr.create_virtual_pointer(Some(&seat), &qh, ());
                    let vkbd: ZwpVirtualKeyboardV1 =
                        kbd_mgr.create_virtual_keyboard(&seat, &qh, ());

                    let keymap_bytes = build_us_evdev_xkb_keymap();
                    let memfd = rustix::fs::memfd_create(
                        "anyleap-xkb-keymap",
                        rustix::fs::MemfdFlags::CLOEXEC,
                    )
                    .map_err(|e| format!("memfd_create failed: {e}"))?;
                    let mut keymap_file = std::fs::File::from(memfd);
                    keymap_file
                        .set_len(keymap_bytes.len() as u64)
                        .map_err(|e| format!("keymap ftruncate failed: {e}"))?;
                    keymap_file
                        .write_all(keymap_bytes)
                        .map_err(|e| format!("keymap write failed: {e}"))?;
                    keymap_file
                        .seek(SeekFrom::Start(0))
                        .map_err(|e| format!("keymap seek failed: {e}"))?;
                    vkbd.keymap(
                        wl_keyboard::KeymapFormat::XkbV1 as u32,
                        keymap_file.as_fd(),
                        keymap_bytes.len() as u32,
                    );

                    let mut wl_state = WlrWaylandState;
                    queue
                        .roundtrip(&mut wl_state)
                        .map_err(|e| format!("Wayland virtual device roundtrip failed: {e}"))?;
                    drop(keymap_file);
                    Ok((conn, ptr_mgr, vptr, vkbd))
                })();

                let (conn, ptr_mgr, vptr, vkbd) = match setup {
                    Ok(v) => {
                        let _ = init_tx.send(Ok(()));
                        v
                    }
                    Err(e) => {
                        let _ = init_tx.send(Err(e));
                        return;
                    }
                };

                let started = Instant::now();
                let mut mod_tracker = LinuxModifierTracker::default();
                let mut prev_mods = (0u32, 0u32);
                let mut scroll_accum = LinuxScrollAccumulator::default();

                let dispatch =
                    |cmd: WlrCmd,
                     vptr: &ZwlrVirtualPointerV1,
                     vkbd: &ZwpVirtualKeyboardV1,
                     mod_tracker: &mut LinuxModifierTracker,
                     prev_mods: &mut (u32, u32),
                     scroll_accum: &mut LinuxScrollAccumulator| {
                        let now_ms = started.elapsed().as_millis() as u32;
                        match cmd {
                            WlrCmd::MotionAbs { x, y, union } => {
                                if let Some((rx, ry, ext_w, ext_h)) =
                                    normalize_linux_abs_pointer(x, y, &union)
                                {
                                    vptr.motion_absolute(now_ms, rx, ry, ext_w, ext_h);
                                    vptr.frame();
                                }
                            }
                            WlrCmd::MotionDelta { dx, dy } => {
                                vptr.motion(now_ms, dx as f64, dy as f64);
                                vptr.frame();
                            }
                            WlrCmd::Button {
                                evdev_code,
                                is_down,
                            } => {
                                let state = if is_down {
                                    wl_pointer::ButtonState::Pressed
                                } else {
                                    wl_pointer::ButtonState::Released
                                };
                                vptr.button(now_ms, evdev_code, state);
                                vptr.frame();
                            }
                            WlrCmd::Wheel { delta } => {
                                let (axis_value, discrete) = scroll_accum.push_wayland(delta);
                                if axis_value != 0.0 || discrete != 0 {
                                    vptr.axis_source(wl_pointer::AxisSource::Wheel);
                                    if discrete != 0 {
                                        vptr.axis_discrete(
                                            now_ms,
                                            wl_pointer::Axis::VerticalScroll,
                                            axis_value,
                                            discrete,
                                        );
                                    } else {
                                        vptr.axis(
                                            now_ms,
                                            wl_pointer::Axis::VerticalScroll,
                                            axis_value,
                                        );
                                    }
                                    vptr.frame();
                                }
                            }
                            WlrCmd::Key { evdev, is_down } => {
                                let key_state = if is_down { 1 } else { 0 };
                                vkbd.key(now_ms, u32::from(evdev), key_state);
                                let next_mods = mod_tracker.update(evdev, is_down);
                                if next_mods != *prev_mods {
                                    *prev_mods = next_mods;
                                    vkbd.modifiers(next_mods.0, 0, next_mods.1, 0);
                                }
                            }
                        }
                    };

                while let Ok(cmd) = cmd_rx.recv() {
                    dispatch(
                        cmd,
                        &vptr,
                        &vkbd,
                        &mut mod_tracker,
                        &mut prev_mods,
                        &mut scroll_accum,
                    );
                    while let Ok(next) = cmd_rx.try_recv() {
                        dispatch(
                            next,
                            &vptr,
                            &vkbd,
                            &mut mod_tracker,
                            &mut prev_mods,
                            &mut scroll_accum,
                        );
                    }
                    if conn.flush().is_err() {
                        break;
                    }
                }

                let _ = mod_tracker.clear();
                vptr.destroy();
                vkbd.destroy();
                ptr_mgr.destroy();
                let _ = conn.flush();
            })
            .map_err(|e| format!("Failed to spawn Wayland worker thread: {e}"))?;

        init_rx
            .recv()
            .map_err(|_| "Wayland virtual input thread exited during init".to_string())??;

        Ok(LinuxBackend::Wlroots {
            tx: cmd_tx,
            soft_cursor: SoftCursorTracker::default(),
        })
    }

    fn connect_portal_wayland(data_dir: Option<PathBuf>) -> Result<LinuxBackend, String> {
        use ashpd::desktop::remote_desktop::{
            Axis, ConnectToEISOptions, DeviceType, KeyState, NotifyKeyboardKeycodeOptions,
            NotifyPointerAxisDiscreteOptions, NotifyPointerAxisOptions, NotifyPointerButtonOptions,
            NotifyPointerMotionOptions, RemoteDesktop, SelectDevicesOptions, StartOptions,
        };
        use ashpd::desktop::{CreateSessionOptions, PersistMode};
        use futures_util::StreamExt as _;
        use reis::ei;
        use reis::event::{DeviceCapability, EiEvent};
        use std::os::unix::net::UnixStream;

        if std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return Err("WAYLAND_DISPLAY not set".into());
        }

        let (init_tx, init_rx) = mpsc::sync_channel::<Result<(), String>>(1);
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<PortalCmd>();

        std::thread::Builder::new()
            .name("kvm-linux-portal".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = init_tx.send(Err(format!("Portal Tokio runtime init failed: {e}")));
                        return;
                    }
                };

                rt.block_on(async move {
                    let remote_desktop = match RemoteDesktop::new().await {
                        Ok(rd) => rd,
                        Err(e) => {
                            let _ = init_tx.send(Err(format!(
                                "XDG RemoteDesktop portal unavailable: {e}"
                            )));
                            return;
                        }
                    };
                    let session = match remote_desktop
                        .create_session(CreateSessionOptions::default())
                        .await
                    {
                        Ok(s) => s,
                        Err(e) => {
                            let _ = init_tx.send(Err(format!(
                                "RemoteDesktop create_session failed: {e}"
                            )));
                            return;
                        }
                    };

                    let saved_token = data_dir
                        .as_ref()
                        .and_then(|dir| std::fs::read_to_string(dir.join(PORTAL_TOKEN_FILE)).ok())
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty());

                    let sel_opts = SelectDevicesOptions::default()
                        .set_devices(DeviceType::Keyboard | DeviceType::Pointer)
                        .set_persist_mode(PersistMode::ExplicitlyRevoked)
                        .set_restore_token(saved_token.as_deref());

                    if let Err(e) = remote_desktop.select_devices(&session, sel_opts).await {
                        let _ = init_tx.send(Err(format!(
                            "RemoteDesktop select_devices failed: {e}"
                        )));
                        return;
                    }

                    let start_resp = match remote_desktop
                        .start(&session, None, StartOptions::default())
                        .await
                    {
                        Ok(req) => match req.response() {
                            Ok(resp) => resp,
                            Err(e) => {
                                let _ = init_tx.send(Err(format!(
                                    "RemoteDesktop permission cancelled or denied: {e}"
                                )));
                                return;
                            }
                        },
                        Err(e) => {
                            let _ = init_tx.send(Err(format!("RemoteDesktop start failed: {e}")));
                            return;
                        }
                    };

                    if let (Some(dir), Some(token)) = (&data_dir, start_resp.restore_token()) {
                        let _ = std::fs::create_dir_all(dir);
                        let _ = std::fs::write(dir.join(PORTAL_TOKEN_FILE), token);
                    }

                    let eis_ctx = match remote_desktop
                        .connect_to_eis(&session, ConnectToEISOptions::default())
                        .await
                    {
                        Ok(fd) => {
                            let stream = UnixStream::from(fd);
                            let _ = stream.set_nonblocking(true);
                            match ei::Context::new(stream) {
                                Ok(ctx) => match ctx
                                    .handshake_tokio(
                                        "AnyLeap",
                                        ei::handshake::ContextType::Sender,
                                    )
                                    .await
                                {
                                    Ok(pair) => Some(pair),
                                    Err(e) => {
                                        super::super::debug_log::note(|| {
                                            format!("PORTAL_EIS_HANDSHAKE_ERR err={e}")
                                        });
                                        None
                                    }
                                },
                                Err(e) => {
                                    super::super::debug_log::note(|| {
                                        format!("PORTAL_EIS_CTX_ERR err={e}")
                                    });
                                    None
                                }
                            }
                        }
                        Err(e) => {
                            super::super::debug_log::note(|| {
                                format!("PORTAL_CONNECT_TO_EIS_UNAVAILABLE err={e}")
                            });
                            None
                        }
                    };

                    let _ = init_tx.send(Ok(()));
                    let started = Instant::now();
                    let mut scroll_accum = LinuxScrollAccumulator::default();

                    if let Some((ei_conn, mut ei_events)) = eis_ctx {
                        struct EiDev {
                            device: reis::event::Device,
                            resumed: bool,
                            emulating: bool,
                        }
                        let mut devices: Vec<EiDev> = Vec::new();
                        let mut seq: u32 = 1;

                        let dispatch_eis = |cmd: PortalCmd,
                                            devices: &[EiDev],
                                            ei_conn: &reis::event::Connection,
                                            scroll_accum: &mut LinuxScrollAccumulator| {
                            let serial = ei_conn.serial();
                            let now_us = started.elapsed().as_micros() as u64;
                            match cmd {
                                PortalCmd::MotionAbs {
                                    x,
                                    y,
                                    dx,
                                    dy,
                                    union,
                                } => {
                                    if let Some(entry) = devices.iter().find(|d| {
                                        d.resumed
                                            && d.device
                                                .interface::<ei::PointerAbsolute>()
                                                .is_some()
                                    }) {
                                        if let (Some(abs), Some((rx, ry, _, _))) = (
                                            entry.device.interface::<ei::PointerAbsolute>(),
                                            normalize_linux_abs_pointer(x, y, &union),
                                        ) {
                                            let off_x = entry
                                                .device
                                                .regions()
                                                .iter()
                                                .map(|r| r.x)
                                                .min()
                                                .unwrap_or(0);
                                            let off_y = entry
                                                .device
                                                .regions()
                                                .iter()
                                                .map(|r| r.y)
                                                .min()
                                                .unwrap_or(0);
                                            abs.motion_absolute(
                                                (off_x + rx) as f32,
                                                (off_y + ry) as f32,
                                            );
                                            entry.device.device().frame(serial, now_us);
                                        }
                                    } else if let Some(entry) = devices.iter().find(|d| {
                                        d.resumed && d.device.interface::<ei::Pointer>().is_some()
                                    }) {
                                        if let Some(ptr) = entry.device.interface::<ei::Pointer>() {
                                            ptr.motion_relative(dx as f32, dy as f32);
                                            entry.device.device().frame(serial, now_us);
                                        }
                                    }
                                }
                                PortalCmd::MotionDelta { dx, dy } => {
                                    if let Some(entry) = devices.iter().find(|d| {
                                        d.resumed && d.device.interface::<ei::Pointer>().is_some()
                                    }) {
                                        if let Some(ptr) = entry.device.interface::<ei::Pointer>() {
                                            ptr.motion_relative(dx as f32, dy as f32);
                                            entry.device.device().frame(serial, now_us);
                                        }
                                    }
                                }
                                PortalCmd::Button {
                                    evdev_code,
                                    is_down,
                                } => {
                                    if let Some(entry) = devices.iter().find(|d| {
                                        d.resumed && d.device.interface::<ei::Button>().is_some()
                                    }) {
                                        if let Some(btn) = entry.device.interface::<ei::Button>() {
                                            let state = if is_down {
                                                ei::button::ButtonState::Press
                                            } else {
                                                ei::button::ButtonState::Released
                                            };
                                            btn.button(evdev_code, state);
                                            entry.device.device().frame(serial, now_us);
                                        }
                                    }
                                }
                                PortalCmd::Wheel { delta } => {
                                    let (_, discrete) = scroll_accum.push_wayland(delta);
                                    let scroll_units = if discrete != 0 {
                                        discrete * 120
                                    } else {
                                        -delta
                                    };
                                    if scroll_units != 0 {
                                        if let Some(entry) = devices.iter().find(|d| {
                                            d.resumed
                                                && d.device.interface::<ei::Scroll>().is_some()
                                        }) {
                                            if let Some(scroll) =
                                                entry.device.interface::<ei::Scroll>()
                                            {
                                                scroll.scroll_discrete(0, scroll_units);
                                                entry.device.device().frame(serial, now_us);
                                            }
                                        }
                                    }
                                }
                                PortalCmd::Key { evdev, is_down } => {
                                    if let Some(entry) = devices.iter().find(|d| {
                                        d.resumed && d.device.interface::<ei::Keyboard>().is_some()
                                    }) {
                                        if let Some(kbd) = entry.device.interface::<ei::Keyboard>()
                                        {
                                            let state = if is_down {
                                                ei::keyboard::KeyState::Press
                                            } else {
                                                ei::keyboard::KeyState::Released
                                            };
                                            kbd.key(u32::from(evdev), state);
                                            entry.device.device().frame(serial, now_us);
                                        }
                                    }
                                }
                            }
                        };

                        loop {
                            tokio::select! {
                                ev = ei_events.next() => {
                                    match ev {
                                        Some(Ok(EiEvent::SeatAdded(evt))) => {
                                            evt.seat.bind_capabilities(
                                                DeviceCapability::Pointer
                                                    | DeviceCapability::PointerAbsolute
                                                    | DeviceCapability::Keyboard
                                                    | DeviceCapability::Scroll
                                                    | DeviceCapability::Button,
                                            );
                                            let _ = ei_conn.flush();
                                        }
                                        Some(Ok(EiEvent::DeviceAdded(evt))) => {
                                            devices.push(EiDev {
                                                device: evt.device,
                                                resumed: false,
                                                emulating: false,
                                            });
                                        }
                                        Some(Ok(EiEvent::DeviceRemoved(evt))) => {
                                            devices.retain(|d| d.device != evt.device);
                                        }
                                        Some(Ok(EiEvent::DeviceResumed(evt))) => {
                                            if let Some(entry) = devices.iter_mut().find(|d| d.device == evt.device) {
                                                entry.resumed = true;
                                                if !entry.emulating {
                                                    entry.device.device().start_emulating(evt.serial, seq);
                                                    seq = seq.wrapping_add(1).max(1);
                                                    entry.emulating = true;
                                                    let _ = ei_conn.flush();
                                                }
                                            }
                                        }
                                        Some(Ok(EiEvent::DevicePaused(evt))) => {
                                            if let Some(entry) = devices.iter_mut().find(|d| d.device == evt.device) {
                                                entry.resumed = false;
                                                if entry.emulating {
                                                    entry.device.device().stop_emulating(evt.serial);
                                                    entry.emulating = false;
                                                    let _ = ei_conn.flush();
                                                }
                                            }
                                        }
                                        Some(Ok(EiEvent::Disconnected(_))) | Some(Err(_)) | None => break,
                                        _ => {}
                                    }
                                }
                                cmd = cmd_rx.recv() => {
                                    match cmd {
                                        Some(first) => {
                                            dispatch_eis(first, &devices, &ei_conn, &mut scroll_accum);
                                            while let Ok(next) = cmd_rx.try_recv() {
                                                dispatch_eis(next, &devices, &ei_conn, &mut scroll_accum);
                                            }
                                            let _ = ei_conn.flush();
                                        }
                                        None => break,
                                    }
                                }
                            }
                        }
                    } else {
                        while let Some(cmd) = cmd_rx.recv().await {
                            match cmd {
                                PortalCmd::MotionAbs { dx, dy, .. }
                                | PortalCmd::MotionDelta { dx, dy } => {
                                    if dx != 0 || dy != 0 {
                                        let _ = remote_desktop
                                            .notify_pointer_motion(
                                                &session,
                                                dx as f64,
                                                dy as f64,
                                                NotifyPointerMotionOptions::default(),
                                            )
                                            .await;
                                    }
                                }
                                PortalCmd::Button {
                                    evdev_code,
                                    is_down,
                                } => {
                                    let state = if is_down {
                                        KeyState::Pressed
                                    } else {
                                        KeyState::Released
                                    };
                                    let _ = remote_desktop
                                        .notify_pointer_button(
                                            &session,
                                            evdev_code as i32,
                                            state,
                                            NotifyPointerButtonOptions::default(),
                                        )
                                        .await;
                                }
                                PortalCmd::Wheel { delta } => {
                                    let (axis_val, discrete) = scroll_accum.push_wayland(delta);
                                    if discrete != 0 {
                                        let _ = remote_desktop
                                            .notify_pointer_axis_discrete(
                                                &session,
                                                Axis::Vertical,
                                                discrete,
                                                NotifyPointerAxisDiscreteOptions::default(),
                                            )
                                            .await;
                                    } else if axis_val != 0.0 {
                                        let _ = remote_desktop
                                            .notify_pointer_axis(
                                                &session,
                                                0.0,
                                                axis_val,
                                                NotifyPointerAxisOptions::default().set_finish(true),
                                            )
                                            .await;
                                    }
                                }
                                PortalCmd::Key { evdev, is_down } => {
                                    let state = if is_down {
                                        KeyState::Pressed
                                    } else {
                                        KeyState::Released
                                    };
                                    let _ = remote_desktop
                                        .notify_keyboard_keycode(
                                            &session,
                                            i32::from(evdev),
                                            state,
                                            NotifyKeyboardKeycodeOptions::default(),
                                        )
                                        .await;
                                }
                            }
                        }
                    }
                });
            })
            .map_err(|e| format!("Failed to spawn Portal worker thread: {e}"))?;

        match init_rx.recv_timeout(Duration::from_millis(600)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Keep Portal worker running asynchronously while user confirms the OS permission prompt.
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("Portal RemoteDesktop worker exited unexpectedly".into());
            }
        }

        Ok(LinuxBackend::Portal {
            tx: cmd_tx,
            soft_cursor: SoftCursorTracker::default(),
        })
    }

    fn connect_x11() -> Result<LinuxBackend, String> {
        use x11rb::connection::RequestConnection as _;
        use x11rb::protocol::xtest;

        if std::env::var_os("DISPLAY").is_none() {
            return Err("DISPLAY not set".into());
        }
        let (conn, screen_num) =
            x11rb::connect(None).map_err(|e| format!("X11 connect failed: {e}"))?;
        let root = conn
            .setup()
            .roots
            .get(screen_num)
            .ok_or_else(|| "No X11 screen found".to_string())?
            .root;
        let has_xtest = conn
            .extension_information(xtest::X11_EXTENSION_NAME)
            .map_err(|e| format!("X11 XTest probe failed: {e}"))?
            .is_some();
        if !has_xtest {
            return Err("X11 XTest extension not available".into());
        }

        Ok(LinuxBackend::X11(X11Backend {
            conn,
            root,
            scroll: LinuxScrollAccumulator::default(),
        }))
    }

    fn init_backend(data_dir: Option<PathBuf>) -> Result<LinuxBackend, String> {
        let mut last_err = None;
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            match connect_wlr_wayland() {
                Ok(b) => return Ok(b),
                Err(e) => {
                    super::super::debug_log::note(|| format!("LINUX_WLR_SKIP err={e}"));
                    match connect_portal_wayland(data_dir) {
                        Ok(b) => return Ok(b),
                        Err(pe) => {
                            last_err = Some(pe);
                        }
                    }
                }
            }
        }

        if std::env::var_os("DISPLAY").is_some() {
            match connect_x11() {
                Ok(b) => return Ok(b),
                Err(xe) => {
                    last_err = Some(last_err.map(|p| format!("{p}; {xe}")).unwrap_or(xe));
                }
            }
        }

        Err(last_err.unwrap_or_else(|| {
            "No active Wayland or X11 session detected (WAYLAND_DISPLAY and DISPLAY are both unset)."
                .into()
        }))
    }

    fn with_backend<R>(f: impl FnOnce(&mut LinuxBackend) -> R) -> Option<R> {
        let mut guard = LINUX_BACKEND.lock().ok()?;
        if guard.is_none() {
            let dir = PORTAL_DATA_DIR.lock().ok().and_then(|d| d.clone());
            match init_backend(dir) {
                Ok(b) => *guard = Some(b),
                Err(e) => {
                    super::super::debug_log::note(|| format!("LINUX_BACKEND_INIT_ERR {e}"));
                    return None;
                }
            }
        }
        guard.as_mut().map(f)
    }

    pub fn prepare_linux_remote_control(app_data_dir: &Path) -> Result<(), String> {
        if let Ok(mut d) = PORTAL_DATA_DIR.lock() {
            *d = Some(app_data_dir.to_path_buf());
        }
        let mut guard = LINUX_BACKEND
            .lock()
            .map_err(|_| "Linux backend lock poisoned".to_string())?;
        if guard.is_some() {
            return Ok(());
        }
        let backend = init_backend(Some(app_data_dir.to_path_buf()))?;
        *guard = Some(backend);
        Ok(())
    }

    pub fn get_cursor_pos() -> Option<(i32, i32)> {
        use x11rb::protocol::xproto::ConnectionExt as _;

        with_backend(|backend| match backend {
            LinuxBackend::Wlroots { soft_cursor, .. }
            | LinuxBackend::Portal { soft_cursor, .. } => soft_cursor.get(),
            LinuxBackend::X11(x11) => x11
                .conn
                .query_pointer(x11.root)
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|r| (i32::from(r.root_x), i32::from(r.root_y))),
        })
        .flatten()
    }

    pub fn set_cursor_position(x: i32, y: i32) {
        use x11rb::connection::Connection as _;
        use x11rb::protocol::xproto::ConnectionExt as _;

        let union = cached_monitor_union(true);
        let dropped = with_backend(|backend| match backend {
            LinuxBackend::Wlroots { tx, soft_cursor } => {
                soft_cursor.set(x, y);
                let Some(rect) = union else {
                    return false;
                };
                tx.send(WlrCmd::MotionAbs { x, y, union: rect }).is_err()
            }
            LinuxBackend::Portal { tx, soft_cursor } => {
                let (px, py) = soft_cursor.get().unwrap_or((x, y));
                soft_cursor.set(x, y);
                let Some(rect) = union else {
                    return false;
                };
                tx.send(PortalCmd::MotionAbs {
                    x,
                    y,
                    dx: x - px,
                    dy: y - py,
                    union: rect,
                })
                .is_err()
            }
            LinuxBackend::X11(x11) => {
                let _ = x11.conn.warp_pointer(
                    x11rb::NONE,
                    x11.root,
                    0,
                    0,
                    0,
                    0,
                    x.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                    y.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                );
                let _ = x11.conn.flush();
                false
            }
        });
        if matches!(dropped, Some(true)) {
            if let Ok(mut guard) = LINUX_BACKEND.lock() {
                *guard = None;
            }
        }
    }

    pub fn inject_mouse_delta(dx: i32, dy: i32) {
        use x11rb::connection::Connection as _;
        use x11rb::protocol::{xproto, xtest};

        let union = cached_monitor_union(false);
        let dropped = with_backend(|backend| match backend {
            LinuxBackend::Wlroots { tx, soft_cursor } => {
                if let (Some((nx, ny)), Some(rect)) =
                    (soft_cursor.apply_delta(dx, dy, union.as_ref()), union)
                {
                    tx.send(WlrCmd::MotionAbs {
                        x: nx,
                        y: ny,
                        union: rect,
                    })
                    .is_err()
                } else {
                    tx.send(WlrCmd::MotionDelta { dx, dy }).is_err()
                }
            }
            LinuxBackend::Portal { tx, soft_cursor } => {
                let _ = soft_cursor.apply_delta(dx, dy, union.as_ref());
                tx.send(PortalCmd::MotionDelta { dx, dy }).is_err()
            }
            LinuxBackend::X11(x11) => {
                let _ = xtest::fake_input(
                    &x11.conn,
                    xproto::MOTION_NOTIFY_EVENT,
                    1,
                    x11rb::CURRENT_TIME,
                    x11rb::NONE,
                    dx.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                    dy.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                    0,
                );
                let _ = x11.conn.flush();
                false
            }
        });
        if matches!(dropped, Some(true)) {
            if let Ok(mut guard) = LINUX_BACKEND.lock() {
                *guard = None;
            }
        }
    }

    pub fn inject_mouse_button(button: u8, is_down: bool) -> (u32, u32) {
        use x11rb::connection::Connection as _;
        use x11rb::protocol::{xproto, xtest};

        let Some(evdev_code) = map_button_to_evdev(button) else {
            return (0, 0);
        };
        let mut sent = (0u32, 0u32);
        let dropped = with_backend(|backend| match backend {
            LinuxBackend::Wlroots { tx, .. } => {
                let err = tx
                    .send(WlrCmd::Button {
                        evdev_code,
                        is_down,
                    })
                    .is_err();
                if !err {
                    sent = (1, 0);
                }
                err
            }
            LinuxBackend::Portal { tx, .. } => {
                let err = tx
                    .send(PortalCmd::Button {
                        evdev_code,
                        is_down,
                    })
                    .is_err();
                if !err {
                    sent = (1, 0);
                }
                err
            }
            LinuxBackend::X11(x11) => {
                let x_btn = match button {
                    1 => 1u8,
                    2 => 3u8,
                    3 => 2u8,
                    _ => return false,
                };
                let ev_type = if is_down {
                    xproto::BUTTON_PRESS_EVENT
                } else {
                    xproto::BUTTON_RELEASE_EVENT
                };
                if xtest::fake_input(
                    &x11.conn,
                    ev_type,
                    x_btn,
                    x11rb::CURRENT_TIME,
                    x11rb::NONE,
                    0,
                    0,
                    0,
                )
                .is_ok()
                {
                    let _ = x11.conn.flush();
                    sent = (1, 0);
                }
                false
            }
        });
        if matches!(dropped, Some(true)) {
            if let Ok(mut guard) = LINUX_BACKEND.lock() {
                *guard = None;
            }
        }
        sent
    }

    pub fn inject_mouse_wheel(delta: i32) {
        use x11rb::connection::Connection as _;
        use x11rb::protocol::{xproto, xtest};

        let dropped = with_backend(|backend| match backend {
            LinuxBackend::Wlroots { tx, .. } => tx.send(WlrCmd::Wheel { delta }).is_err(),
            LinuxBackend::Portal { tx, .. } => tx.send(PortalCmd::Wheel { delta }).is_err(),
            LinuxBackend::X11(x11) => {
                let clicks = x11.scroll.push_x11_clicks(delta);
                if clicks != 0 {
                    let btn = if clicks > 0 { 4u8 } else { 5u8 };
                    for _ in 0..clicks.unsigned_abs().min(32) {
                        let _ = xtest::fake_input(
                            &x11.conn,
                            xproto::BUTTON_PRESS_EVENT,
                            btn,
                            x11rb::CURRENT_TIME,
                            x11rb::NONE,
                            0,
                            0,
                            0,
                        );
                        let _ = xtest::fake_input(
                            &x11.conn,
                            xproto::BUTTON_RELEASE_EVENT,
                            btn,
                            x11rb::CURRENT_TIME,
                            x11rb::NONE,
                            0,
                            0,
                            0,
                        );
                    }
                    let _ = x11.conn.flush();
                }
                false
            }
        });
        if matches!(dropped, Some(true)) {
            if let Ok(mut guard) = LINUX_BACKEND.lock() {
                *guard = None;
            }
        }
    }

    pub fn inject_key(key_code: u16, scan_code: u16, is_down: bool, extended: bool) -> (u32, u32) {
        use x11rb::connection::Connection as _;
        use x11rb::protocol::{xproto, xtest};

        let Some(evdev) = win_key_to_linux_evdev(key_code, scan_code, extended) else {
            return (0, 0);
        };
        let mut sent = (0u32, 0u32);
        let dropped = with_backend(|backend| match backend {
            LinuxBackend::Wlroots { tx, .. } => {
                let err = tx.send(WlrCmd::Key { evdev, is_down }).is_err();
                if !err {
                    sent = (1, 0);
                }
                err
            }
            LinuxBackend::Portal { tx, .. } => {
                let err = tx.send(PortalCmd::Key { evdev, is_down }).is_err();
                if !err {
                    sent = (1, 0);
                }
                err
            }
            LinuxBackend::X11(x11) => {
                if let Ok(keycode) = u8::try_from(u32::from(evdev) + 8) {
                    let ev_type = if is_down {
                        xproto::KEY_PRESS_EVENT
                    } else {
                        xproto::KEY_RELEASE_EVENT
                    };
                    if xtest::fake_input(
                        &x11.conn,
                        ev_type,
                        keycode,
                        x11rb::CURRENT_TIME,
                        x11rb::NONE,
                        0,
                        0,
                        0,
                    )
                    .is_ok()
                    {
                        let _ = x11.conn.flush();
                        sent = (1, 0);
                    }
                }
                false
            }
        });
        if matches!(dropped, Some(true)) {
            if let Ok(mut guard) = LINUX_BACKEND.lock() {
                *guard = None;
            }
        }
        sent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_visibility_nudge_triggers_only_when_hidden_suppressed_or_unknown() {
        let visible = CursorVisibilityInfo::from_flags_and_handle(CURSOR_FLAG_SHOWING, 0x1);
        assert!(visible.showing);
        assert!(!visible.suppressed);
        assert!(!cursor_visibility_needs_nudge(Some(visible)));

        let hidden = CursorVisibilityInfo::from_flags(0);
        assert!(!hidden.showing);
        assert!(cursor_visibility_needs_nudge(Some(hidden)));

        let visible_null_hcursor =
            CursorVisibilityInfo::from_flags_and_handle(CURSOR_FLAG_SHOWING, 0);
        assert!(visible_null_hcursor.hcursor_null());
        assert!(cursor_visibility_needs_nudge(Some(visible_null_hcursor)));

        let visible_valid =
            CursorVisibilityInfo::from_flags_and_handle(CURSOR_FLAG_SHOWING, 0x1001);
        assert!(!visible_valid.hcursor_null());
        assert!(!cursor_visibility_needs_nudge(Some(visible_valid)));

        let suppressed =
            CursorVisibilityInfo::from_flags(CURSOR_FLAG_SHOWING | CURSOR_FLAG_SUPPRESSED);
        assert!(suppressed.suppressed);
        assert!(cursor_visibility_needs_nudge(Some(suppressed)));

        assert!(cursor_visibility_needs_nudge(None));
    }

    #[test]
    fn cursor_info_log_deduplicates_identical_flags_and_hcursor() {
        let mut last = None;
        let a = Some(CursorVisibilityInfo::from_flags_and_handle(
            CURSOR_FLAG_SHOWING,
            0x1001,
        ));
        let b = Some(CursorVisibilityInfo::from_flags_and_handle(
            CURSOR_FLAG_SHOWING,
            0x2002,
        ));
        let c = Some(CursorVisibilityInfo::from_flags_and_handle(0, 0x2002));

        assert!(should_log_cursor_info(&mut last, a));
        assert!(!should_log_cursor_info(&mut last, a));
        assert!(should_log_cursor_info(&mut last, b));
        assert!(!should_log_cursor_info(&mut last, b));
        assert!(should_log_cursor_info(&mut last, c));
        assert!(should_log_cursor_info(&mut last, None));
        assert!(!should_log_cursor_info(&mut last, None));
    }

    #[test]
    fn post_unlock_nudge_flag_is_one_shot() {
        let _ = take_post_unlock_nudge_pending();
        mark_post_unlock_nudge_pending();
        assert!(take_post_unlock_nudge_pending());
        assert!(!take_post_unlock_nudge_pending());
    }

    #[test]
    fn macos_vk_and_scancode_mapping_covers_keys_modifiers_and_extended() {
        use macos_logic::{win_key_to_macos_cg_keycode, win_scan_code_to_macos_cg_keycode};

        // Letters & digits
        assert_eq!(win_key_to_macos_cg_keycode(0x41, 0x1E, false), Some(0x00)); // A
        assert_eq!(win_key_to_macos_cg_keycode(0x5A, 0x2C, false), Some(0x06)); // Z
        assert_eq!(win_key_to_macos_cg_keycode(0x30, 0x0B, false), Some(0x1D)); // 0
        assert_eq!(win_key_to_macos_cg_keycode(0x39, 0x0A, false), Some(0x19)); // 9

        // Navigation, editing & Return vs Numpad Enter
        assert_eq!(win_key_to_macos_cg_keycode(0x08, 0x0E, false), Some(0x33)); // Backspace -> kVK_Delete
        assert_eq!(win_key_to_macos_cg_keycode(0x2E, 0x53, true), Some(0x75)); // Delete -> kVK_ForwardDelete
        assert_eq!(win_key_to_macos_cg_keycode(0x0D, 0x1C, false), Some(0x24)); // Return
        assert_eq!(win_key_to_macos_cg_keycode(0x0D, 0x1C, true), Some(0x4C)); // KeypadEnter
        assert_eq!(win_key_to_macos_cg_keycode(0x25, 0x4B, true), Some(0x7B)); // LeftArrow

        // 1:1 Modifiers: Win -> Command, Alt -> Option, Ctrl -> Control
        assert_eq!(win_key_to_macos_cg_keycode(0x5B, 0x5B, true), Some(0x37)); // LWin -> kVK_Command
        assert_eq!(win_key_to_macos_cg_keycode(0x5C, 0x5C, true), Some(0x36)); // RWin -> kVK_RightCommand
        assert_eq!(win_key_to_macos_cg_keycode(0xA4, 0x38, false), Some(0x3A)); // LAlt -> kVK_Option
        assert_eq!(win_key_to_macos_cg_keycode(0xA5, 0x38, true), Some(0x3D)); // RAlt -> kVK_RightOption
        assert_eq!(win_key_to_macos_cg_keycode(0xA2, 0x1D, false), Some(0x3B)); // LCtrl -> kVK_Control
        assert_eq!(win_key_to_macos_cg_keycode(0xA3, 0x1D, true), Some(0x3E)); // RCtrl -> kVK_RightControl
        assert_eq!(win_key_to_macos_cg_keycode(0x10, 0x2A, false), Some(0x38)); // generic LShift
        assert_eq!(win_key_to_macos_cg_keycode(0x10, 0x36, false), Some(0x3C)); // generic RShift
        assert_eq!(win_key_to_macos_cg_keycode(0x11, 0x1D, true), Some(0x3E)); // generic RCtrl
        assert_eq!(win_key_to_macos_cg_keycode(0x12, 0x38, true), Some(0x3D)); // generic RAlt

        // Scan-code fallback when key_code == 0
        assert_eq!(win_key_to_macos_cg_keycode(0, 0x1E, false), Some(0x00)); // 'A'
        assert_eq!(win_key_to_macos_cg_keycode(0, 0x4B, true), Some(0x7B)); // Left arrow
        assert_eq!(win_scan_code_to_macos_cg_keycode(0x01, false), Some(0x35)); // Escape
        assert_eq!(win_key_to_macos_cg_keycode(0xB3, 0, false), None); // Media key ignored
    }

    #[test]
    fn macos_modifier_state_tracks_sides_and_capslock_toggle() {
        use macos_logic::{
            MacModifierState, KCG_EVENT_FLAG_MASK_ALPHA_SHIFT, KCG_EVENT_FLAG_MASK_COMMAND,
            KCG_EVENT_FLAG_MASK_NUMERIC_PAD, KCG_EVENT_FLAG_MASK_SHIFT,
        };

        let mut mods = MacModifierState::default();
        assert_eq!(mods.flags(), 0);

        mods.update(0x38, true); // LShift down
        mods.update(0x3C, true); // RShift down
        assert_eq!(mods.flags(), KCG_EVENT_FLAG_MASK_SHIFT);

        mods.update(0x38, false); // LShift up -> RShift still held
        assert_eq!(mods.flags(), KCG_EVENT_FLAG_MASK_SHIFT);
        mods.update(0x3C, false); // RShift up
        assert_eq!(mods.flags(), 0);

        mods.update(0x37, true); // Command down
        assert_eq!(mods.flags(), KCG_EVENT_FLAG_MASK_COMMAND);
        assert_eq!(
            mods.flags_for_key(0x7B), // LeftArrow gets NumericPad flag too
            KCG_EVENT_FLAG_MASK_COMMAND | KCG_EVENT_FLAG_MASK_NUMERIC_PAD
        );
        mods.update(0x37, false);

        // CapsLock toggles on down only
        mods.update(0x39, true);
        assert_eq!(mods.flags(), KCG_EVENT_FLAG_MASK_ALPHA_SHIFT);
        mods.update(0x39, false);
        assert_eq!(mods.flags(), KCG_EVENT_FLAG_MASK_ALPHA_SHIFT);
        mods.update(0x39, true);
        assert_eq!(mods.flags(), 0);
    }

    #[test]
    fn macos_click_tracker_wheel_scaling_and_drag_event_selection() {
        use macos_logic::{
            macos_mouse_move_event_type, scale_macos_wheel_delta_px, ClickStateTracker,
            KCG_EVENT_LEFT_MOUSE_DRAGGED, KCG_EVENT_MOUSE_MOVED, KCG_EVENT_OTHER_MOUSE_DRAGGED,
            KCG_EVENT_RIGHT_MOUSE_DRAGGED, KCG_MOUSE_BUTTON_CENTER, KCG_MOUSE_BUTTON_LEFT,
            KCG_MOUSE_BUTTON_RIGHT,
        };

        let mut ct = ClickStateTracker::default();
        assert_eq!(ct.on_button(1, true, (100, 100), 1000), 1);
        assert_eq!(ct.on_button(1, false, (100, 100), 1040), 1);
        assert_eq!(ct.on_button(1, true, (102, 99), 1200), 2);
        assert_eq!(ct.on_button(1, false, (102, 99), 1230), 2);
        assert_eq!(ct.on_button(1, true, (102, 99), 1400), 3);
        // Moved too far -> resets streak
        assert_eq!(ct.on_button(1, true, (110, 99), 1500), 1);
        // Too slow -> resets streak
        assert_eq!(ct.on_button(1, true, (110, 99), 2500), 1);
        // Different button -> resets streak
        assert_eq!(ct.on_button(2, true, (110, 99), 2600), 1);

        assert_eq!(scale_macos_wheel_delta_px(0), 0);
        assert_eq!(scale_macos_wheel_delta_px(120), 40);
        assert_eq!(scale_macos_wheel_delta_px(-120), -40);
        assert_eq!(scale_macos_wheel_delta_px(1), 1);
        assert_eq!(scale_macos_wheel_delta_px(-1), -1);

        assert_eq!(
            macos_mouse_move_event_type(0),
            (KCG_EVENT_MOUSE_MOVED, KCG_MOUSE_BUTTON_LEFT)
        );
        assert_eq!(
            macos_mouse_move_event_type(1),
            (KCG_EVENT_LEFT_MOUSE_DRAGGED, KCG_MOUSE_BUTTON_LEFT)
        );
        assert_eq!(
            macos_mouse_move_event_type(2),
            (KCG_EVENT_RIGHT_MOUSE_DRAGGED, KCG_MOUSE_BUTTON_RIGHT)
        );
        assert_eq!(
            macos_mouse_move_event_type(4),
            (KCG_EVENT_OTHER_MOUSE_DRAGGED, KCG_MOUSE_BUTTON_CENTER)
        );
    }

    #[test]
    fn self_injected_cg_user_data_tag_matches_exact_magic_only() {
        assert!(is_self_injected_cg_user_data(ANYLEAP_CG_USER_DATA));
        assert!(!is_self_injected_cg_user_data(0));
        assert!(!is_self_injected_cg_user_data(-1));
        assert!(!is_self_injected_cg_user_data(ANYLEAP_CG_USER_DATA ^ 1));
    }

    #[test]
    fn linux_evdev_keymap_covers_keys_modifiers_extended_and_scancodes() {
        use linux_logic::win_key_to_linux_evdev;

        assert_eq!(win_key_to_linux_evdev(0x41, 0, false), Some(30)); // A
        assert_eq!(win_key_to_linux_evdev(0x51, 0, false), Some(16)); // Q
        assert_eq!(win_key_to_linux_evdev(0x5A, 0, false), Some(44)); // Z
        assert_eq!(win_key_to_linux_evdev(0x31, 0, false), Some(2)); // 1
        assert_eq!(win_key_to_linux_evdev(0x30, 0, false), Some(11)); // 0

        // 1:1 modifiers
        assert_eq!(win_key_to_linux_evdev(0xA0, 0, false), Some(42)); // LShift
        assert_eq!(win_key_to_linux_evdev(0xA1, 0, false), Some(54)); // RShift
        assert_eq!(win_key_to_linux_evdev(0xA2, 0, false), Some(29)); // LCtrl
        assert_eq!(win_key_to_linux_evdev(0xA3, 0, true), Some(97)); // RCtrl
        assert_eq!(win_key_to_linux_evdev(0x11, 0, true), Some(97)); // Ctrl extended -> RCtrl
        assert_eq!(win_key_to_linux_evdev(0x11, 0, false), Some(29)); // Ctrl -> LCtrl
        assert_eq!(win_key_to_linux_evdev(0xA4, 0, false), Some(56)); // LAlt
        assert_eq!(win_key_to_linux_evdev(0xA5, 0, true), Some(100)); // RAlt
        assert_eq!(win_key_to_linux_evdev(0x12, 0, true), Some(100)); // Menu extended -> RAlt
        assert_eq!(win_key_to_linux_evdev(0x12, 0, false), Some(56)); // Menu -> LAlt
        assert_eq!(win_key_to_linux_evdev(0x5B, 0, true), Some(125)); // LWin -> LeftMeta
        assert_eq!(win_key_to_linux_evdev(0x5C, 0, true), Some(126)); // RWin -> RightMeta
        assert_eq!(win_key_to_linux_evdev(0x14, 0, false), Some(58)); // CapsLock

        // Enter vs KeypadEnter, navigation, F-keys, numpad, and scancode fallback
        assert_eq!(win_key_to_linux_evdev(0x0D, 0, false), Some(28));
        assert_eq!(win_key_to_linux_evdev(0x0D, 0, true), Some(96));
        assert_eq!(win_key_to_linux_evdev(0x25, 0, true), Some(105)); // Left
        assert_eq!(win_key_to_linux_evdev(0x26, 0, true), Some(103)); // Up
        assert_eq!(win_key_to_linux_evdev(0x27, 0, true), Some(106)); // Right
        assert_eq!(win_key_to_linux_evdev(0x28, 0, true), Some(108)); // Down
        assert_eq!(win_key_to_linux_evdev(0x2E, 0, true), Some(111)); // Delete
        assert_eq!(win_key_to_linux_evdev(0x08, 0, false), Some(14)); // Backspace
        assert_eq!(win_key_to_linux_evdev(0x70, 0, false), Some(59)); // F1
        assert_eq!(win_key_to_linux_evdev(0x7A, 0, false), Some(87)); // F11
        assert_eq!(win_key_to_linux_evdev(0x7B, 0, false), Some(88)); // F12
        assert_eq!(win_key_to_linux_evdev(0x60, 0, false), Some(82)); // KP0
        assert_eq!(win_key_to_linux_evdev(0, 0x1E, false), Some(30)); // Scancode A
        assert_eq!(win_key_to_linux_evdev(0, 0x4B, true), Some(105)); // Extended scancode Left
    }

    #[test]
    fn linux_modifier_tracker_tracks_sides_xkb_masks_and_capslock_lock() {
        use linux_logic::LinuxModifierTracker;

        let mut tracker = LinuxModifierTracker::default();
        assert_eq!(tracker.update(29, true), (0x04, 0)); // LCtrl down
        assert_eq!(tracker.update(97, true), (0x04, 0)); // RCtrl down too
        assert_eq!(tracker.update(29, false), (0x04, 0)); // LCtrl up -> RCtrl keeps Ctrl bit
        assert_eq!(tracker.update(97, false), (0x00, 0)); // Both Ctrl up

        assert_eq!(tracker.update(42, true), (0x01, 0)); // LShift
        assert_eq!(tracker.update(56, true), (0x09, 0)); // + LAlt
        assert_eq!(tracker.update(125, true), (0x49, 0)); // + LSuper
        assert_eq!(tracker.update(58, true), (0x49, 0x02)); // CapsLock press toggles lock ON
        assert_eq!(tracker.update(58, false), (0x49, 0x02)); // CapsLock release keeps lock
        assert_eq!(tracker.update(58, true), (0x49, 0x00)); // Second press toggles lock OFF
        assert_eq!(tracker.clear(), (0, 0));
    }

    #[test]
    fn linux_pointer_normalization_scroll_inversion_xkb_keymap_and_soft_cursor() {
        use crate::kvm::protocol::ScreenRect;
        use linux_logic::{
            build_us_evdev_xkb_keymap, normalize_linux_abs_pointer, LinuxScrollAccumulator,
            SoftCursorTracker,
        };

        let union = ScreenRect {
            x: -1920,
            y: 0,
            width: 3840,
            height: 1080,
        };
        assert_eq!(
            normalize_linux_abs_pointer(-1920, 0, &union),
            Some((0, 0, 3840, 1080))
        );
        assert_eq!(
            normalize_linux_abs_pointer(0, 540, &union),
            Some((1920, 540, 3840, 1080))
        );
        assert_eq!(
            normalize_linux_abs_pointer(2500, 2000, &union),
            Some((3839, 1079, 3840, 1080))
        );
        assert_eq!(
            normalize_linux_abs_pointer(
                0,
                0,
                &ScreenRect {
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 1080
                }
            ),
            None
        );

        let mut scroll = LinuxScrollAccumulator::default();
        // Win32 +120 = scroll UP -> Wayland negative axis & negative discrete step
        let (axis_up, disc_up) = scroll.push_wayland(120);
        assert!((axis_up + 15.0).abs() < 1e-6);
        assert_eq!(disc_up, -1);

        let (axis_down, disc_down) = scroll.push_wayland(-120);
        assert!((axis_down - 15.0).abs() < 1e-6);
        assert_eq!(disc_down, 1);

        // Two half-detents accumulate cleanly
        let (half1, d1) = scroll.push_wayland(60);
        assert!((half1 + 7.5).abs() < 1e-6);
        assert_eq!(d1, 0);
        let (half2, d2) = scroll.push_wayland(60);
        assert!((half2 + 7.5).abs() < 1e-6);
        assert_eq!(d2, -1);

        assert_eq!(scroll.push_x11_clicks(120), 1);
        assert_eq!(scroll.push_x11_clicks(-120), -1);

        let keymap = build_us_evdev_xkb_keymap();
        assert_eq!(keymap.last(), Some(&0u8));
        let text = std::str::from_utf8(&keymap[..keymap.len() - 1]).expect("valid utf8 xkb");
        assert!(text.contains("xkb_keymap"));
        assert!(text.contains("evdev+aliases(qwerty)"));
        assert!(text.contains("pc+us+inet(evdev)"));

        let mut cursor = SoftCursorTracker::default();
        assert_eq!(cursor.get(), None);
        cursor.set(100, 200);
        assert_eq!(cursor.get(), Some((100, 200)));
        assert_eq!(cursor.apply_delta(30, -50, Some(&union)), Some((130, 150)));
        assert_eq!(
            cursor.apply_delta(-500, 2000, Some(&union)),
            Some((-370, 1079))
        );
        assert_eq!(
            cursor.apply_delta(-3000, -2000, Some(&union)),
            Some((-1920, 0))
        );
    }
}
