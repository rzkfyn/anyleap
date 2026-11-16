# macOS Main PC Input Capture & Trap Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans (inline execution selected) or superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement macOS as a **Main PC** (Controller) in Shared Mouse & Keyboard using Quartz `CGEventTap` (`hooks.rs`), reverse `CGKeyCode -> Windows VK/scancode` mapping, TCC Input Monitoring / Accessibility helpers (`injector.rs`), and user-facing permission notices (`kvm/commands.rs`).

**Architecture:** Replace the non-Windows `start_hooks()` no-op stub for `target_os = "macos"` with a dedicated `CFRunLoop` thread hosting an active `CGEventTapCreate(kCGSessionEventTap, ...)` plus two wakeup `CFRunLoopSource` handles (`engage` and `restore`). Pure state machines (`MacTapModifierTracker`, `MacTapDeltaTracker`, `MacTapScrollAccumulator`, `macos_cg_keycode_to_win_key`) under `#[cfg(any(target_os = "macos", test))]` isolate all key/flag/delta/scroll math for cross-platform unit testing while FFI lives solely inside `#[cfg(target_os = "macos")]`.

**Tech Stack:** Rust, macOS CoreGraphics / CoreFoundation (`CGEventTap`, `CFRunLoop`, `CFMachPort`), existing `tokio::sync::mpsc` & `anyleap` KVM protocol (`KvmPacket`, `input_logic`, `monitors`, `injector`).

**Spec:** [SPEC-29: macOS Main PC Input Capture & Trap](../../specs/SPEC-29-macos-kvm-main-pc.md)

## Global Constraints
- Follow `AGENTS.md` §1.1: squash into ONE clean atomic commit on `feat/macos-kvm-main-pc` before PR (intermediate task steps stay uncommitted locally).
- Follow `AGENTS.md` §1.3: zero third-party competitor names in specs, comments, or commit messages.
- Follow `SPEC-28` / `SPEC-29`: keep 1:1 modifier mapping (`Command ↔ Win`, `Option ↔ Alt`, `Control ↔ Ctrl`), CoreGraphics logical points across `monitors.rs`/`hooks.rs`/`injector.rs`, and zero wire protocol (`KvmPacket` / `UdpMouseDelta`) breaking changes.
- Zero new Cargo crate dependencies (direct CoreGraphics / CoreFoundation / ApplicationServices framework FFI only).
- Zero new compiler warnings in edited modules (`monitors.rs`, `injector.rs`, `hooks.rs`, `kvm/commands.rs`) on macOS or Windows CI (`clippy -D warnings`).

## Review Focus
1. **Windows `KEYEVENTF_SCANCODE` interaction:** Windows receiver (`injector.rs`) sets `KEYEVENTF_SCANCODE` whenever `scan_code > 0` and ignores `wVk`; every entry in `macos_cg_keycode_to_win_key` must emit an accurate PS/2 Set 1 `scan_code` + `extended` flag alongside `vk_code`.
2. **Simultaneous left+right modifiers & generic flag fallback:** Releasing `Right Shift` while `Left Shift` remains held on macOS must emit `VK_RSHIFT up` without clearing `Shift`; virtual keyboards omitting `NX_DEVICE*` low-16-bit flags must still emit clean press/release transitions.
3. **Self-injection recursion guard:** `perform_macos_engage_leap` calls `injector::inject_key(..., false, ...)` while `HOOK_ACTIVE == true`; events tagged with `ANYLEAP_CG_USER_DATA` must be skipped by the `CGEventTap` callback identically to Win32 `LLKHF_INJECTED`.
4. **Pre-warp queued HID jump & fractional trackpad drift:** Events already in WindowServer's HID queue immediately before `CGWarpMouseCursorPosition(center)` on engage must not produce a cross-screen delta spike; tiny fractional trackpad deltas must accumulate smoothly across ticks.
5. **macOS `CGDisplayHideCursor` counter balance & tap timeout recovery:** `CGDisplayHideCursor` must balance 1:1 with `CGDisplayShowCursor` via an atomic flag on cursor return / emergency release / stop / panic; `kCGEventTapDisabledByTimeout` must re-enable the tap immediately.

---

### Task 1: Self-Injection Event Tagging & Input Monitoring Permission (`injector.rs`)

**Files:**
- Modify: `src-tauri/src/kvm/injector.rs`

**Interfaces:**
- Produces:
  - `pub const ANYLEAP_CG_USER_DATA: i64 = 0x414E_594C_4B56_4D31;`
  - `pub fn is_self_injected_cg_user_data(user_data: i64) -> bool`
  - `pub fn check_input_monitoring_permission(prompt: bool) -> bool`

- [x] **Step 1: Write failing unit test in `src-tauri/src/kvm/injector.rs` (`mod tests`)**
  - Test `self_injected_cg_user_data_tag_matches_exact_magic_only`: asserts `is_self_injected_cg_user_data(ANYLEAP_CG_USER_DATA)` is `true`, and `0`, `-1`, `ANYLEAP_CG_USER_DATA ^ 1` are `false`.
- [x] **Step 2: Run targeted test to verify failure (`RED`)**
  - Run: `cargo test --manifest-path src-tauri/Cargo.toml --locked self_injected_cg_user_data`
  - Expected: FAIL (`cannot find value ANYLEAP_CG_USER_DATA / function is_self_injected_cg_user_data`).
- [x] **Step 3: Implement tagging & Input Monitoring helper in `src-tauri/src/kvm/injector.rs`**
  - Export `ANYLEAP_CG_USER_DATA` and `is_self_injected_cg_user_data(user_data)`.
  - Export `check_input_monitoring_permission(prompt: bool) -> bool` (non-macOS returns `true`; macOS calls `CGPreflightListenEventAccess` / `CGRequestListenEventAccess` from `CoreGraphics`).
  - In `macos_input::event_source()`, call `CGEventSourceSetUserData(src, ANYLEAP_CG_USER_DATA)` so every event created by AnyLeap carries `kCGEventSourceUserData = ANYLEAP_CG_USER_DATA`.
- [x] **Step 4: Run targeted tests to verify pass (`GREEN`)**
  - Run: `cargo test --manifest-path src-tauri/Cargo.toml --locked injector::tests`
  - Expected: PASS (all `injector::tests`, including `self_injected_cg_user_data_tag_matches_exact_magic_only`).

---

### Task 2: Pure macOS Reverse Keymap, Modifier Tracker, Delta & Scroll Engine (`hooks.rs` `mod macos_logic`)

**Files:**
- Modify: `src-tauri/src/kvm/hooks.rs`

**Interfaces:**
- Consumes: `MonitorDisplay` (`monitors.rs`), `monitor_at_point` (`input_logic.rs`)
- Produces (`#[cfg(any(target_os = "macos", test))] mod macos_logic`):
  - `pub fn macos_cg_keycode_to_win_key(cg_keycode: u16) -> Option<(u16, u16, bool)>`
  - `pub struct MacTapModifierTracker` with `on_flags_changed(cg_keycode: u16, raw_flags: u64) -> ModifierFlagTransition`, `clear()`, `ctrl_and_option_down() -> bool`
  - `pub struct MacTapDeltaTracker` with `engage(center: (f64, f64), leap_edge_pt: (f64, f64))`, `reset()`, `on_moved(pt: (f64, f64), elapsed_since_engage_ms: u64) -> Option<(i32, i32, bool)>` (returns `Some((dx, dy, should_warp_to_center))` or `None` if dropped during pre-warp settle / zero integer change)
  - `pub struct MacTapScrollAccumulator` with `push(is_continuous: bool, point_delta_axis1: i64, discrete_delta_axis1: i64) -> i32`, `reset()`
  - `pub fn is_emergency_release_combo(vk_code: u16, ctrl_down: bool, alt_or_opt_down: bool) -> bool`
  - `pub fn cg_button_to_kvm(event_type: u32, button_number: i64) -> Option<(u8, bool)>`
  - `pub fn is_point_on_local_display(pt: (i32, i32), monitors: &[MonitorDisplay]) -> bool`

- [x] **Step 1: Write failing unit tests in `src-tauri/src/kvm/hooks.rs` (`mod tests`)**
  - `macos_reverse_keymap_round_trips_spec28_with_set1_scancodes`: verifies letters (`A..Z`), digits (`0..9`), Return (`0x24 -> (0x0D, 0x1C, false)`), KeypadEnter (`0x4C -> (0x0D, 0x1C, true)`), Backspace (`0x33 -> (0x08, 0x0E, false)`), ForwardDelete (`0x75 -> (0x2E, 0x53, true)`), Left/Right `Command/Option/Control/Shift` (`1:1` VK + Set 1 scan codes + `extended`), F1..F20, Numpad, and arrows (`extended = true`), plus cross-checks every mapped key against `injector::macos_logic::win_key_to_macos_cg_keycode`.
  - `macos_modifier_tracker_handles_sides_fallback_capslock_and_emergency`: verifies simultaneous LShift + RShift release keeps Shift semantics clean per side (`VK_LSHIFT` vs `VK_RSHIFT`), generic mask fallback when low 16 bits are zero, `CapsLock` `AlphaShift` toggle emitting `KeyPair` (`is_down = true` + `false`), and `Ctrl + Option + Esc` (plus `VK_SCROLL` / `VK_PAUSE`) without triggering on `Cmd + Option + Esc`.
  - `macos_delta_tracker_drops_prewarp_edge_event_and_accumulates_fractions`: verifies pre-warp move near `leap_edge_pt` within `25ms` is dropped (`None`), micro-moves `(0.4, 0.4)` + `(0.4, 0.4)` + `(0.4, 0.4)` accumulate to `(1, 1)` cleanly, normal move emits exact delta and `should_warp = true`.
  - `macos_scroll_and_button_mapping`: verifies continuous `point_delta_axis1 = 10 -> +30` and sub-tick accumulation (`40 pt -> +120`), discrete `delta = 1 -> +120`, and Left/Right/Middle button number decoding (`otherMouseDown` button `2 -> 3`, ignoring `>2` cleanly).
- [x] **Step 2: Run targeted tests to verify failure (`RED`)**
  - Run: `cargo test --manifest-path src-tauri/Cargo.toml --locked hooks::tests::macos_`
  - Expected: FAIL (`module macos_logic not found`).
- [x] **Step 3: Implement `mod macos_logic` in `src-tauri/src/kvm/hooks.rs`**
  - Implement all pure functions & trackers concisely, sharing PS/2 Set 1 constants cleanly.
- [x] **Step 4: Run targeted tests to verify pass (`GREEN`)**
  - Run: `cargo test --manifest-path src-tauri/Cargo.toml --locked hooks::tests::macos_`
  - Expected: PASS.

---

### Task 3: CoreGraphics `CGEventTap` Capture Engine & Cursor Lifecycle (`hooks.rs` `mod macos_tap`)

**Files:**
- Modify: `src-tauri/src/kvm/hooks.rs`

**Interfaces:**
- Consumes:
  - `macos_logic::*` (`hooks.rs`)
  - `injector::{ANYLEAP_CG_USER_DATA, is_self_injected_cg_user_data, check_accessibility_permission, check_input_monitoring_permission, inject_key}`
- Produces:
  - Functional `#[cfg(target_os = "macos")] HookController::{start_hooks, stop_hooks, handle_cursor_return}` & `restore_cursor_visibility()`
  - Updated `#[cfg(not(any(windows, target_os = "macos")))]` test `start_hooks_on_unsupported_os_leaves_hooks_inactive`
  - Unit check `macos_tap_permission_preflight_never_panics_and_stops_cleanly` (verifying `stop_hooks()` and `restore_cursor_visibility()` without requiring TCC grant on CI runners).

- [x] **Step 1: Write test for updated stub guard & macOS tap clean stop**
  - Narrow existing non-Windows test to `#[cfg(not(any(windows, target_os = "macos")))]` and add `macos_restore_cursor_and_stop_without_active_tap_are_safe` (`#[cfg(target_os = "macos")]`).
- [x] **Step 2: Implement `#[cfg(target_os = "macos")] mod macos_tap` and wire `HookController`**
  - CoreFoundation + CoreGraphics `extern "C"` bindings (`CFMachPortCreateRunLoopSource`, `CFMachPortInvalidate`, `CFRunLoopGetCurrent`, `CFRunLoopAddSource`, `CFRunLoopSourceCreate`, `CFRunLoopSourceSignal`, `CFRunLoopSourceInvalidate`, `CFRunLoopWakeUp`, `CFRunLoopRun`, `CFRunLoopStop`, `CFRelease`, `kCFRunLoopCommonModes`, `CGEventTapCreate`, `CGEventTapEnable`, `CGEventGetLocation`, `CGEventGetFlags`, `CGEventGetIntegerValueField`, `CGDisplayHideCursor`, `CGDisplayShowCursor`, `CGMainDisplayID`, `CGWarpMouseCursorPosition`).
  - Tap thread spawns with `mpsc::sync_channel::<bool>(1)` readiness signal; creates `kCGSessionEventTap` active filter + `engage` & `restore` `CFRunLoopSource`s, enters `CFRunLoopRun()`.
  - `perform_macos_engage_leap` hides cursor once (`MACOS_CURSOR_HIDDEN` guard), computes leap edge point, arms `MacTapDeltaTracker`, warps cursor to trap center, releases held non-mouse keys tracked in `CONTROLLER_TRACKER`, and sends `KvmPacket::CursorEnter`.
  - `restore_macos_cursor(warp_to: Option<(i32, i32)>)` balances `CGDisplayShowCursor` and warps cursor to `(rx, ry)` on `CursorReturn`.
  - Panic guard (`catch_unwind`) inside tap callback disarms `IS_TRAPPED`, schedules cursor restore, and passes event through.
- [x] **Step 3: Compile & run tests on macOS**
  - Run: `cargo check --manifest-path src-tauri/Cargo.toml --all-targets --locked && cargo test --manifest-path src-tauri/Cargo.toml --locked hooks::tests`
  - Expected: PASS with 0 warnings in `hooks.rs`, `injector.rs`, `monitors.rs`.

---

### Task 4: Controller Permission Notice, Doc Sync, Full Gates & Single Atomic Commit

**Files:**
- Modify: `src-tauri/src/kvm/commands.rs`
- Modify: `docs/specs/SPEC-29-macos-kvm-main-pc.md`, `docs/QUEUE.md`, `CONTEXT.md`, `CONTRIBUTING.md`, `README.md`, `docs/MANUAL-TEST-CHECKLIST.md`

- [x] **Step 1: Refine `apply_controller_topology` failure notice in `src-tauri/src/kvm/commands.rs`**
  - Split helper `controller_start_failure_notice(is_macos: bool) -> &'static str` (unit-tested!) so `#[cfg(target_os = "macos")]` reports Accessibility / Input Monitoring instructions while other non-Windows keep `"Main PC input capture is not supported on this OS yet. This machine can still act as a Linked PC."`.
  - Update existing `WARNED_UNSUPPORTED_CONTROLLER` emission site cleanly.
- [x] **Step 2: Synchronize project documentation per `AGENTS.md` §1.4**
  - Mark `SPEC-29-macos-kvm-main-pc.md` status as `Implemented / Pending Two-PC Hardware Verification`.
  - Update `docs/QUEUE.md` row #13 (`SPEC-28, SPEC-29`), `CONTEXT.md` (`CGEventTap` adapter entry), `CONTRIBUTING.md` Cross-Platform table (`hooks.rs` macOS entry), `README.md` (macOS Main PC + Linked PC note & Accessibility/Input Monitoring requirement), and `docs/MANUAL-TEST-CHECKLIST.md` Section G (macOS Main PC manual test steps).
- [x] **Step 3: Run all quality gates + `.app` bundle build**
  - Run:
    - `npm run check:tauri-versions`
    - `npm test`
    - `npm run build`
    - `cargo fmt --manifest-path src-tauri/Cargo.toml --all -- --check`
    - `cargo check --manifest-path src-tauri/Cargo.toml --all-targets --locked`
    - `cargo test --manifest-path src-tauri/Cargo.toml --locked`
    - `npx tauri build --bundles app --config '{"bundle":{"createUpdaterArtifacts":false}}'`
  - Expected: 100% pass, zero new compiler warnings, clean `AnyLeap.app` bundle.
- [x] **Step 4: Mark plan checkboxes complete & create atomic commit**
  - Commit on `feat/macos-kvm-main-pc`: `feat(kvm): add macOS Main PC CGEventTap capture (SPEC-29)` (user reviews before any `git push` or PR).
