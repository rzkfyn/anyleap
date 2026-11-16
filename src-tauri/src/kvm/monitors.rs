#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct MonitorDisplay {
    pub id: String,
    #[serde(default)]
    pub device_name: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub is_primary: bool,
}

impl MonitorDisplay {
    /// Stable monitor identifier across reconnects (`device_name`, falling back to `id`).
    pub fn effective_id(&self) -> &str {
        if !self.device_name.is_empty() {
            &self.device_name
        } else {
            &self.id
        }
    }
}

pub fn get_local_monitors() -> Vec<MonitorDisplay> {
    #[cfg(windows)]
    unsafe {
        use windows::core::BOOL;
        use windows::Win32::Foundation::{LPARAM, RECT};
        use windows::Win32::Graphics::Gdi::*;

        let mut list: Vec<MonitorDisplay> = Vec::new();
        let list_ptr = &mut list as *mut Vec<MonitorDisplay> as isize;

        unsafe extern "system" fn monitor_proc(
            hmonitor: HMONITOR,
            _hdc: HDC,
            _rect: *mut RECT,
            lparam: LPARAM,
        ) -> BOOL {
            let list = &mut *(lparam.0 as *mut Vec<MonitorDisplay>);
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;

            if GetMonitorInfoW(hmonitor, &mut info.monitorInfo).as_bool() {
                let rc = info.monitorInfo.rcMonitor;
                let w = rc.right - rc.left;
                let h = rc.bottom - rc.top;
                let is_primary = (info.monitorInfo.dwFlags & 1) != 0; // MONITORINFOF_PRIMARY = 1
                let nul = info
                    .szDevice
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(info.szDevice.len());
                let raw_device = String::from_utf16_lossy(&info.szDevice[..nul]);
                let device_name = if raw_device.is_empty() {
                    format!("mon-{}", list.len() + 1)
                } else {
                    raw_device
                };
                list.push(MonitorDisplay {
                    id: device_name.clone(),
                    device_name,
                    x: rc.left,
                    y: rc.top,
                    width: w,
                    height: h,
                    is_primary,
                });
            }
            BOOL(1)
        }

        let _ = EnumDisplayMonitors(None, None, Some(monitor_proc), LPARAM(list_ptr));

        if !list.is_empty() {
            return list;
        }
    }

    #[cfg(target_os = "macos")]
    {
        let list = macos_display::enumerate();
        if !list.is_empty() {
            return list;
        }
    }

    #[cfg(target_os = "linux")]
    {
        let list = linux_display::enumerate();
        if !list.is_empty() {
            return list;
        }
    }

    // Fallback
    vec![MonitorDisplay {
        id: r"\\.\DISPLAY1".to_string(),
        device_name: r"\\.\DISPLAY1".to_string(),
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
        is_primary: true,
    }]
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LinuxRawMonitor {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub is_primary: bool,
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn normalize_linux_monitors(raw: Vec<LinuxRawMonitor>) -> Vec<MonitorDisplay> {
    let mut list: Vec<MonitorDisplay> = raw
        .into_iter()
        .filter(|m| m.width > 0 && m.height > 0)
        .enumerate()
        .map(|(idx, m)| {
            let trimmed = m.name.trim();
            let id = if trimmed.is_empty() {
                format!("wl-{}", idx + 1)
            } else {
                trimmed.to_string()
            };
            MonitorDisplay {
                device_name: id.clone(),
                id,
                x: m.x,
                y: m.y,
                width: m.width,
                height: m.height,
                is_primary: m.is_primary,
            }
        })
        .collect();

    if list.is_empty() {
        return list;
    }

    // Ensure exactly one primary monitor: honor explicit primary first, else (0, 0), else index 0.
    let primary_idx = list
        .iter()
        .position(|m| m.is_primary)
        .or_else(|| list.iter().position(|m| m.x == 0 && m.y == 0))
        .unwrap_or(0);
    for (idx, mon) in list.iter_mut().enumerate() {
        mon.is_primary = idx == primary_idx;
    }
    list
}

#[cfg(target_os = "linux")]
mod linux_display {
    use super::{normalize_linux_monitors, LinuxRawMonitor, MonitorDisplay};
    use std::collections::{BTreeMap, HashMap};

    #[derive(Default)]
    struct WaylandOutputAccum {
        name: Option<String>,
        logical_x: Option<i32>,
        logical_y: Option<i32>,
        logical_w: Option<i32>,
        logical_h: Option<i32>,
        geom_x: i32,
        geom_y: i32,
        mode_w: i32,
        mode_h: i32,
        has_current_mode: bool,
        scale: i32,
    }

    struct WaylandDisplayState {
        outputs: BTreeMap<u32, WaylandOutputAccum>,
    }

    impl
        wayland_client::Dispatch<
            wayland_client::protocol::wl_registry::WlRegistry,
            wayland_client::globals::GlobalListContents,
        > for WaylandDisplayState
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

    impl wayland_client::Dispatch<wayland_client::protocol::wl_output::WlOutput, u32>
        for WaylandDisplayState
    {
        fn event(
            state: &mut Self,
            _proxy: &wayland_client::protocol::wl_output::WlOutput,
            event: wayland_client::protocol::wl_output::Event,
            output_global_id: &u32,
            _conn: &wayland_client::Connection,
            _qh: &wayland_client::QueueHandle<Self>,
        ) {
            use wayland_client::protocol::wl_output::{Event, Mode};
            use wayland_client::WEnum;

            let entry = state.outputs.entry(*output_global_id).or_default();
            match event {
                Event::Geometry { x, y, .. } => {
                    entry.geom_x = x;
                    entry.geom_y = y;
                }
                Event::Mode {
                    flags,
                    width,
                    height,
                    ..
                } => {
                    let is_current = matches!(flags, WEnum::Value(m) if m.contains(Mode::Current));
                    if is_current || !entry.has_current_mode {
                        entry.mode_w = width;
                        entry.mode_h = height;
                        if is_current {
                            entry.has_current_mode = true;
                        }
                    }
                }
                Event::Scale { factor } => {
                    entry.scale = factor.max(1);
                }
                Event::Name { name } => {
                    if entry.name.as_deref().unwrap_or("").is_empty() && !name.is_empty() {
                        entry.name = Some(name);
                    }
                }
                _ => {}
            }
        }
    }

    impl
        wayland_client::Dispatch<
            wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_manager_v1::ZxdgOutputManagerV1,
            (),
        > for WaylandDisplayState
    {
        fn event(
            _state: &mut Self,
            _proxy: &wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_manager_v1::ZxdgOutputManagerV1,
            _event: wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_manager_v1::Event,
            _data: &(),
            _conn: &wayland_client::Connection,
            _qh: &wayland_client::QueueHandle<Self>,
        ) {
        }
    }

    impl
        wayland_client::Dispatch<
            wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_v1::ZxdgOutputV1,
            u32,
        > for WaylandDisplayState
    {
        fn event(
            state: &mut Self,
            _proxy: &wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_v1::ZxdgOutputV1,
            event: wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_v1::Event,
            output_global_id: &u32,
            _conn: &wayland_client::Connection,
            _qh: &wayland_client::QueueHandle<Self>,
        ) {
            use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_v1::Event;

            let entry = state.outputs.entry(*output_global_id).or_default();
            match event {
                Event::LogicalPosition { x, y } => {
                    entry.logical_x = Some(x);
                    entry.logical_y = Some(y);
                }
                Event::LogicalSize { width, height } => {
                    entry.logical_w = Some(width);
                    entry.logical_h = Some(height);
                }
                Event::Name { name } => {
                    if !name.is_empty() {
                        entry.name = Some(name);
                    }
                }
                _ => {}
            }
        }
    }

    fn enumerate_wayland() -> Vec<LinuxRawMonitor> {
        use wayland_client::globals::registry_queue_init;
        use wayland_client::protocol::wl_output::WlOutput;
        use wayland_client::{Connection, Proxy};
        use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_manager_v1::ZxdgOutputManagerV1;

        if std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return Vec::new();
        }
        let Ok(conn) = Connection::connect_to_env() else {
            return Vec::new();
        };
        let Ok((globals, mut queue)) = registry_queue_init::<WaylandDisplayState>(&conn) else {
            return Vec::new();
        };
        let qh = queue.handle();

        let xdg_mgr: Option<ZxdgOutputManagerV1> = globals.bind(&qh, 1..=3, ()).ok();
        let mut state = WaylandDisplayState {
            outputs: BTreeMap::new(),
        };

        let mut bound_outputs = Vec::new();
        let mut bound_xdg_outputs = Vec::new();
        for global in globals.contents().clone_list() {
            if global.interface == WlOutput::interface().name {
                let ver = global.version.clamp(1, 4);
                let out: WlOutput = globals.registry().bind(global.name, ver, &qh, global.name);
                if let Some(ref mgr) = xdg_mgr {
                    bound_xdg_outputs.push(mgr.get_xdg_output(&out, &qh, global.name));
                }
                state.outputs.entry(global.name).or_default();
                bound_outputs.push(out);
            }
        }

        if bound_outputs.is_empty() {
            return Vec::new();
        }

        if queue.roundtrip(&mut state).is_err() {
            return Vec::new();
        }

        for xo in bound_xdg_outputs {
            xo.destroy();
        }
        if let Some(mgr) = xdg_mgr {
            mgr.destroy();
        }
        for out in bound_outputs {
            if out.version() >= 3 {
                out.release();
            }
        }
        let _ = conn.flush();

        state
            .outputs
            .into_values()
            .map(|o| {
                let scale = o.scale.max(1);
                let x = o.logical_x.unwrap_or(o.geom_x);
                let y = o.logical_y.unwrap_or(o.geom_y);
                let width = o
                    .logical_w
                    .unwrap_or_else(|| (o.mode_w + scale / 2) / scale);
                let height = o
                    .logical_h
                    .unwrap_or_else(|| (o.mode_h + scale / 2) / scale);
                LinuxRawMonitor {
                    name: o.name.unwrap_or_default(),
                    x,
                    y,
                    width,
                    height,
                    is_primary: false,
                }
            })
            .collect()
    }

    type MutterPhysicalMode = (
        String,
        i32,
        i32,
        f64,
        f64,
        Vec<f64>,
        HashMap<String, zbus::zvariant::OwnedValue>,
    );
    type MutterPhysicalMonitor = (
        (String, String, String, String),
        Vec<MutterPhysicalMode>,
        HashMap<String, zbus::zvariant::OwnedValue>,
    );
    type MutterLogicalMonitor = (
        i32,
        i32,
        f64,
        u32,
        bool,
        Vec<(String, String, String, String)>,
        HashMap<String, zbus::zvariant::OwnedValue>,
    );
    type MutterDisplayState = (
        u32,
        Vec<MutterPhysicalMonitor>,
        Vec<MutterLogicalMonitor>,
        HashMap<String, zbus::zvariant::OwnedValue>,
    );

    fn enumerate_gnome_mutter_dbus() -> Vec<LinuxRawMonitor> {
        let Ok(conn) = zbus::blocking::Connection::session() else {
            return Vec::new();
        };
        let Ok(proxy) = zbus::blocking::Proxy::new(
            &conn,
            "org.gnome.Mutter.DisplayConfig",
            "/org/gnome/Mutter/DisplayConfig",
            "org.gnome.Mutter.DisplayConfig",
        ) else {
            return Vec::new();
        };
        let Ok((_, physical, logical, _)): Result<MutterDisplayState, _> =
            proxy.call("GetCurrentState", &())
        else {
            return Vec::new();
        };

        let mut current_mode_by_connector: HashMap<String, (i32, i32)> = HashMap::new();
        for ((connector, _vendor, _product, _serial), modes, _props) in physical {
            if let Some((_, w, h, _, _, _, _)) = modes.into_iter().find(|mode| {
                mode.6
                    .get("is-current")
                    .and_then(|v| v.downcast_ref::<bool>().ok())
                    .unwrap_or(false)
            }) {
                current_mode_by_connector.insert(connector, (w, h));
            }
        }

        let mut out = Vec::new();
        for (x, y, scale, transform, primary, monitors, _props) in logical {
            let Some((connector, _, _, _)) = monitors.into_iter().next() else {
                continue;
            };
            let Some((pw, ph)) = current_mode_by_connector.get(&connector).copied() else {
                continue;
            };
            // Mutter transform values 1, 3, 5, 7 swap width and height (90/270 degrees).
            let (rw, rh) = if transform % 2 == 1 {
                (ph, pw)
            } else {
                (pw, ph)
            };
            let eff_scale = if scale > 0.0 { scale } else { 1.0 };
            out.push(LinuxRawMonitor {
                name: connector,
                x,
                y,
                width: (rw as f64 / eff_scale).round() as i32,
                height: (rh as f64 / eff_scale).round() as i32,
                is_primary: primary,
            });
        }
        out
    }

    fn enumerate_x11() -> Vec<LinuxRawMonitor> {
        use x11rb::connection::Connection as _;
        use x11rb::protocol::randr::ConnectionExt as _;

        if std::env::var_os("DISPLAY").is_none() {
            return Vec::new();
        }
        let Ok((conn, screen_num)) = x11rb::connect(None) else {
            return Vec::new();
        };
        let Some(screen) = conn.setup().roots.get(screen_num) else {
            return Vec::new();
        };
        let root = screen.root;

        if let Ok(res_cookie) = conn.randr_get_screen_resources_current(root) {
            if let Ok(resources) = res_cookie.reply() {
                let primary_output = conn
                    .randr_get_output_primary(root)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .map(|r| r.output)
                    .unwrap_or(0);

                let mut raw = Vec::new();
                for &output in &resources.outputs {
                    let Ok(out_cookie) =
                        conn.randr_get_output_info(output, resources.config_timestamp)
                    else {
                        continue;
                    };
                    let Ok(out_info) = out_cookie.reply() else {
                        continue;
                    };
                    if out_info.crtc == 0 {
                        continue;
                    }
                    let Ok(crtc_cookie) =
                        conn.randr_get_crtc_info(out_info.crtc, resources.config_timestamp)
                    else {
                        continue;
                    };
                    let Ok(crtc) = crtc_cookie.reply() else {
                        continue;
                    };
                    let name = String::from_utf8_lossy(&out_info.name).into_owned();
                    raw.push(LinuxRawMonitor {
                        name,
                        x: i32::from(crtc.x),
                        y: i32::from(crtc.y),
                        width: i32::from(crtc.width),
                        height: i32::from(crtc.height),
                        is_primary: output == primary_output,
                    });
                }
                if !raw.is_empty() {
                    return raw;
                }
            }
        }

        vec![LinuxRawMonitor {
            name: format!("x11-screen-{screen_num}"),
            x: 0,
            y: 0,
            width: i32::from(screen.width_in_pixels),
            height: i32::from(screen.height_in_pixels),
            is_primary: true,
        }]
    }

    pub fn enumerate() -> Vec<MonitorDisplay> {
        let mut raw = enumerate_wayland();
        if raw.is_empty() && std::env::var_os("WAYLAND_DISPLAY").is_some() {
            raw = enumerate_gnome_mutter_dbus();
        }
        if raw.is_empty() {
            raw = enumerate_x11();
        }
        normalize_linux_monitors(raw)
    }
}

#[cfg(any(target_os = "macos", test))]
fn monitor_from_cg_rect(
    display_id: u32,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    is_primary: bool,
) -> Option<MonitorDisplay> {
    let w = width.round() as i32;
    let h = height.round() as i32;
    if w <= 0 || h <= 0 {
        return None;
    }
    let name = format!("cg-{display_id}");
    Some(MonitorDisplay {
        id: name.clone(),
        device_name: name,
        x: x.round() as i32,
        y: y.round() as i32,
        width: w,
        height: h,
        is_primary,
    })
}

#[cfg(any(target_os = "macos", test))]
fn normalize_macos_monitors(mut list: Vec<MonitorDisplay>) -> Vec<MonitorDisplay> {
    if !list.is_empty() && !list.iter().any(|m| m.is_primary) {
        list[0].is_primary = true;
    }
    list
}

#[cfg(target_os = "macos")]
mod macos_display {
    use super::{monitor_from_cg_rect, normalize_macos_monitors, MonitorDisplay};

    type CGDirectDisplayID = u32;
    type CGError = i32;
    const K_CG_ERROR_SUCCESS: CGError = 0;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGPoint {
        x: f64,
        y: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGSize {
        width: f64,
        height: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGRect {
        origin: CGPoint,
        size: CGSize,
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGGetActiveDisplayList(
            max_displays: u32,
            active_displays: *mut CGDirectDisplayID,
            display_count: *mut u32,
        ) -> CGError;
        fn CGDisplayBounds(display: CGDirectDisplayID) -> CGRect;
        fn CGDisplayIsMain(display: CGDirectDisplayID) -> u32;
    }

    pub fn enumerate() -> Vec<MonitorDisplay> {
        unsafe {
            let mut count: u32 = 0;
            if CGGetActiveDisplayList(0, std::ptr::null_mut(), &mut count) != K_CG_ERROR_SUCCESS
                || count == 0
            {
                return Vec::new();
            }
            let mut ids = vec![0u32; count as usize];
            let mut actual: u32 = 0;
            if CGGetActiveDisplayList(count, ids.as_mut_ptr(), &mut actual) != K_CG_ERROR_SUCCESS
                || actual == 0
            {
                return Vec::new();
            }
            let list = ids[..actual as usize]
                .iter()
                .filter_map(|&did| {
                    let rect = CGDisplayBounds(did);
                    let is_primary = CGDisplayIsMain(did) != 0;
                    monitor_from_cg_rect(
                        did,
                        rect.origin.x,
                        rect.origin.y,
                        rect.size.width,
                        rect.size.height,
                        is_primary,
                    )
                })
                .collect();
            normalize_macos_monitors(list)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_cg_rect_conversion_and_primary_normalization() {
        let main = monitor_from_cg_rect(1, 0.0, 0.0, 1512.0, 982.0, true).expect("valid main");
        assert_eq!(main.id, "cg-1");
        assert_eq!(main.device_name, "cg-1");
        assert_eq!((main.x, main.y, main.width, main.height), (0, 0, 1512, 982));
        assert!(main.is_primary);

        let left =
            monitor_from_cg_rect(69731904, -1920.0, -120.0, 1920.0, 1080.0, false).expect("left");
        assert_eq!(left.effective_id(), "cg-69731904");
        assert_eq!(
            (left.x, left.y, left.width, left.height),
            (-1920, -120, 1920, 1080)
        );
        assert!(!left.is_primary);

        assert!(monitor_from_cg_rect(2, 0.0, 0.0, 0.0, 1080.0, false).is_none());
        assert!(monitor_from_cg_rect(2, 0.0, 0.0, 1920.0, -10.0, false).is_none());

        let normalized = normalize_macos_monitors(vec![
            monitor_from_cg_rect(10, 0.0, 0.0, 1440.0, 900.0, false).unwrap(),
            monitor_from_cg_rect(20, 1440.0, 0.0, 1920.0, 1080.0, false).unwrap(),
        ]);
        assert!(normalized[0].is_primary);
        assert!(!normalized[1].is_primary);
    }

    #[test]
    fn monitor_display_deserializes_legacy_and_uses_device_name_as_id() {
        let legacy = r#"{"id":"mon-1","x":0,"y":0,"width":1920,"height":1080,"is_primary":true}"#;
        let m: MonitorDisplay = serde_json::from_str(legacy).expect("deserialize legacy");
        assert_eq!(m.device_name, "");
        assert_eq!(m.effective_id(), "mon-1");

        let modern = MonitorDisplay {
            id: r"\\.\DISPLAY1".into(),
            device_name: r"\\.\DISPLAY1".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            is_primary: true,
        };
        assert_eq!(modern.effective_id(), r"\\.\DISPLAY1");

        let local = get_local_monitors();
        assert!(!local.is_empty());
        for mon in &local {
            assert!(!mon.device_name.is_empty());
            assert_eq!(mon.id, mon.device_name);
        }
    }

    #[test]
    fn linux_monitors_normalize_coordinates_names_and_primary() {
        let normalized = normalize_linux_monitors(vec![
            LinuxRawMonitor {
                name: "DP-1".into(),
                x: -1920,
                y: 0,
                width: 1920,
                height: 1080,
                is_primary: false,
            },
            LinuxRawMonitor {
                name: "broken-w".into(),
                x: 0,
                y: 0,
                width: 0,
                height: 1080,
                is_primary: true,
            },
            LinuxRawMonitor {
                name: "".into(),
                x: 0,
                y: 0,
                width: 2560,
                height: 1440,
                is_primary: false,
            },
            LinuxRawMonitor {
                name: "broken-h".into(),
                x: 0,
                y: 0,
                width: 1920,
                height: -10,
                is_primary: false,
            },
        ]);

        assert_eq!(normalized.len(), 2);
        assert_eq!(normalized[0].id, "DP-1");
        assert_eq!(normalized[0].device_name, "DP-1");
        assert_eq!(
            (
                normalized[0].x,
                normalized[0].y,
                normalized[0].width,
                normalized[0].height
            ),
            (-1920, 0, 1920, 1080)
        );
        assert!(!normalized[0].is_primary);

        assert_eq!(normalized[1].id, "wl-2");
        assert_eq!(normalized[1].device_name, "wl-2");
        assert_eq!(
            (
                normalized[1].x,
                normalized[1].y,
                normalized[1].width,
                normalized[1].height
            ),
            (0, 0, 2560, 1440)
        );
        assert!(normalized[1].is_primary);

        let fallback_primary = normalize_linux_monitors(vec![
            LinuxRawMonitor {
                name: "HDMI-A-1".into(),
                x: 100,
                y: 0,
                width: 1920,
                height: 1080,
                is_primary: false,
            },
            LinuxRawMonitor {
                name: "DP-2".into(),
                x: 2020,
                y: 0,
                width: 1920,
                height: 1080,
                is_primary: false,
            },
        ]);
        assert_eq!(fallback_primary.len(), 2);
        assert!(fallback_primary[0].is_primary);
        assert!(!fallback_primary[1].is_primary);
    }
}
