# SPEC-29: macOS Main PC Input Capture & Trap (Virtual KVM Milestone 2)

> **Status:** Implemented / Pending Two-PC Hardware Verification
> **Scope:** `(kvm)` — Enables macOS to act as a **Main PC** (Controller) in **Shared Mouse & Keyboard**, controlling Windows, macOS (`SPEC-28`), or future Linked PCs.
> **Depends on:** `SPEC-28` (CoreGraphics logical-point display enumeration, `check_accessibility_permission`, private `CGEventSource` tagging, and computer name/download directory hygiene).

---

## 1. Goal & Scope

Allow AnyLeap running on macOS to capture local mouse and keyboard input via Quartz Event Services (`CGEventTap`), detect seam/edge crossings across the active desktop layout, trap and forward mouse deltas, clicks, scrolls, and keystrokes to a Linked PC over the existing authenticated QUIC link, and cleanly release control on `CursorReturn` or `EmergencyRelease`.

### In Scope
- macOS global input capture & trapping in `src-tauri/src/kvm/hooks.rs` (`HookController::start_hooks`, `stop_hooks`, `handle_cursor_return`, `emergency_release`, cursor hide/restore/warp).
- Reverse key & modifier mapping from macOS `CGKeyCode` / `CGEventFlags` (`kCGEventKeyDown`, `kCGEventKeyUp`, `kCGEventFlagsChanged`) to Windows `VK_*` + PS/2 Set 1 `scan_code` + `extended` for `KvmPacket::KeyEvent`.
- Smooth macOS trackpad & mouse scroll scaling (`kCGEventScrollWheel` continuous and discrete ticks -> Win32 `WHEEL_DELTA` units).
- Trapped relative mouse delta computation resilient to background app focus, `CGWarpMouseCursorPosition` settle jumps, and fractional trackpad sub-point accumulation.
- Self-injection filtering via `kCGEventSourceUserData` tag shared between `injector.rs` and `hooks.rs`.
- Permission preflight & graceful non-blocking fallback notice when **Accessibility** (or **Input Monitoring**) trust is missing.
- Bidirectional macOS Emergency Release hotkey (`Ctrl + Option + Esc`, plus `Scroll Lock` and `Pause` for external PC keyboards).

### Out of Scope
- macOS Continuity capture (`clipboard.rs`, `dragdrop.rs`, `audio.rs`, `media_keys.rs`, `session_sync.rs`) — stays Windows-only (`#[cfg(windows)]`).
- macOS `wheel_swipe.rs` scrcpy scroll-to-fling hook (separate Android mirroring feature).
- Settings/UI schema changes for custom modifier remapping (keeps 1:1 `Command ↔ Win`, `Option ↔ Alt`, `Control ↔ Ctrl` symmetric with `SPEC-28`).
- Linux Main PC / Linked PC adapters (`SPEC-30`, next milestone).

---

## 2. Approaches Considered

| Approach | How it works | Trade-offs | Pick |
|---|---|---|---|
| **A. Quartz `CGEventTap` + `CGWarpMouseCursorPosition` + `NSRunLoop`/`CFRunLoop`** | Active filter at `kCGSessionEventTap` (`kCGHeadInsertEventTap`, `kCGEventTapOptionDefault`). Reads edge crossings while untrapped; swallows events (`NULL`), hides/warps cursor to trap center, and accumulates deltas relative to trap anchor when trapped. | Requires Accessibility trust (same grant already requested by `SPEC-28`); subject to WindowServer keeping cursor hidden only while foreground, mitigated by pinning cursor at monitor trap center and dropping local events cleanly. | **Picked** |
| **B. Pure `CGAssociateMouseAndMouseCursorPosition(false)` decoupling** | Disassociates mouse hardware from cursor rendering and reads `kCGMouseEventDeltaX/Y`. | Apple CoreGraphics restricts cursor disassociation to foreground applications only (`CGRemoteOperation.h`); fails whenever the user switches into Chrome/Xcode/Finder on their Mac before leaping. | Rejected |
| **C. `IOHIDManager` raw HID capture + separate swallow layer** | Reads raw USB/Bluetooth mouse/trackpad HID reports directly. | Cannot swallow clicks or keystrokes destined for macOS apps on its own, still needs an active `CGEventTap` to suppress local input, and doesn't track WindowServer pointer acceleration/logical point coordinates consistently with `monitors.rs`. | Rejected |

---

## 3. Detailed Design

### 3.1 Architecture & Threading Model (`hooks.rs`)
- **Dedicated RunLoop Thread:** On `HookController::start_hooks()`, macOS spawns a dedicated thread (`kvm-macos-event-tap`) hosting a `CFRunLoop`:
  - Creates a `CGEventTapCreate(kCGSessionEventTap, kCGHeadInsertEventTap, kCGEventTapOptionDefault, mask, callback, null)`.
  - `mask` covers: mouse move (`kCGEventMouseMoved`), left/right/other mouse down/up/dragged, scroll wheel (`kCGEventScrollWheel`), key down/up (`kCGEventKeyDown`, `kCGEventKeyUp`), and modifier flag changes (`kCGEventFlagsChanged`).
  - Creates two `CFRunLoopSource` (`CFRunLoopSourceCreate`) wakeup sources attached to that thread's `CFRunLoop`:
    1. **`engage` source:** signaled by `request_engage_leap` inside the tap callback to run `perform_macos_engage_leap()` safely outside callback stack frames.
    2. **`restore` source:** signaled by `notify_hook_thread_restore` / `handle_cursor_return` / `emergency_release` to execute cursor unhide (`CGDisplayShowCursor`) and optional `CursorReturn` warp (`CGWarpMouseCursorPosition(rx, ry)`) on the exact same `CFRunLoop` thread that owns the tap.
  - Handshake via a bounded `mpsc::sync_channel::<Result<(), &'static str>>(1)`: `start_hooks()` waits synchronously for the `CFRunLoop` thread to confirm tap creation succeeded before setting `HOOK_ACTIVE = true` and returning `true` (or returning `false` on permission failure without leaving stale state).
- **`stop_hooks()` Teardown:** Calls `emergency_release()`, disables and invalidates the `CFMachPortRef` tap, signals `CFRunLoopStop`, and joins cleanly when convenient without blocking UI exit.
- **Tap Timeout Recovery:** When the callback receives `kCGEventTapDisabledByTimeout` or `kCGEventTapDisabledByUserInput`, it immediately calls `CGEventTapEnable(tap, true)` (unless `!HOOK_ACTIVE`) so heavy system load never permanently kills input capture. Every tap callback uses only atomics, `try_lock()`, and `try_send()`.

### 3.2 Permission Flow (`injector.rs`, `hooks.rs`, `kvm/commands.rs`)
- Expose `injector::check_input_monitoring_permission(prompt: bool) -> bool` wrapping CoreGraphics `CGPreflightListenEventAccess` / `CGRequestListenEventAccess`.
- In `HookController::start_hooks()` on macOS:
  1. Ensure Accessibility trust via `injector::check_accessibility_permission(true)`.
  2. Attempt `CGEventTapCreate(...)` on the `CFRunLoop` thread.
  3. If tap creation returns `NULL`, check `injector::check_input_monitoring_permission(true)` so modern macOS builds requiring ListenEvent access also trigger a prompt once, and return `false`.
- In `kvm/commands.rs::apply_controller_topology`:
  - On `#[cfg(target_os = "macos")]`, if `start_hooks()` returns `false`, emit a one-shot `kvm-continuity-notice` guiding the user to enable AnyLeap in **System Settings → Privacy & Security → Accessibility** (and **Input Monitoring** if shown).
  - On `#[cfg(not(any(windows, target_os = "macos")))]`, preserve the `"Main PC input capture is not supported on this OS yet. This machine can still act as a Linked PC."` notice.

### 3.3 Self-Injection Tagging (`injector.rs` & `hooks.rs`)
- `injector.rs` tags its shared private `CGEventSource` via `CGEventSourceSetUserData(source, ANYLEAP_CG_USER_DATA)` (`pub const ANYLEAP_CG_USER_DATA: i64 = 0x414E_594C_4B56_4D31`, `"ANYLKVM1"`).
- Inside `hooks.rs`'s `CGEventTap` callback, any event where `CGEventGetIntegerValueField(event, kCGEventSourceUserData) == ANYLEAP_CG_USER_DATA` is immediately passed through untouched (`return event`) — identical to `LLKHF_INJECTED` / `LLMHF_INJECTED` filtering on Windows, preventing `release_keys()` on leap engage from re-entering controller hooks.

### 3.4 Untrapped Edge Detection & Trapped Cursor/Delta Engine (`hooks.rs`)
- **Untrapped (`!IS_TRAPPED`):**
  - Reads `pt = CGEventGetLocation(event)` (CoreGraphics global logical points matching `monitors.rs`), tracks physical mouse button bitmask (`MOUSE_BUTTONS_DOWN`), and skips edge leaping while any mouse button is held (preserving normal macOS dragging).
  - Runs the existing `detect_controller_edge_seams` / `detect_controller_edge` + neighbor-monitor check (`monitor_at_point(&layout.main_monitors, neighbor.0, neighbor.1)`) + `check_dwell_resistance(resistance_ms)`.
  - On dwell hit: calls `request_engage_leap(...)`, swallows the edge-crossing event (`NULL`), and wakes the `engage` source.
- **On Engage (`perform_macos_engage_leap`):**
  - Sets `CURSOR_CENTERED = true`, hides cursor with `CGDisplayHideCursor(CGMainDisplayID())` (balanced 1:1 with `CGDisplayShowCursor` tracked via `MACOS_CURSOR_HIDDEN: AtomicBool` so counters never drift), arms the delta tracker with `(center_x, center_y)` and the `leap_edge_pt`, and calls `CGWarpMouseCursorPosition(CGPoint { x: center_x, y: center_y })`.
  - Releases locally held non-mouse keys tracked in `CONTROLLER_TRACKER` via tagged `injector::inject_key(..., false, ...)` and emits `KvmPacket::CursorEnter`.
- **Trapped Motion (`IS_TRAPPED == true`):**
  - Pure unit-testable `MacTapDeltaTracker`:
    - Handles **pre-warp flush** (within the initial 25 ms settle window after engage, any event whose location is still near `leap_edge_pt` rather than `center` is dropped cleanly so the jump to `center` never sends a full-screen phantom delta).
    - Computes fractional sub-point delta `(dx_f, dy_f) = (pt.x - center.x, pt.y - center.y)` added to a residual accumulator `(acc_x, acc_y)`, emitting truncated integer `(dx, dy)` when non-zero and retaining sub-point remainders so slow trackpad micro-movements remain responsive.
    - Immediately warps cursor back to `(center_x, center_y)` via `CGWarpMouseCursorPosition` whenever `(pt.x - center.x).abs() >= 0.5 || (pt.y - center.y).abs() >= 0.5`, using a warp-origin re-sync check (if WindowServer emits back-to-back buffered moves before warp takes effect or emits synthetic delta fields, our anchor tracking uses absolute `CGEventGetLocation` rather than `kCGMouseEventDeltaX/Y` spikes).
    - Swallows the event (`return NULL`) so macOS apps underneath do not receive hover or drag focus while trapped.
- **Trapped Mouse Buttons & Scroll:**
  - Buttons `1` (Left), `2` (Right), `3` (Middle via `kCGMouseEventButtonNumber == 2`) emit `KvmPacket::MouseButton { button, is_down }` and return `NULL`.
  - `kCGEventScrollWheel` extracts continuous trackpad points (`kCGScrollWheelEventIsContinuous != 0` via `kCGScrollWheelEventPointDeltaAxis1`, scaled by `120 / 40 = 3` with fractional remainder accumulator matching the exact inverse of `SPEC-28`) or discrete wheel ticks (`kCGScrollWheelEventDeltaAxis1 * 120`), emits `KvmPacket::MouseWheel { delta }`, and returns `NULL`.

### 3.5 Reverse Keyboard & Modifier Mapping + Emergency Release (`hooks.rs`)
- Pure unit-testable `macos_cg_keycode_to_win_key(cg_keycode: u16) -> Option<(u16, u16, bool)>`:
  - Exact inverse of `SPEC-28`'s `win_key_to_macos_cg_keycode`, emitting `(vk_code, set1_scan_code, extended)` so Windows `SendInput` (which sets `KEYEVENTF_SCANCODE` whenever `scan_code > 0`) as well as macOS/Linux receivers decode every key identically.
  - Uses symmetric **1:1 modifier mapping**:
    - `kVK_Command` (`0x37`) / `kVK_RightCommand` (`0x36`) ↔ `VK_LWIN` (`0x5B`, scan `0x5B`, `extended=true`) / `VK_RWIN` (`0x5C`, scan `0x5C`, `extended=true`)
    - `kVK_Option` (`0x3A`) / `kVK_RightOption` (`0x3D`) ↔ `VK_LMENU` (`0xA4`, scan `0x38`, `extended=false`) / `VK_RMENU` (`0xA5`, scan `0x38`, `extended=true`)
    - `kVK_Control` (`0x3B`) / `kVK_RightControl` (`0x3E`) ↔ `VK_LCONTROL` (`0xA2`, scan `0x1D`, `extended=false`) / `VK_RCONTROL` (`0xA3`, scan `0x1D`, `extended=true`)
    - `kVK_Shift` (`0x38`) / `kVK_RightShift` (`0x3C`) ↔ `VK_LSHIFT` (`0xA0`, scan `0x2A`, `extended=false`) / `VK_RSHIFT` (`0xA1`, scan `0x36`, `extended=false`)
    - `kVK_CapsLock` (`0x39`) ↔ `VK_CAPITAL` (`0x14`, scan `0x3A`, `extended=false`)
- Pure `MacTapModifierTracker::on_flags_changed(cg_keycode: u16, flags: u64)` resolves exact down/up transitions using Apple `IOLLEvent.h` device-dependent left/right masks (`NX_DEVICELSHIFTKEYMASK`..`NX_DEVICERCTLKEYMASK`), falling back to device-independent masks when lower bits are absent (e.g., virtual keyboards), and emits clean press+release pairs on `AlphaShift` toggle transitions.
- **Emergency Release Hotkeys:**
  - While active (`HOOK_ACTIVE`), pressing `kVK_Escape` (`0x35`) with both `Control` and `Option` held (`Ctrl + Option + Esc`, the 1:1 counterpart of Windows `Ctrl + Alt + Esc` without conflicting with macOS `Cmd + Option + Esc` Force Quit), OR pressing `Scroll Lock` / `Pause` on external PC keyboards (`kVK_F14`? No — only real `VK_SCROLL` / `VK_PAUSE` if HID sends them, keeping `F14`/`F15` usable as normal keys on Mac keyboards), triggers `disarm_trapped_in_callback()`, clears modifiers/tracker, schedules cursor restore, and emits `EmergencyRelease` + `ReleaseAll`.

### 3.6 `CursorReturn` & Safety Recovery
- `handle_cursor_return` calculates `(rx, ry)` via existing `map_controller_return_pos` and queues a return warp on the macOS tap thread (`CGDisplayShowCursor` + `CGWarpMouseCursorPosition((rx, ry))`) so cursor seamlessly re-enters the Mac at the shared seam point without stuck invisibility.
- Panic guard inside the tap callback (`std::panic::catch_unwind`) mirrors Windows `low_level_mouse_proc` / `low_level_keyboard_proc`: disarms `IS_TRAPPED`, schedules cursor restore, and passes subsequent events through.

---

## 4. Verification Plan

1. **Automated Unit Tests (macOS + Windows CI):**
   - `macos_cg_keycode_to_win_key`: round-trip compatibility against `SPEC-28`'s `win_key_to_macos_cg_keycode`, accurate Set 1 scancodes + `extended` bits for Enter vs NumpadEnter, Delete vs Backspace, Right modifiers, arrows, and F1..F20.
   - `MacTapModifierTracker`: left+right simultaneous Shift/Control/Option/Command transitions via `NX_DEVICE*` bits, generic-flag fallback, `CapsLock` toggle edge, and `Ctrl+Option+Esc` emergency-release detection.
   - `MacTapDeltaTracker`: pre-warp settle filtering near `leap_edge_pt`, fractional sub-point trackpad accumulation, warp threshold behavior, and clean reset.
   - `scale_macos_tap_scroll`: continuous point-delta accumulator (inverse of `SPEC-28`) + discrete wheel tick scaling.
   - Non-Windows Linux stub test updated: `#[cfg(not(any(windows, target_os = "macos")))]` keeps `start_hooks()` returning `false`.
2. **Quality & Build Gates:**
   - `npm run check:tauri-versions`, `npm test`, `npm run build`
   - `cargo fmt --check`, `cargo check --all-targets --locked`, `cargo test --locked`
   - `npx tauri build --bundles app --config '{"bundle":{"createUpdaterArtifacts":false}}'`
3. **Manual Two-PC Hardware Verification (Section G added to `docs/MANUAL-TEST-CHECKLIST.md`):**
   - macOS **Main PC** ↔ Windows **Linked PC** (or macOS ↔ macOS): Accessibility/Input Monitoring prompt, seam leap into Linked PC, hidden/pinned cursor without triggering local clicks/hot-corners, trackpad smoothness, continuous & mouse-wheel scrolling, `Cmd/Opt/Ctrl` typing (`Cmd+C/V` -> `Win`, etc.), `Ctrl+Option+Esc` emergency release, and `CursorReturn` back to Mac seam position.
