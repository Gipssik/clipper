//! Reading frames back to the CPU as PNG.
//!
//! Diagnostics only — the recording pipeline never does this. It exists so the capture path can be
//! checked against real pixels instead of being assumed correct, and it is the slowest thing in
//! this crate: a staging copy plus a map stalls the GPU. Never call it on a tick you are timing.

use windows::core::Result;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Texture2D, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT,
};

use crate::d3d::Gpu;

pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Copies a GPU texture into an RGBA8 buffer.
pub fn read_back(gpu: &Gpu, texture: &ID3D11Texture2D) -> Result<Image> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };

    let mut staging_desc = desc;
    staging_desc.Usage = D3D11_USAGE_STAGING;
    staging_desc.BindFlags = 0;
    staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
    staging_desc.MiscFlags = 0;

    let mut staging: Option<ID3D11Texture2D> = None;
    unsafe { gpu.device.CreateTexture2D(&staging_desc, None, Some(&mut staging))? };
    let staging = staging.unwrap();

    unsafe { gpu.context.CopyResource(&staging, texture) };

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe { gpu.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))? };

    let (w, h) = (desc.Width as usize, desc.Height as usize);
    let mut rgba = vec![0u8; w * h * 4];
    let pitch = mapped.RowPitch as usize;
    let base = mapped.pData as *const u8;

    match desc.Format {
        DXGI_FORMAT_B8G8R8A8_UNORM => unsafe {
            for y in 0..h {
                let row = std::slice::from_raw_parts(base.add(y * pitch), w * 4);
                let out = &mut rgba[y * w * 4..(y + 1) * w * 4];
                for x in 0..w {
                    // BGRA on the wire, RGBA in the file.
                    out[x * 4] = row[x * 4 + 2];
                    out[x * 4 + 1] = row[x * 4 + 1];
                    out[x * 4 + 2] = row[x * 4];
                    out[x * 4 + 3] = 255;
                }
            }
        },
        DXGI_FORMAT_R16G16B16A16_FLOAT => unsafe {
            for y in 0..h {
                let row = std::slice::from_raw_parts(base.add(y * pitch) as *const u16, w * 4);
                let out = &mut rgba[y * w * 4..(y + 1) * w * 4];
                for x in 0..w {
                    for c in 0..3 {
                        // A placeholder, not the real thing: scRGB is clamped to [0,1] and encoded
                        // sRGB just to make the PNG viewable. Anything above 1.0 — which on an HDR
                        // display is most of what makes it HDR — is thrown away here. Milestone 3
                        // replaces this with the mobius tone map on the GPU.
                        let v = half_to_f32(row[x * 4 + c]).clamp(0.0, 1.0);
                        out[x * 4 + c] = (srgb_encode(v) * 255.0 + 0.5) as u8;
                    }
                    out[x * 4 + 3] = 255;
                }
            }
        },
        other => {
            unsafe { gpu.context.Unmap(&staging, 0) };
            return Err(windows::core::Error::new(
                windows::Win32::Foundation::E_NOTIMPL,
                format!("read_back: unhandled texture format {other:?}"),
            ));
        }
    }

    unsafe { gpu.context.Unmap(&staging, 0) };

    Ok(Image {
        width: desc.Width,
        height: desc.Height,
        rgba,
    })
}

/// Reads an NV12 texture back and undoes the colour conversion, so the shader's output can be
/// looked at as a picture. The inverse is the exact transpose of what `convert.hlsl` applies, so a
/// mistake in either direction shows up as a colour cast rather than cancelling out.
pub fn read_back_nv12(gpu: &Gpu, texture: &ID3D11Texture2D) -> Result<Image> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };

    let mut staging_desc = desc;
    staging_desc.Usage = D3D11_USAGE_STAGING;
    staging_desc.BindFlags = 0;
    staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
    staging_desc.MiscFlags = 0;

    let mut staging: Option<ID3D11Texture2D> = None;
    unsafe { gpu.device.CreateTexture2D(&staging_desc, None, Some(&mut staging))? };
    let staging = staging.unwrap();
    unsafe { gpu.context.CopyResource(&staging, texture) };

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe { gpu.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))? };

    let (w, h) = (desc.Width as usize, desc.Height as usize);
    let pitch = mapped.RowPitch as usize;
    let base = mapped.pData as *const u8;
    let mut rgba = vec![0u8; w * h * 4];

    unsafe {
        // NV12 maps as one allocation: the full-resolution luma plane, then the half-resolution
        // interleaved chroma plane immediately after it.
        let luma = std::slice::from_raw_parts(base, pitch * h);
        let chroma = std::slice::from_raw_parts(base.add(pitch * h), pitch * h / 2);

        for y in 0..h {
            for x in 0..w {
                let yv = (luma[y * pitch + x] as f32 - 16.0) / 219.0;
                let ci = (y / 2) * pitch + (x / 2) * 2;
                let cb = (chroma[ci] as f32 - 128.0) / 224.0;
                let cr = (chroma[ci + 1] as f32 - 128.0) / 224.0;

                let r = yv + 1.5748 * cr;
                let g = yv - 0.1873 * cb - 0.4681 * cr;
                let b = yv + 1.8556 * cb;

                let o = (y * w + x) * 4;
                rgba[o] = to_u8(r);
                rgba[o + 1] = to_u8(g);
                rgba[o + 2] = to_u8(b);
                rgba[o + 3] = 255;
            }
        }
    }

    unsafe { gpu.context.Unmap(&staging, 0) };
    Ok(Image { width: desc.Width, height: desc.Height, rgba })
}

fn to_u8(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

pub fn write_png(path: &std::path::Path, image: &Image) -> std::io::Result<()> {
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut encoder = png::Encoder::new(file, image.width, image.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    writer
        .write_image_data(&image.rgba)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(())
}

fn srgb_encode(v: f32) -> f32 {
    if v <= 0.0031308 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

fn half_to_f32(h: u16) -> f32 {
    let sign = ((h >> 15) & 1) as u32;
    let exp = ((h >> 10) & 0x1f) as u32;
    let frac = (h & 0x3ff) as u32;
    let bits = match exp {
        0 if frac == 0 => sign << 31,
        0 => {
            // Subnormal: renormalise into a float32 exponent.
            let mut e = -1i32;
            let mut f = frac;
            while f & 0x400 == 0 {
                f <<= 1;
                e -= 1;
            }
            (sign << 31) | (((127 - 15 + e + 1) as u32) << 23) | ((f & 0x3ff) << 13)
        }
        0x1f => (sign << 31) | (0xff << 23) | (frac << 13),
        _ => (sign << 31) | ((exp + 127 - 15) << 23) | (frac << 13),
    };
    f32::from_bits(bits)
}
