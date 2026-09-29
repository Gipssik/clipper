//! The one D3D11 device everything shares.
//!
//! Capture hands us textures on this device, the tone-map shader reads them on this device, and
//! the Media Foundation encoder is given this device through `MFT_MESSAGE_SET_D3D_MANAGER`. One
//! device is what keeps a frame on the GPU from capture to bitstream — the moment two devices are
//! involved, every frame has to go through system memory and the whole lightweight argument
//! collapses.
//!
//! **And it is the device of the GPU driving the monitor, not whichever GPU Windows lists first.**
//! On a desktop with one card those are the same thing. On a laptop they routinely are not: the
//! built-in panel hangs off the integrated GPU, an HDMI port is often wired to the discrete one,
//! and which of the two is adapter 0 depends on the graphics preference Windows has for *this*
//! executable. A device on the wrong adapter still works — WGC copies every frame across to it —
//! but that copy goes through system memory on both GPUs at the full refresh rate, which is
//! exactly the cost this module exists to avoid. The adapter that owns the output is the one the
//! desktop is already composed on, so capture, convert and encode all happen where the pixels are.

use windows::core::{Interface, Result};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Win32::Foundation::{HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1};
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice;

pub struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    /// The same device seen through WinRT, which is what the capture frame pool takes.
    pub winrt: IDirect3DDevice,
    /// Which physical GPU this is. The encoder enumerates only the transforms that belong to it —
    /// see `encoder::enumerate`.
    pub luid: LUID,
    /// The adapter's own name, for the log: on a machine with two GPUs, "which one is recording"
    /// is the first question anybody asks.
    pub adapter: String,
}

/// Creates the device on the adapter that drives `monitor`, or on the default adapter when no
/// adapter claims it (a monitor that has just gone away, or a driver that enumerates no outputs).
pub fn create(monitor: Option<HMONITOR>) -> Result<Gpu> {
    // An adapter that owns the output but will not make a device with these flags is not a reason
    // to record nothing; the default adapter is what this always used before.
    let (device, context) = match monitor.and_then(adapter_for) {
        Some(adapter) => make_device(Some(&adapter)).or_else(|_| make_device(None))?,
        None => make_device(None)?,
    };
    // Capture delivers frames on its own threads and the encoder pumps on another, so the
    // immediate context has to be safe to touch from more than one of them.
    let multithread: ID3D11Multithread = device.cast()?;
    unsafe { let _ = multithread.SetMultithreadProtected(true); };

    let dxgi: IDXGIDevice = device.cast()?;
    let winrt: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?.cast()? };

    // Read back from the device rather than from `adapter`, so the default-adapter path reports
    // the GPU it actually landed on.
    let desc = unsafe { dxgi.GetAdapter()?.GetDesc()? };
    let len = desc.Description.iter().position(|&c| c == 0).unwrap_or(desc.Description.len());

    Ok(Gpu {
        device,
        context,
        winrt,
        luid: desc.AdapterLuid,
        adapter: String::from_utf16_lossy(&desc.Description[..len]),
    })
}

fn make_device(adapter: Option<&IDXGIAdapter>) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    unsafe {
        D3D11CreateDevice(
            adapter,
            // An explicit adapter requires UNKNOWN; HARDWARE means "pick one for me".
            if adapter.is_some() { D3D_DRIVER_TYPE_UNKNOWN } else { D3D_DRIVER_TYPE_HARDWARE },
            HMODULE::default(),
            // BGRA_SUPPORT is required by WGC; VIDEO_SUPPORT is what lets the hardware encoder
            // MFT accept our textures directly later on.
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    Ok((
        device.expect("D3D11CreateDevice returned success with no device"),
        context.expect("D3D11CreateDevice returned success with no context"),
    ))
}

/// The adapter one of whose outputs is `monitor`.
fn adapter_for(monitor: HMONITOR) -> Option<IDXGIAdapter> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        for i in 0.. {
            let adapter: IDXGIAdapter1 = factory.EnumAdapters1(i).ok()?;
            for j in 0.. {
                let Ok(output) = adapter.EnumOutputs(j) else { break };
                if output.GetDesc().is_ok_and(|d| d.Monitor == monitor) {
                    return adapter.cast().ok();
                }
            }
        }
    }
    None
}
