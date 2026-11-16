# SPEC-30: Linux Linked PC Display & Input Adapters (Wayland Hyprland/wlroots + Portal/EIS + X11)

> **Status:** Implemented (pending CachyOS/Hyprland hardware & Linux CI verification)
> **Scope:** `(kvm)` — Implements **Linked PC** (Receiver) display enumeration and input injection on Linux, with first-class support for Wayland (CachyOS / **Hyprland** & wlroots compositors), XDG Desktop Portal (`RemoteDesktop` + `libei` for GNOME/KDE), and X11 fallback.
> **Depends on:** Branch 1 (`fix/macos-linux-scrcpy-build` #58 build fix) and `SPEC-28` (`resolve_download_dir` `HOME` fallback & `local_computer_name` `hostname -s`).

---

## 1. Goal & Scope

Enable a Linux workstation (specifically targeting **Wayland on CachyOS/Hyprland** out of the box, plus GNOME/KDE Wayland and X11 desktops) to act as a **Linked PC** (`allow_control` / QUIC receiver server) controlled by a Windows or macOS Main PC over `anyleap-kvm/1`.

### In Scope
- Linux display enumeration (`monitors.rs`) in logical compositor coordinates across:
  1. Wayland `zxdg_output_manager_v1` + `wl_output` (Hyprland, Sway, KDE, wlroots).
  2. GNOME Mutter `org.gnome.Mutter.DisplayConfig` D-Bus fallback (when `xdg_output` is absent/restricted).
  3. X11 RandR (`x11rb`) fallback when running under Xorg / without Wayland display output.
- Linux input injection & virtual pointer tracking (`injector.rs`) across three auto-selected runtime backends:
  1. **wlroots / Hyprland Native Wayland** (`zwlr_virtual_pointer_manager_v1` + `zwp_virtual_keyboard_manager_v1`) — zero permission prompt, zero root/`udev`, single-upload `memfd` XKB keymap, synchronized modifier bitmasks.
  2. **Wayland XDG Desktop Portal** (`org.freedesktop.portal.RemoteDesktop` + `PersistMode::ExplicitlyRevoked` + `reis` EIS, falling back to Portal D-Bus `notify_*` methods if `ConnectToEIS` is unsupported by the portal).
  3. **X11 Fallback** (`x11rb` `xtest` + `randr` + `query_pointer`/`warp_pointer`).
- Unified Win32 `VK_*` / PS/2 Set 1 `scan_code` + `extended` → Linux `evdev` keycode mapping (`win_key_to_linux_evdev`) using 1:1 modifiers (`Win ↔ Super`, `Alt ↔ Alt`, `Ctrl ↔ Ctrl`).
- Software-tracked global cursor coordinates on Wayland (`SoftCursorTracker`, seeded at `CursorEnter`/`set_cursor_position`) so `get_cursor_pos()` and seam-based `CursorReturn` operate reliably without Wayland global pointer query privileges.
- Non-blocking Wayland Portal initialization/permission notice in `kvm/commands.rs` (no prompt needed on Hyprland/wlroots; one-time portal prompt / notice toast only when Portal backend is selected).

### Out of Scope
- Linux **Main PC** (Controller) global capture (`org.freedesktop.portal.InputCapture` / `evdev` capture) — remains non-blocking unsupported notice (`start_hooks()` returns `false`).
- Linux Continuity adapters (`clipboard.rs`, `dragdrop.rs`, `audio.rs`, `session_sync.rs`, `open_on_linked.rs`, `cursor_scheme.rs`).
- Wire protocol changes (`KvmPacket`, `UdpMouseDelta`, `MonitorDisplay`) or `settings` UI schema changes.

---

## 2. Approaches Considered

| Approach | How it works | Trade-offs | Pick |
|---|---|---|---|
| **A. Dual-Layer Native Wayland (`wlr-virtual-pointer` + `zwp-virtual-keyboard` + `xdg-output`) + Portal/EIS Fallback + X11 (`x11rb`)** | Pure-Rust `wayland-client` binds `zwlr_virtual_pointer_manager_v1` + `zwp_virtual_keyboard_manager_v1` directly on Hyprland/Sway (zero prompts, zero daemon); falls back to `ashpd` `RemoteDesktop` + `reis` EIS for GNOME/KDE; falls back to `x11rb` on X11. | Covers CachyOS/Hyprland seamlessly out of the box without adding any C `-dev` headers (`libwayland-dev` / `libxkbcommon-dev`) to CI. Requires maintaining a clean backend trait internally. | **Picked** |
| **B. Portal-Only (`ashpd` `RemoteDesktop` + `ScreenCast` + `reis` EIS)** | Single Wayland API via XDG Desktop Portal. | Triggers system confirmation dialogs on every unpersisted start, and `xdg-desktop-portal-hyprland` lags behind GNOME/KDE on `RemoteDesktop` / `ConnectToEIS` across distro versions. | Rejected |
| **C. `/dev/uinput` Virtual Device** | Writes raw `input_event` structs to `/dev/uinput`. | Requires root / `udev` + `input` group configuration by every user and lacks compositor coordinate normalization or display layout discovery on multi-monitor Wayland. | Rejected |

---

## 3. Detailed Design

### 3.1 Dependency Footprint (`src-tauri/Cargo.toml`)
All new dependencies are **100% pure Rust**, placed strictly under `[target.'cfg(target_os = "linux")'.dependencies]` so Windows/macOS builds add zero crates and Ubuntu CI needs zero new apt `-dev` packages:
- `wayland-client`, `wayland-protocols` (features: `["client", "unstable"]`), `wayland-protocols-wlr` (feature: `["client"]`), `wayland-protocols-misc` (feature: `["client"]`)
- `x11rb` (features: `["randr", "xtest"]`)
- `ashpd` + `reis` (feature: `["tokio"]`) for GNOME/KDE Portal + EIS fallback
- Existing workspace crates already in `Cargo.lock`: `rustix` (`memfd_create` / `ftruncate`), `zbus` (GNOME `Mutter.DisplayConfig` fallback & Portal D-Bus)

### 3.2 Monitor Enumeration & Logical Coordinate Space (`monitors.rs`)
- **Primary Wayland path (`WAYLAND_DISPLAY` present):**
  1. Connect via `wayland-client`, query `wl_output` globals and `zxdg_output_manager_v1`.
  2. For each `wl_output`, read `zxdg_output_v1::Event::{LogicalPosition, LogicalSize, Name, Done}`.
  3. Normalize via pure `normalize_linux_monitors()`: discard zero-sized outputs; mark primary display as the one anchored at `(0, 0)` (or first valid output if none starts at `(0, 0)`), giving deterministic `id` / `device_name` from output connector name (e.g. `DP-1`, `HDMI-A-1`) or fallback `wl-<idx>`.
- **GNOME fallback:** If `zxdg_output_manager_v1` returns empty outputs, query `org.gnome.Mutter.DisplayConfig.GetCurrentState` over session `zbus` to read logical monitor rectangles `(x, y, width, height, primary, connector)`.
- **X11 fallback (`DISPLAY` present):** Connect via `x11rb`, query `randr::get_screen_resources_current`, `get_output_info`, and `get_crtc_info`, converting connected CRTCs to `MonitorDisplay` and setting `is_primary` from `randr::get_output_primary`.
- Fallback static `1920x1080` monitor retained only if no Wayland/X11 displays exist (e.g., headless CI).

### 3.3 Wayland Input Injection & Keymap Handling (`injector.rs`)
- **Backend Selection (`init_linux_backend`):**
  1. **Wlroots / Hyprland Native Wayland** (preferred when `WAYLAND_DISPLAY` is set and registry advertises `wl_seat` + `zwlr_virtual_pointer_manager_v1` + `zwp_virtual_keyboard_manager_v1`):
     - Persistent background thread `kvm-linux-wayland` owns `Connection`, `EventQueue`, `zwlr_virtual_pointer_v1` (created unbound to a single output so coordinates span the entire compositor bounding box), and `zwp_virtual_keyboard_v1`.
     - **Single XKB Keymap Upload (`build_us_evdev_xkb_keymap()`):** Generates standard US-evdev XKB keymap string ending with `;\n\0`, writes exact byte length to `rustix::fs::memfd_create` (rewound to offset `0`), and sends `virtual_keyboard.keymap(XkbV1, fd, size)` ONCE on device init — preventing FD exhaustion (`EMFILE`) and keymap truncation/crash issues.
     - **Pointer Coordinates (`normalize_linux_abs_pointer`):** Maps global logical target `(x, y)` into current `local_monitors` `union_rect` as `((x - union.x).clamp(0, w - 1) as u32, (y - union.y).clamp(0, h - 1) as u32)` with extents `(w as u32, h as u32)` and calls `pointer.motion_absolute(time, rx, ry, w, h)` + `pointer.frame()`. (Refreshes monitor union when `CursorEnter` / `Monitors` update.)
     - **Buttons:** Maps `1 -> 0x110 (BTN_LEFT)`, `2 -> 0x111 (BTN_RIGHT)`, `3 -> 0x112 (BTN_MIDDLE)` -> `pointer.button(...)` + `pointer.frame()`.
     - **Scroll Direction (`scale_wayland_wheel_axis`):** Follows `wl_pointer::axis` specification cleanly (**negative = UP**, **positive = DOWN** — opposite of Win32 `WHEEL_DELTA`):
       `discrete_steps = round_or_accum(-delta / 120)`, `axis_value = -(delta as f64) * 15.0 / 120.0`, dispatching `axis_source(Wheel)`, `axis_discrete` (when discrete steps != 0) or continuous `axis(...)`, plus `pointer.frame()`.
     - **Keyboard & XKB Modifier Sync (`LinuxModifierTracker`):** Translates `(vk_code, scan_code, extended)` → Linux `evdev` code via pure `win_key_to_linux_evdev`, sends `virtual_keyboard.key(time, evdev_code, state)` AND `virtual_keyboard.modifiers(depressed, 0, locked, 0)` tracking `Shift` (`0x01`), `CapsLock` (`0x02` in `locked`), `Ctrl` (`0x04`), `Alt` (`0x08`), `Super` (`0x40`).
  2. **Wayland Portal `RemoteDesktop` + `EIS` (for GNOME/KDE):** Connects on background Tokio/worker runtime via `ashpd::desktop::remote_desktop::RemoteDesktop` (`DeviceType::Keyboard | DeviceType::Pointer`, `PersistMode::ExplicitlyRevoked`), persists restore token in AnyLeap app data dir (`kvm_wayland_portal_token.txt` or runtime memory fallback), and prefers `connect_to_eis` via `reis` (`PointerAbsolute` / `Pointer` / `Keyboard`), falling back cleanly to Portal D-Bus `notify_*` calls if `connect_to_eis` returns unsupported.
  3. **X11 Fallback (`x11rb`):** Uses `x11rb` `xtest` (`MOTION_NOTIFY` / `BUTTON_PRESS` / `BUTTON_RELEASE` / `KEY_PRESS` / `KEY_RELEASE` with `evdev + 8`, button `4/5` wheel up/down) and `randr` geometry.

### 3.4 Software Cursor Tracking on Wayland (`injector.rs`)
- Pure `SoftCursorTracker`:
  - Stores current `(x, y)` inside `union_rect`.
  - Updated on every `set_cursor_position(x, y)` (so `CursorEnter` seeds exact entry position immediately!).
  - On Wayland, `get_cursor_pos()` returns the tracked position (clamped within active monitors if needed) instead of `None`, allowing `commands.rs`'s `delta_rx` loop (`receiver_step_with_seams` / `receiver_step_with_dwell`) to drive smooth motion and seam `CursorReturn` back to Main PC seamlessly.
  - On X11, `get_cursor_pos()` queries live `query_pointer`.

### 3.5 Server Lifecycle & User Notices (`injector.rs` + `kvm/commands.rs`)
- Add `injector::prepare_linux_remote_control(app_data_dir) -> Result<(), String>` (called non-blockingly when `start_server_inner` starts Linked PC on `#[cfg(target_os = "linux")]`):
  - On **CachyOS / Hyprland**, tier-1 wlroots backend succeeds silently (`Ok(())`, zero prompts).
  - If Portal backend is needed and fails/is cancelled or no Wayland/X11 session is reachable, emits a clear non-blocking `kvm-continuity-notice` explaining what happened without blocking discovery or QUIC pairing.

---

## 4. Verification Plan

1. **Pure-Rust Cross-Platform Unit Tests (`#[cfg(any(target_os = "linux", test))]`, run on macOS, Windows, and Linux CI):**
   - `monitors.rs`: `normalize_linux_monitors` (mixed negative/scaled coordinates, primary selection at `(0, 0)` vs first-fallback, empty filtering).
   - `injector.rs`:
     - `win_key_to_linux_evdev`: full letter, digit, punctuation, navigation, arrow, F1..F20, numpad, and 1:1 modifier mapping (`VK_LWIN/RWIN -> KEY_LEFTMETA/RIGHTMETA`, `VK_LMENU/RMENU -> KEY_LEFTALT/RIGHTALT`, `VK_LCONTROL/RCONTROL -> KEY_LEFTCTRL/RIGHTCTRL`, Enter vs KeypadEnter `96`, Backspace `14` vs Delete `111`).
     - `LinuxModifierTracker`: simultaneous L/R Shift/Ctrl/Alt/Super tracking, XKB `mods_depressed` mask bits (`0x01, 0x04, 0x08, 0x40`), `CapsLock` `mods_locked` (`0x02`) toggle on press only, and clean reset.
     - `normalize_linux_abs_pointer`: negative-origin multi-monitor mapping (`(-1920, 0)` + `(0, 0)` -> normalized unsigned coordinates within full union extents).
     - `scale_wayland_wheel_axis`: verifies inverted sign vs Win32 (`+120 -> negative up`, `-120 -> positive down`), continuous high-res accumulator, and X11 button `4`/`5` counts.
     - `SoftCursorTracker`: `set_cursor_position` seeding & clamping, `inject_mouse_delta` fallback stepping.
     - `build_us_evdev_xkb_keymap`: ends cleanly with `\0`, includes valid XKB `evdev+aliases(qwerty)` sections.
2. **Quality & Cross-OS CI Checks:**
   - Update `Cargo.lock` so `cargo check --locked` passes on Linux CI (`ubuntu-latest` `linux-check`) as well as macOS and Windows.
   - Local macOS verification after final edits: `npm run check:tauri-versions`, `npm test`, `npm run build`, `cargo fmt --check`, `cargo check --all-targets --locked`, `cargo test --locked`.
3. **Manual Two-PC Hardware Checklist (Section H added to `docs/MANUAL-TEST-CHECKLIST.md`):**
   - CachyOS/Hyprland **Linked PC** ↔ Windows/macOS **Main PC**: zero-prompt startup, `monitors.rs` logical coordinates on scaled/multi-monitor Hyprland, seam entry/return, click/drag, scroll direction (`up`/`down`), modifier shortcuts (`Super+Enter`/app shortcuts, `Ctrl+C/V`, `Alt+Tab`), and file transfer into `~/Downloads/AnyLeap_Transfers`.
