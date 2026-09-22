//! What a display is currently doing, which is what the tone map needs to be correct.
//!
//! Two numbers decide the whole HDR→SDR conversion and neither can be guessed:
//!
//! * **SDR white level.** WGC hands us scRGB, where 1.0 is 80 nits by definition. But the white of
//!   the Windows desktop on an HDR display is not 80 nits — it is whatever the "SDR content
//!   brightness" slider says, commonly 200-ish. Dividing by that is what makes a captured desktop
//!   come out the same brightness it looked, instead of blown out by 2.5x.
//! * **Peak luminance.** The tone curve needs to know how far above white the source can go. Using
//!   a fixed guess crushes highlights on a dim panel and wastes range on a bright one.
//!
//! The colour space reported by DXGI is also the honest answer to "is HDR on right now", which is
//! what `toneMap: "auto"` resolves against — far better than asking the user to keep a setting in
//! sync with their display.

use serde::Serialize;
use windows::core::Interface;
use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_SDR_WHITE_LEVEL,
};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput6, DXGI_OUTPUT_DESC1,
};
use windows::Win32::Graphics::Gdi::HMONITOR;

/// scRGB's fixed reference: a value of 1.0 is 80 nits.
pub const SCRGB_WHITE_NITS: f32 = 80.0;

/// What Windows falls back to when a display will not say. 200 nits is the default position of the
/// SDR brightness slider.
const DEFAULT_SDR_WHITE_NITS: f32 = 200.0;

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DisplayHdr {
    /// True when the desktop is currently composited in HDR on this output.
    pub enabled: bool,
    /// Peak the panel claims it can hit, in nits.
    pub max_nits: f32,
    /// What the desktop's white is worth, in nits.
    pub sdr_white_nits: f32,
    pub bits_per_color: u32,
}

impl Default for DisplayHdr {
    fn default() -> Self {
        DisplayHdr {
            enabled: false,
            max_nits: SCRGB_WHITE_NITS,
            sdr_white_nits: SCRGB_WHITE_NITS,
            bits_per_color: 8,
        }
    }
}

impl DisplayHdr {
    /// Desktop white expressed in scRGB units — the divisor that puts SDR white back at 1.0.
    pub fn white_scale(&self) -> f32 {
        (self.sdr_white_nits / SCRGB_WHITE_NITS).max(1.0)
    }

    /// How far above white the source can go, once white is normalised to 1.0. This is the `peak`
    /// the tone curve is built for.
    pub fn peak(&self) -> f32 {
        (self.max_nits / self.sdr_white_nits).max(1.0)
    }
}

pub fn probe(monitor: HMONITOR, gdi_device: &str) -> DisplayHdr {
    let mut info = DisplayHdr::default();

    if let Some(desc) = output_desc(monitor) {
        info.enabled = desc.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
        info.bits_per_color = desc.BitsPerColor;
        if desc.MaxLuminance > 0.0 {
            info.max_nits = desc.MaxLuminance;
        }
    }

    info.sdr_white_nits = sdr_white_level(gdi_device).unwrap_or(if info.enabled {
        DEFAULT_SDR_WHITE_NITS
    } else {
        SCRGB_WHITE_NITS
    });

    // A panel that reports a peak below its own desktop white is lying; treating it as 1.0 at
    // least keeps the tone curve monotonic.
    if info.max_nits < info.sdr_white_nits {
        info.max_nits = info.sdr_white_nits;
    }
    info
}

fn output_desc(monitor: HMONITOR) -> Option<DXGI_OUTPUT_DESC1> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        for i in 0.. {
            let adapter: IDXGIAdapter1 = match factory.EnumAdapters1(i) {
                Ok(a) => a,
                Err(_) => break,
            };
            for j in 0.. {
                let output = match adapter.EnumOutputs(j) {
                    Ok(o) => o,
                    Err(_) => break,
                };
                let Ok(output6) = output.cast::<IDXGIOutput6>() else {
                    continue;
                };
                let Ok(desc) = output6.GetDesc1() else { continue };
                if desc.Monitor == monitor {
                    return Some(desc);
                }
            }
        }
    }
    None
}

/// The SDR brightness slider's position for one display path, in nits.
fn sdr_white_level(gdi_device: &str) -> Option<f32> {
    let (adapter_id, source_id) = crate::monitors::source_path(gdi_device)?;
    unsafe {
        let mut level = DISPLAYCONFIG_SDR_WHITE_LEVEL {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
                size: std::mem::size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32,
                adapterId: adapter_id,
                id: source_id,
            },
            ..Default::default()
        };
        if DisplayConfigGetDeviceInfo(&mut level.header) != ERROR_SUCCESS.0 as i32 {
            return None;
        }
        // Reported in thousandths of the scRGB reference: 1000 means 80 nits.
        Some(level.SDRWhiteLevel as f32 / 1000.0 * SCRGB_WHITE_NITS)
    }
}
