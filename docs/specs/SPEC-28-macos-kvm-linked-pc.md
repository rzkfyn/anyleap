# SPEC-28: macOS Input & Display Adapters for Linked PC (Virtual KVM Milestone 1)

> **Status:** Implemented / Pending Two-PC Hardware Verification
> **Scope:** `(kvm)` — Addresses Milestone 1 of cross-platform Virtual KVM support so macOS can act as a **Linked PC** (Receiver) controlled by a Windows **Main PC**.

## 1. Goal

Enable a macOS machine to participate in **Shared Mouse & Keyboard** as a **Linked PC** (`allow_control` / QUIC receiver server), enumerating its real display layout and injecting mouse movement, clicks, drags, scroll wheel ticks, and keyboard input received over the existing authenticated QUIC link (`anyleap-kvm/1`).

Out of scope for Milestone 1:
- **Main PC (Controller)** global input capture/swallowing on macOS (`CGEventTap` in `hooks.rs`).
- **Continuity** macOS adapters (`clipboard.rs`, `dragdrop.rs`, `audio.rs`, `media_keys.rs`, `session_sync.rs`, `open_on_linked.rs`, `cursor_scheme.rs`).
- Linux input adapters (X11/Wayland).
- Wire protocol breaking changes to `KvmPacket` or `UdpMouseDelta`.

---

## 2. Design Decisions

### 2.1 Display Enumeration & Coordinate Space (`monitors.rs`)
- macOS implements `get_local_monitors()` via CoreGraphics (`CGGetActiveDisplayList`, `CGDisplayBounds`, `CGDisplayIsMain`).
- All monitor bounds (`x`, `y`, `width`, `height`) and cursor coordinates use **CoreGraphics Global Display Coordinates (logical points)** — top-left primary origin `(0, 0)`, Y increasing downward — matching `CGEventGetLocation`, `CGWarpMouseCursorPosition`, and `CGEventCreateMouseEvent`.
- Using logical points keeps `monitors.rs`, `input_logic.rs` seam/edge geometry, and `injector.rs` unit-consistent across mixed Retina and non-Retina external displays without changing `MonitorDisplay` or `UdpMouseDelta` wire formats (`1 px` Main PC delta = `1 pt` Linked PC delta).
- Each active display receives a deterministic identifier `cg-<CGDirectDisplayID>` in both `id` and `device_name`. If `CGGetActiveDisplayList` returns an empty list (e.g., headless CI), `get_local_monitors()` preserves the existing fallback display; if active displays exist without a main flag, the first display is marked `is_primary = true`.

### 2.2 Input Injection & Cursor Positioning (`injector.rs`)
All macOS event injection uses a shared private Quartz event source (`CGEventSourceCreate(kCGEventSourceStatePrivate)`) with local-event suppression interval set to `0.0` so remote injection never locks out local trackpad/keyboard responsiveness.

- **Cursor Read & Move (`get_cursor_pos`, `set_cursor_position`, `inject_mouse_delta`):**
  - `get_cursor_pos()` queries `CGEventGetLocation` on a temporary `CGEventCreate(NULL)` and rounds to `(i32, i32)` points.
  - `set_cursor_position(x, y)` calls `CGAssociateMouseAndMouseCursorPosition(1)` + `CGWarpMouseCursorPosition` (for immediate visual placement) and posts a `CGEventCreateMouseEvent` with `kCGEventMouseMoved` (or `kCGEventLeftMouseDragged` / `kCGEventRightMouseDragged` / `kCGEventOtherMouseDragged` when the corresponding mouse button is held), carrying active modifier flags and relative `kCGMouseEventDeltaX` / `kCGMouseEventDeltaY` fields so hover menus and window/text dragging behave natively.
  - `inject_mouse_delta(dx, dy)` moves relative to `get_cursor_pos()`, preserving `ensure_cursor_visible_with_nudge`'s net-zero 1-px refresh on `CursorEnter`.
- **Mouse Buttons & Click Streaks (`inject_mouse_button`):**
  - Maps buttons `1` (Left), `2` (Right), `3` (Middle) to `kCGEventLeftMouseDown/Up`, `kCGEventRightMouseDown/Up`, and `kCGEventOtherMouseDown/Up`.
  - Maintains `ClickStateTracker` (`500 ms` window, `4 pt` Chebyshev distance threshold) to set `kCGMouseEventClickState` (`1`, `2`, `3`, ...) on matching button-down and button-up events so double-click and triple-click interactions work natively.
- **Scroll Wheel (`inject_mouse_wheel`):**
  - Scales Win32 `WHEEL_DELTA` (`120` per detent) to continuous pixel units (`scale_macos_wheel_delta_px`, `40 px` per `120`, preserving non-zero sign for small precision ticks) via non-variadic `CGEventCreateScrollWheelEvent2(..., kCGScrollEventUnitPixel, 1, wheel1, 0, 0)`.
- **Keyboard Translation & Modifier Flags (`inject_key`):**
  - Translates Win32 `VK_*` (`key_code`), `extended`, and fallback PS/2 Set 1 `scan_code` to macOS virtual keycodes (`kVK_*`) using **1:1 modifier mapping**: `Win` (`VK_LWIN`/`VK_RWIN`) → `Command`, `Alt` (`VK_LMENU`/`VK_RMENU`) → `Option`, `Ctrl` (`VK_LCONTROL`/`VK_RCONTROL`) → `Control`, and `Shift` → `Shift`.
  - Tracks held left/right modifier keys and `CapsLock` (`VK_CAPITAL`) toggle in `MacModifierState` and applies the resulting `CGEventFlags` mask (plus `kCGEventFlagMaskNumericPad` for arrow/keypad keys) to posted keyboard and mouse events. Calling `inject_key(..., false, ...)` during `ReleaseAll` / `CursorReturn` cleanly clears held modifier flags.

### 2.3 Accessibility Permission Handling (`TCC`)
- `CGEventPost(kCGHIDEventTap, ...)` requires macOS Accessibility trust (`Privacy & Security → Accessibility`).
- `injector::check_accessibility_permission(prompt)` wraps `AXIsProcessTrusted` / `AXIsProcessTrustedWithOptions`.
- When `start_server_inner` starts the receiver server on macOS (`allow_control` enabled), AnyLeap checks trust with `prompt = true` (prompting once if untrusted) and emits a non-blocking `kvm-continuity-notice` guiding the user to System Settings if Accessibility is not yet granted. The server still starts so pairing and discovery are not blocked.

### 2.4 Non-Windows Controller Guard & Runtime Hygiene (`hooks.rs`, `kvm/commands.rs`)
- `HookController::start_hooks()` returns `bool`: on `#[cfg(not(windows))]`, it clears `HAS_ACTIVE_LINK`, leaves `HOOK_ACTIVE` as `false` (eliminating false-active status reporting), and returns `false`.
- `apply_controller_topology()` emits a one-shot `kvm-continuity-notice` when `start_hooks()` returns `false` on non-Windows, reset on client disconnect.
- `get_download_dir()` resolves `USERPROFILE`, then `HOME`, before falling back to `"."`, so received transfers on macOS land in `~/Downloads/AnyLeap_Transfers`.
- `get_local_machine_info()` resolves the macOS computer name (`scutil --get ComputerName` / `hostname -s`, cached once per process) instead of always falling back to `"AnyLeap-PC"` in GUI sessions lacking `COMPUTERNAME` / `HOSTNAME`.

---

## 3. Verification Strategy

- **Unit tests (cross-platform, run on both macOS and Windows CI):**
  - `monitors.rs`: `CGRect` conversion, negative secondary monitor coordinates, primary flag normalization.
  - `injector.rs`: `VK_*` & scan-code → `CGKeyCode` mappings (including `1:1` modifiers and `extended` disambiguation), `MacModifierState` left/right + CapsLock tracking, `ClickStateTracker` multi-click window/distance thresholds, wheel scaling, and move-vs-drag event type selection.
  - `hooks.rs` & `kvm/commands.rs`: non-Windows `start_hooks()` returning `false` without activating hooks, `resolve_download_dir` fallbacks, `resolve_local_computer_name` fallback chain.
- **Manual two-PC hardware verification:**
  - Windows **Main PC** ↔ macOS **Linked PC** pairing, discovery, monitor canvas alignment, leap/return, drag/double-click, scroll, modifier combos (`Cmd+C/V`, `Shift+Arrow`, `Option+Left`), and Accessibility prompt flow (tracked in `docs/MANUAL-TEST-CHECKLIST.md`).
