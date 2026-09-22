//! The one D3D11 device everything shares.
//!
//! Capture hands us textures on this device, the tone-map shader reads them on this device, and
//! the Media Foundation encoder is given this device through `MFT_MESSAGE_SET_D3D_MANAGER`. One
//! device is what keeps a frame on the GPU from capture to bitstream — the moment two devices are
//! involved, every frame has to go through system memory and the whole lightweight argument
//! collapses.

use windows::core::{Interface, Result};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice;

pub struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    /// The same device seen through WinRT, which is what the capture frame pool takes.
    pub winrt: IDirect3DDevice,
}

pub fn create() -> Result<Gpu> {
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;

    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
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

    let device = device.expect("D3D11CreateDevice returned success with no device");
    let context = context.expect("D3D11CreateDevice returned success with no context");

    // Capture delivers frames on its own threads and the encoder pumps on another, so the
    // immediate context has to be safe to touch from more than one of them.
    let multithread: ID3D11Multithread = device.cast()?;
    unsafe { let _ = multithread.SetMultithreadProtected(true); };

    let dxgi: IDXGIDevice = device.cast()?;
    let winrt: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?.cast()? };

    Ok(Gpu {
        device,
        context,
        winrt,
    })
}
