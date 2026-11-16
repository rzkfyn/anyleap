# Linux Linked PC Implementation Plan (`SPEC-30`)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans (inline execution selected) or superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Enable Linux workstations — prioritizing native Wayland on **CachyOS / Hyprland** (wlroots virtual input + `xdg-output`), with Wayland XDG Desktop Portal + EIS (`ashpd` + `reis`) for GNOME/KDE and `x11rb` XTest fallback — to act as a **Linked PC** in Shared Mouse & Keyboard.

**Architecture:** Isolate pure-Rust Linux display normalization, Win32→`evdev` translation, XKB modifier tracking, `union_rect` pointer normalization, inverted-sign Wayland scroll scaling, XKB keymap generation, and software cursor tracking inside `#[cfg(any(target_os = "linux", test))]` modules so macOS/Windows/Linux CI all test the core logic directly. Isolate Wayland/Portal/X11 I/O strictly inside `#[cfg(target_os = "linux")]` modules built from verified pure-Rust crates (`wayland-client`, `wayland-protocols*`, `ashpd`, `reis`, `x11rb`) with zero new C `-dev` headers.

**Tech Stack:** Rust, `wayland-client`, `wayland-protocols` (`xdg_output`), `wayland-protocols-wlr` (`virtual_pointer`), `wayland-protocols-misc` (`zwp_virtual_keyboard_v1`), `ashpd` + `reis`, `x11rb`, `zbus`, `rustix`.

**Spec:** [SPEC-30: Linux Linked PC Display & Input Adapters](../../specs/SPEC-30-linux-kvm-linked-pc.md)

## Global Constraints
- `AGENTS.md` §1.1: single atomic squashed commit on `feat/linux-kvm-linked-pc` before PR.
- `AGENTS.md` §1.3: zero competitor software names in docs, comments, or commits.
- Zero new system C `-dev` apt dependencies in `.github/workflows/ci.yml` (`linux-check` runs pure-Rust crates cleanly with `--locked`).
- Zero changes to `KvmPacket` / `UdpMouseDelta` wire format or `1:1` modifier semantics (`Win -> Super`, `Alt -> Alt`, `Ctrl -> Ctrl`).
- Verify every Linux crate API directly against the fetched crate source in Cargo's local registry cache after locking `Cargo.lock`, since this development machine runs macOS.

## Review Focus
1. **Scroll direction inversion:** Wayland `wl_pointer::axis` treats **negative** as up and **positive** as down (opposite of Win32 `WHEEL_DELTA`); must invert and preserve small precision ticks cleanly.
2. **Hyprland `zwp_virtual_keyboard_v1` FD & keymap safety:** Must upload keymap ONCE per session (zero `EMFILE` leaks) via `rustix::fs::memfd_create`, include trailing `\0`, set exact byte size, and rewind FD offset to `0` before sending `keymap`.
3. **Virtual keyboard XKB modifier state sync:** Pressing `Ctrl`, `Alt`, `Super`, `Shift`, or `CapsLock` must send both `key(evdev)` and explicit `modifiers(depressed, 0, locked, 0)` so Hyprland shortcuts (`Super+...`, `Ctrl+C/V`) trigger reliably.
4. **Negative-origin multi-monitor normalization:** When a secondary Linux display sits at negative coordinates (e.g. `(-1920, 0)`), `zwlr_virtual_pointer_v1::motion_absolute` must offset coordinates by `-union.x, -union.y` relative to total `union_rect` width/height.
5. **Wayland cursor position round-trip (`get_cursor_pos`):** Wayland denies reading global OS pointer coordinates; `SoftCursorTracker` must seed at `CursorEnter` (`set_cursor_position`) and maintain clamped global position so seam `CursorReturn` back to Main PC fires cleanly.

---

### Task 1: Lock Pure-Rust Linux Crates in `Cargo.toml` / `Cargo.lock` & Verify APIs

**Files:**
- Modify: `src-tauri/Cargo.toml`, `src-tauri/Cargo.lock`

- [x] **Step 1: Add `[target.'cfg(target_os = "linux")'.dependencies]` cleanly matching `Cargo.lock` (`zbus 5`, `tokio 1`, `rustix 1`)**
  - Add pure-Rust `wayland-client`, `wayland-protocols` (`client`, `unstable`), `wayland-protocols-wlr` (`client`), `wayland-protocols-misc` (`client`), `x11rb` (`randr`, `xtest`), `ashpd`, and `reis` (`tokio`).
- [x] **Step 2: Resolve `Cargo.lock` and fetch crate sources locally**
  - Run: `cargo metadata --manifest-path src-tauri/Cargo.toml --format-version 1 >/dev/null && cargo fetch --manifest-path src-tauri/Cargo.toml && cargo check --manifest-path src-tauri/Cargo.toml --all-targets --locked`
  - Expected: `Cargo.lock` locks cleanly with zero conflicts and macOS check still passes.
- [x] **Step 3: Inspect fetched crate sources in Cargo registry**
  - Verify exact module paths & method signatures for `xdg_output`, `wlr-virtual-pointer`, `virtual-keyboard`, `ashpd::desktop::remote_desktop`, `reis::ei`, `zbus`, `x11rb`, and `rustix::fs::memfd_create`.

---

### Task 2: Pure Linux Display Normalization & Input Translation (`monitors.rs`, `injector.rs`)

**Files:**
- Modify: `src-tauri/src/kvm/monitors.rs`, `src-tauri/src/kvm/injector.rs`

**Interfaces:**
- Produces (`monitors.rs` under `#[cfg(any(target_os = "linux", test))]`):
  - `pub(crate) struct LinuxRawMonitor { pub name: String, pub x: i32, pub y: i32, pub width: i32, pub height: i32, pub is_primary: bool }`
  - `pub(crate) fn normalize_linux_monitors(raw: Vec<LinuxRawMonitor>) -> Vec<MonitorDisplay>`
- Produces (`injector.rs` under `#[cfg(any(target_os = "linux", test))] mod linux_logic`):
  - `pub fn win_key_to_linux_evdev(key_code: u16, scan_code: u16, extended: bool) -> Option<u16>`
  - `pub fn build_us_evdev_xkb_keymap() -> &'static [u8]`
  - `pub fn normalize_linux_abs_pointer(x: i32, y: i32, union: &ScreenRect) -> Option<(u32, u32, u32, u32)>`
  - `pub struct LinuxModifierTracker` (`update(evdev_code: u16, is_down: bool) -> (u32, u32)`, `clear() -> (u32, u32)`)
  - `pub struct LinuxScrollAccumulator` (`push_wayland(delta: i32) -> (f64, i32)`, `push_x11_clicks(delta: i32) -> i32`)
  - `pub struct SoftCursorTracker` (`set(&mut self, x: i32, y: i32)`, `get(&self) -> Option<(i32, i32)>`, `apply_delta(&mut self, dx: i32, dy: i32, union: Option<&ScreenRect>) -> Option<(i32, i32)>`)

- [x] **Step 1: Write failing unit tests (`RED`)**
  - `monitors.rs`: `linux_monitors_normalize_coordinates_names_and_primary` (filters <=0 dims, assigns `wl-1` fallback when connector name empty, selects `(0,0)` monitor primary when unset).
  - `injector.rs`:
    - `linux_evdev_keymap_covers_keys_modifiers_extended_and_scancodes`
    - `linux_modifier_tracker_tracks_sides_xkb_masks_and_capslock_lock`
    - `linux_pointer_normalization_scroll_inversion_xkb_keymap_and_soft_cursor`
- [x] **Step 2: Run targeted tests (`RED`)**
  - Run: `cargo test --manifest-path src-tauri/Cargo.toml --locked linux_`
  - Expected: FAIL (`module linux_logic` / `normalize_linux_monitors` missing).
- [x] **Step 3: Implement pure logic in `monitors.rs` and `injector.rs` (`GREEN`)**
  - Implement `normalize_linux_monitors` and `mod linux_logic`.
- [x] **Step 4: Verify unit tests pass cleanly (`GREEN`)**
  - Run: `cargo test --manifest-path src-tauri/Cargo.toml --locked monitors::tests && cargo test --manifest-path src-tauri/Cargo.toml --locked injector::tests`
  - Expected: PASS with 0 warnings in `monitors.rs` and `injector.rs`.

---

### Task 3: Linux Display Enumeration Backend (`monitors.rs` `#[cfg(target_os = "linux")]`)

**Files:**
- Modify: `src-tauri/src/kvm/monitors.rs`

- [x] **Step 1: Implement `#[cfg(target_os = "linux")] mod linux_display`**
  - Wayland `wl_output` + `zxdg_output_manager_v1` round-trip enumeration (`LogicalPosition`, `LogicalSize`, `Name`, `Done`).
  - GNOME `org.gnome.Mutter.DisplayConfig.GetCurrentState` session D-Bus fallback via `zbus::blocking`.
  - X11 `x11rb` RandR CRTC enumeration + primary output query fallback.
  - Normalize results via `normalize_linux_monitors()` inside `get_local_monitors()`.

---

### Task 4: Linux Input Injection Backends & Server Prep Hook (`injector.rs`, `kvm/commands.rs`)

**Files:**
- Modify: `src-tauri/src/kvm/injector.rs`, `src-tauri/src/kvm/commands.rs`

**Interfaces:**
- Produces (`injector.rs`): `pub fn prepare_linux_remote_control(app_data_dir: &std::path::Path) -> Result<(), String>`
- Consumes (`kvm/commands.rs`): calls `injector::prepare_linux_remote_control(&app_data_dir)` inside `start_server_inner` under `#[cfg(target_os = "linux")]`, emitting `kvm-continuity-notice` on failure.

- [x] **Step 1: Implement `#[cfg(target_os = "linux")] mod linux_input`**
  - Thread-backed Wayland wlroots virtual input worker (`zwlr_virtual_pointer_manager_v1` + `zwp_virtual_keyboard_manager_v1` + single-upload `rustix::fs::memfd_create` XKB keymap + `SoftCursorTracker`).
  - Portal `RemoteDesktop` + `reis` EIS fallback (`ashpd`, `PersistMode::ExplicitlyRevoked`, saved restore token file + D-Bus `notify_*` fallback).
  - X11 `x11rb` `xtest` + `query_pointer`/`warp_pointer` fallback.
  - Wire `get_cursor_pos`, `set_cursor_position`, `inject_mouse_delta`, `inject_mouse_button`, `inject_mouse_wheel`, `inject_key`, and `prepare_linux_remote_control`.
- [x] **Step 2: Wire `prepare_linux_remote_control` notice in `kvm/commands.rs`**
  - Add pure helper `linux_remote_control_error_notice(err: &str) -> String` (unit-tested!) and call under `#[cfg(target_os = "linux")]` inside `start_server_inner`.

---

### Task 5: Documentation Sync, Verification Gates & Single Atomic Commit

**Files:**
- Modify: `docs/specs/SPEC-30-linux-kvm-linked-pc.md`, `docs/QUEUE.md`, `CONTEXT.md`, `CONTRIBUTING.md`, `README.md`, `docs/MANUAL-TEST-CHECKLIST.md`

- [x] **Step 1: Update project documentation per `AGENTS.md` §1.4**
  - Mark `SPEC-30-linux-kvm-linked-pc.md` status as `Implemented (pending CachyOS/Hyprland hardware & Linux CI verification)`.
  - Update `docs/QUEUE.md`, `CONTEXT.md`, `CONTRIBUTING.md`, `README.md`, and add Section H (**Linux / CachyOS Hyprland Linked PC (`SPEC-30`)**) to `docs/MANUAL-TEST-CHECKLIST.md`.
- [x] **Step 2: Run all local verification gates**
  - Run: `npm run check:tauri-versions && npm test && npm run build && cargo fmt --manifest-path src-tauri/Cargo.toml --all -- --check && cargo check --manifest-path src-tauri/Cargo.toml --all-targets --locked && cargo test --manifest-path src-tauri/Cargo.toml --locked`
  - Expected: All frontend tests/builds + Rust format/check/tests pass cleanly with zero new warnings.
- [x] **Step 3: Create single atomic commit on `feat/linux-kvm-linked-pc`**
  - Commit: `feat(kvm): add Linux Linked PC Wayland and X11 adapters (SPEC-30)`.
