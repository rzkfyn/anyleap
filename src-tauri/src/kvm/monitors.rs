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
}
