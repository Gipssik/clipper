//! Monitor enumeration.
//!
//! The settings UI needs a list a human can recognise, and the config file needs a name that
//! survives a reboot. Neither is available from one API: `EnumDisplayMonitors` gives the GDI
//! device name (`\\.\DISPLAY1`) and the HMONITOR that WGC wants, while only `QueryDisplayConfig`
//! knows the name printed on the bezel ("LG HDR 4K"). So we enumerate with the first and decorate
//! with the second, joining on the GDI name.
//!
//! Device names shuffle when displays are replugged, which is why the config stores both and
//! matches on the friendly name first.

use std::collections::HashMap;

use serde::Serialize;
use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
    DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
    DISPLAYCONFIG_TARGET_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS,
};
use windows::core::BOOL;
use windows::Win32::Foundation::{ERROR_SUCCESS, LPARAM, LUID, RECT, TRUE};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;

#[derive(Clone)]
pub struct Monitor {
    pub handle: HMONITOR,
    /// GDI device name, e.g. `\\.\DISPLAY1`.
    pub device: String,
    /// What is printed on the bezel, when the display bothers to report it.
    pub friendly: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

/// The shape handed to the Electron settings UI.
#[derive(Serialize)]
pub struct MonitorInfo {
    pub device: String,
    pub friendly: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
    pub hdr: crate::display::DisplayHdr,
}

impl From<&Monitor> for MonitorInfo {
    fn from(m: &Monitor) -> Self {
        MonitorInfo {
            device: m.device.clone(),
            friendly: m.friendly.clone(),
            width: m.width,
            height: m.height,
            primary: m.primary,
            hdr: crate::display::probe(m.handle, &m.device),
        }
    }
}

pub fn list() -> windows::core::Result<Vec<Monitor>> {
    let mut monitors: Vec<Monitor> = Vec::new();
    unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(enum_proc),
            LPARAM(&mut monitors as *mut Vec<Monitor> as isize),
        )
        .ok()?;
    }

    let friendly = friendly_names();
    for m in &mut monitors {
        if let Some(name) = friendly.get(&m.device) {
            m.friendly = name.clone();
        }
    }
    Ok(monitors)
}

/// Resolves a config value to a monitor. Friendly name first (survives replugging), then the GDI
/// device name, then a bare index for convenience on the command line. An empty selector means
/// the primary display.
pub fn resolve(monitors: &[Monitor], selector: &str) -> Option<Monitor> {
    if selector.is_empty() {
        return monitors
            .iter()
            .find(|m| m.primary)
            .or_else(|| monitors.first())
            .cloned();
    }
    monitors
        .iter()
        .find(|m| m.friendly.eq_ignore_ascii_case(selector))
        .or_else(|| monitors.iter().find(|m| m.device.eq_ignore_ascii_case(selector)))
        .or_else(|| {
            selector
                .parse::<usize>()
                .ok()
                .and_then(|i| monitors.get(i))
        })
        .cloned()
}

unsafe extern "system" fn enum_proc(
    handle: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    let monitors = &mut *(lparam.0 as *mut Vec<Monitor>);

    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    if GetMonitorInfoW(handle, &mut info as *mut _ as *mut MONITORINFO).as_bool() {
        let device = wide_to_string(&info.szDevice);
        let rc = info.monitorInfo.rcMonitor;
        monitors.push(Monitor {
            handle,
            friendly: device.clone(),
            device,
            width: (rc.right - rc.left).unsigned_abs(),
            height: (rc.bottom - rc.top).unsigned_abs(),
            primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
        });
    }
    TRUE
}

/// One active display path, reduced to the bits anything else here needs.
struct Path {
    gdi: String,
    friendly: String,
    adapter: LUID,
    source_id: u32,
}

/// GDI device name -> bezel name, for every active display path.
fn friendly_names() -> HashMap<String, String> {
    active_paths()
        .into_iter()
        .filter(|p| !p.gdi.is_empty() && !p.friendly.is_empty())
        .map(|p| (p.gdi, p.friendly))
        .collect()
}

/// The adapter and source id behind a GDI device name, which is how `DisplayConfigGetDeviceInfo`
/// wants a display identified when asked anything else about it.
pub fn source_path(gdi_device: &str) -> Option<(LUID, u32)> {
    active_paths()
        .into_iter()
        .find(|p| p.gdi.eq_ignore_ascii_case(gdi_device))
        .map(|p| (p.adapter, p.source_id))
}

fn active_paths() -> Vec<Path> {
    let mut out = Vec::new();
    unsafe {
        let (mut n_paths, mut n_modes) = (0u32, 0u32);
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut n_paths, &mut n_modes)
            != ERROR_SUCCESS
        {
            return out;
        }

        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); n_paths as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); n_modes as usize];
        if QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut n_paths,
            paths.as_mut_ptr(),
            &mut n_modes,
            modes.as_mut_ptr(),
            None,
        ) != ERROR_SUCCESS
        {
            return out;
        }
        paths.truncate(n_paths as usize);

        for path in &paths {
            let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            source.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
            source.header.size = std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
            source.header.adapterId = path.sourceInfo.adapterId;
            source.header.id = path.sourceInfo.id;
            if DisplayConfigGetDeviceInfo(&mut source.header) != ERROR_SUCCESS.0 as i32 {
                continue;
            }

            let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
            target.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
            target.header.size = std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
            target.header.adapterId = path.targetInfo.adapterId;
            target.header.id = path.targetInfo.id;
            let friendly = if DisplayConfigGetDeviceInfo(&mut target.header) == ERROR_SUCCESS.0 as i32
            {
                wide_to_string(&target.monitorFriendlyDeviceName)
            } else {
                String::new()
            };

            out.push(Path {
                gdi: wide_to_string(&source.viewGdiDeviceName),
                friendly,
                adapter: path.sourceInfo.adapterId,
                source_id: path.sourceInfo.id,
            });
        }
    }
    out
}

fn wide_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}
