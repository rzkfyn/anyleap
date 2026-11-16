# macOS Linked PC Input & Display Adapters Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Enable macOS to run as a Shared Mouse & Keyboard **Linked PC** (Receiver) per Milestone 1 of Issue #59, with CoreGraphics display enumeration, Quartz/ApplicationServices input injection, Accessibility trust guidance, honest non-Windows controller hook guards, and macOS download/hostname hygiene.

**Architecture:**
- `src-tauri/src/kvm/monitors.rs`: `#[cfg(target_os = "macos")]` display enumeration via CoreGraphics (`CGGetActiveDisplayList`, `CGDisplayBounds`, `CGDisplayIsMain`) in logical points.
- `src-tauri/src/kvm/injector.rs`: `#[cfg(target_os = "macos")]` FFI to CoreGraphics, ApplicationServices (`AXIsProcessTrustedWithOptions`), and CoreFoundation with pure testable helpers (`win_key_to_macos_cg_keycode`, `MacModifierState`, `ClickStateTracker`, `scale_macos_wheel_delta_px`, `macos_mouse_move_event_type`).
- `src-tauri/src/kvm/hooks.rs`: return `bool` from `start_hooks()` and prevent `HOOK_ACTIVE = true` on `#[cfg(not(windows))]`.
- `src-tauri/src/kvm/commands.rs`: Accessibility prompt/notice on macOS receiver start, one-shot unsupported-controller notice, `HOME` fallback in `get_download_dir()`, and macOS computer name resolution.

**Tech Stack:** Rust (`CoreGraphics`, `ApplicationServices`, `CoreFoundation` framework FFI, zero new crates), Tauri 2.

**Spec:** `docs/specs/SPEC-28-macos-kvm-linked-pc.md`

## Global Constraints
- Follow `AGENTS.md` (zero third-party competitor names, atomic squashed commit, documentation sync, all quality gates passing).
- All pure macOS mapping/tracking logic must compile under `#[cfg(any(target_os = "macos", test))]` so unit tests run on both macOS and Windows CI.

---

### Task 1: macOS Display Enumeration (`monitors.rs`)
**Files:**
- Modify: `src-tauri/src/kvm/monitors.rs`

- [x] Add `monitor_from_cg_rect` and `normalize_macos_monitors` pure helpers under `#[cfg(any(target_os = "macos", test))]`.
- [x] Implement `#[cfg(target_os = "macos")]` enumeration using `CGGetActiveDisplayList`, `CGDisplayBounds`, and `CGDisplayIsMain`.
- [x] Add unit tests for `CGRect` conversion, negative secondary origins, and primary monitor fallback.

---

### Task 2: macOS Input Injection & Accessibility Check (`injector.rs`)
**Files:**
- Modify: `src-tauri/src/kvm/injector.rs`

- [x] Add pure logic helpers under `#[cfg(any(target_os = "macos", test))]`:
  - `win_key_to_macos_cg_keycode(key_code, scan_code, extended)` with `1:1` modifiers (`Win -> Command`, `Alt -> Option`, `Ctrl -> Control`).
  - `MacModifierState` (left/right modifier tracking, CapsLock toggle, `NumericPad` mask helper, reset).
  - `ClickStateTracker` (`500 ms` / `4 pt` double/triple-click tracking for `kCGMouseEventClickState`).
  - `scale_macos_wheel_delta_px(delta)` and `macos_mouse_move_event_type(buttons_mask)`.
- [x] Implement `#[cfg(target_os = "macos")]` FFI for `check_accessibility_permission`, `get_cursor_pos`, `set_cursor_position`, `inject_mouse_delta`, `inject_mouse_button`, `inject_mouse_wheel`, and `inject_key`, updating `#[cfg(not(windows))]` fallbacks to `#[cfg(not(any(windows, target_os = "macos")))]`.
- [x] Add comprehensive unit tests for all pure helpers.

---

### Task 3: Non-Windows Hook Guard & Runtime Hygiene (`hooks.rs`, `kvm/commands.rs`)
**Files:**
- Modify: `src-tauri/src/kvm/hooks.rs`
- Modify: `src-tauri/src/kvm/commands.rs`

- [x] Update `HookController::start_hooks()` to return `bool` and avoid setting `HOOK_ACTIVE = true` on `#[cfg(not(windows))]`.
- [x] Emit one-shot `kvm-continuity-notice` when `start_hooks()` returns `false`, and reset the notice guard on disconnect.
- [x] Prompt/check Accessibility trust once in `start_server_inner` on macOS and emit `kvm-continuity-notice` if untrusted.
- [x] Add `HOME` fallback in `get_download_dir()` (`resolve_download_dir`) and macOS computer name resolution (`resolve_local_computer_name`).
- [x] Add unit tests in `hooks.rs` and `kvm/commands.rs`.

---

### Task 4: Documentation Sync & Verification
**Files:**
- Modify: `docs/QUEUE.md`, `CONTEXT.md`, `CONTRIBUTING.md`, `README.md`, `docs/MANUAL-TEST-CHECKLIST.md`, `docs/specs/SPEC-28-macos-kvm-linked-pc.md`

- [x] Update docs per `AGENTS.md` §1.4.
- [x] Run all verification gates (`npm run check:tauri-versions`, `npm test`, `npm run build`, `cargo fmt --check`, `cargo check --all-targets --locked`, `cargo test --locked`, `npx tauri build --bundles app`).
