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
    #[cfg(not(target_os = "macos"))]
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
    #[cfg(not(any(windows, target_os = "macos")))]
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
    #[cfg(not(any(windows, target_os = "macos")))]
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
    #[cfg(not(any(windows, target_os = "macos")))]
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
    #[cfg(not(any(windows, target_os = "macos")))]
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
    #[cfg(not(any(windows, target_os = "macos")))]
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
}
