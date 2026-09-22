//! Scale, tone map and NV12 pack — one GPU pass over the picture, no CPU copy.
//!
//! This is the only place in the pipeline that touches pixels, which is deliberate: an export that
//! needs a downscale *and* a tone map *and* a colour conversion does all three here, so nothing is
//! ever done twice. Same rule as `runEncode()` on the Electron side.
//!
//! **Why two draws rather than one compute dispatch.** NV12 keeps luma at full resolution and
//! chroma at half, so they are different-sized render targets. D3D11 will hand out a render target
//! view onto an individual plane of an NV12 texture if you ask for it by plane format — R8 for
//! luma, R8G8 for chroma — which lets the encoder's own texture be written in place. A compute
//! shader cannot: D3D11 has no plane slice for UAVs, so it would need a scratch texture and a copy.
//!
//! **Why the downscale is a real filter.** The first version leaned on the sampler: read the
//! source through a linear sampler at the destination's resolution and the scaling comes for free
//! in the fetch the tone map needs anyway. That is a true box filter at exactly 2x and under-
//! filters at every other ratio — and even where it is right, a box is the softest useful filter
//! there is. It is most of why a 720p capture off a 1440p panel looked mushy beside one taken by
//! hardware that resamples properly. It is now a Catmull-Rom kernel taken as four bilinear taps
//! per axis, evaluated at every source texel inside the kernel's support. Measured cost: half a
//! percentage point of the 3D engine, the one the game is actually using — 1.13% against 0.63% at
//! 720p from 1440p. It is skipped entirely when there is no scaling to do.

use windows::core::{Interface, Result, PCSTR};
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Buffer, ID3D11PixelShader, ID3D11RenderTargetView, ID3D11SamplerState,
    ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader, D3D11_BIND_CONSTANT_BUFFER,
    D3D11_BIND_RENDER_TARGET, D3D11_BUFFER_DESC, D3D11_COMPARISON_NEVER, D3D11_CPU_ACCESS_WRITE,
    D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_FORMAT_SUPPORT_RENDER_TARGET, D3D11_MAP_WRITE_DISCARD,
    D3D11_RENDER_TARGET_VIEW_DESC, D3D11_RENDER_TARGET_VIEW_DESC_0, D3D11_RTV_DIMENSION_TEXTURE2D,
    D3D11_SAMPLER_DESC, D3D11_TEX2D_RTV, D3D11_TEXTURE2D_DESC, D3D11_TEXTURE_ADDRESS_CLAMP,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_DYNAMIC, D3D11_VIEWPORT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_NV12, DXGI_FORMAT_R8G8_UNORM, DXGI_FORMAT_R8_UNORM, DXGI_SAMPLE_DESC,
};

use crate::d3d::Gpu;

const SHADER: &str = include_str!("convert.hlsl");

/// The encoder reads a surface asynchronously and gives no signal when it is finished with one, so
/// writing the next frame into the same texture can corrupt the one being encoded. Cycling through
/// a few surfaces costs a handful of megabytes and removes the hazard entirely.
const SURFACES: usize = 4;

#[repr(C)]
#[derive(Clone, Copy)]
struct Params {
    dst_size: [f32; 2],
    src_size: [f32; 2],
    white_scale: f32,
    peak: f32,
    hdr: u32,
    resampling: u32,
}

pub struct Converter {
    vs: ID3D11VertexShader,
    ps_luma: ID3D11PixelShader,
    ps_chroma: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    constants: ID3D11Buffer,
    surfaces: Vec<Surface>,
    next: usize,
    srv: Option<(ID3D11ShaderResourceView, isize)>,
    width: u32,
    height: u32,
    pub resampling: bool,
}

struct Surface {
    nv12: ID3D11Texture2D,
    rtv_luma: ID3D11RenderTargetView,
    rtv_chroma: ID3D11RenderTargetView,
}

impl Converter {
    /// `hdr` says the source is linear scRGB needing the tone map; false means it is already
    /// sRGB-encoded and only needs the colour conversion.
    pub fn new(
        gpu: &Gpu,
        src_width: u32,
        src_height: u32,
        dst_width: u32,
        dst_height: u32,
        hdr: bool,
        white_scale: f32,
        peak: f32,
    ) -> Result<Self> {
        // NV12 chroma is half resolution in both directions, so odd dimensions have no
        // representation. Round down rather than up: never invent a row that was not captured.
        let width = dst_width & !1;
        let height = dst_height & !1;

        let vs_blob = compile(SHADER, "VSMain", "vs_5_0")?;
        let luma_blob = compile(SHADER, "PSLuma", "ps_5_0")?;
        let chroma_blob = compile(SHADER, "PSChroma", "ps_5_0")?;

        let mut vs = None;
        let mut ps_luma = None;
        let mut ps_chroma = None;
        unsafe {
            gpu.device
                .CreateVertexShader(blob_bytes(&vs_blob), None, Some(&mut vs))?;
            gpu.device
                .CreatePixelShader(blob_bytes(&luma_blob), None, Some(&mut ps_luma))?;
            gpu.device
                .CreatePixelShader(blob_bytes(&chroma_blob), None, Some(&mut ps_chroma))?;
        }

        let mut surfaces = Vec::with_capacity(SURFACES);
        for _ in 0..SURFACES {
            let nv12 = create_nv12(gpu, width, height)?;
            surfaces.push(Surface {
                rtv_luma: plane_rtv(gpu, &nv12, DXGI_FORMAT_R8_UNORM)?,
                rtv_chroma: plane_rtv(gpu, &nv12, DXGI_FORMAT_R8G8_UNORM)?,
                nv12,
            });
        }

        let mut sampler = None;
        unsafe {
            gpu.device.CreateSamplerState(
                &D3D11_SAMPLER_DESC {
                    Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                    AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                    ComparisonFunc: D3D11_COMPARISON_NEVER,
                    MaxLOD: f32::MAX,
                    ..Default::default()
                },
                Some(&mut sampler),
            )?;
        }

        // A few percent of slack: at 1:1 the kernel would only be a mild sharpen of pixels that
        // are already exactly right, and sharpening something nobody scaled is not our business.
        let resampling = src_width as f32 > width as f32 * 1.05
            || src_height as f32 > height as f32 * 1.05;

        let params = Params {
            dst_size: [width as f32, height as f32],
            src_size: [src_width as f32, src_height as f32],
            white_scale: if hdr { white_scale.max(1.0) } else { 1.0 },
            peak: peak.max(1.0),
            hdr: hdr as u32,
            resampling: resampling as u32,
        };
        let mut constants = None;
        unsafe {
            gpu.device.CreateBuffer(
                &D3D11_BUFFER_DESC {
                    ByteWidth: std::mem::size_of::<Params>() as u32,
                    Usage: D3D11_USAGE_DYNAMIC,
                    BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                    CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                    ..Default::default()
                },
                None,
                Some(&mut constants),
            )?;
        }
        let constants: ID3D11Buffer = constants.unwrap();
        write_constants(gpu, &constants, &params)?;

        Ok(Converter {
            vs: vs.unwrap(),
            ps_luma: ps_luma.unwrap(),
            ps_chroma: ps_chroma.unwrap(),
            sampler: sampler.unwrap(),
            constants,
            surfaces,
            next: 0,
            srv: None,
            width,
            height,
            resampling,
        })
    }

    /// The surface the last `convert()` wrote. Valid until `SURFACES` more conversions have run.
    pub fn nv12(&self) -> &ID3D11Texture2D {
        let last = (self.next + SURFACES - 1) % SURFACES;
        &self.surfaces[last].nv12
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn convert(&mut self, gpu: &Gpu, source: &ID3D11Texture2D) -> Result<()> {
        let srv = self.source_view(gpu, source)?;
        let ctx = &gpu.context;
        let surface = &self.surfaces[self.next];
        self.next = (self.next + 1) % SURFACES;

        unsafe {
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetShaderResources(0, Some(&[Some(srv)]));
            ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            ctx.PSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));

            // Luma at full size, then chroma at half. Both read the same source and run the same
            // tone map; the chroma pass just averages four luma-grid samples per output texel.
            for (rtv, ps, w, h) in [
                (&surface.rtv_luma, &self.ps_luma, self.width, self.height),
                (
                    &surface.rtv_chroma,
                    &self.ps_chroma,
                    self.width / 2,
                    self.height / 2,
                ),
            ] {
                ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
                ctx.RSSetViewports(Some(&[D3D11_VIEWPORT {
                    TopLeftX: 0.0,
                    TopLeftY: 0.0,
                    Width: w as f32,
                    Height: h as f32,
                    MinDepth: 0.0,
                    MaxDepth: 1.0,
                }]));
                ctx.PSSetShader(ps, None);
                ctx.Draw(3, 0);
            }

            // Leave nothing bound: the encoder is about to be handed this same texture, and a
            // still-bound render target view would stop it being read.
            ctx.OMSetRenderTargets(None, None);
            ctx.PSSetShaderResources(0, Some(&[None]));
        }
        Ok(())
    }

    /// Views are cached against the source texture's identity — the capture only makes a new one
    /// when the display resolution changes, so this rebuilds roughly never.
    fn source_view(&mut self, gpu: &Gpu, source: &ID3D11Texture2D) -> Result<ID3D11ShaderResourceView> {
        let key = source.as_raw() as isize;
        if let Some((srv, cached)) = &self.srv {
            if *cached == key {
                return Ok(srv.clone());
            }
        }
        let mut srv = None;
        unsafe { gpu.device.CreateShaderResourceView(source, None, Some(&mut srv))? };
        let srv: ID3D11ShaderResourceView = srv.unwrap();
        self.srv = Some((srv.clone(), key));
        Ok(srv)
    }
}

/// Whether this GPU will let us render into NV12 planes at all. Checked up front so a driver that
/// says no produces a clear message instead of a confusing failure three calls later.
pub fn supports_nv12_render_target(gpu: &Gpu) -> bool {
    unsafe {
        gpu.device
            .CheckFormatSupport(DXGI_FORMAT_NV12)
            .map(|flags| flags & D3D11_FORMAT_SUPPORT_RENDER_TARGET.0 as u32 != 0)
            .unwrap_or(false)
    }
}

fn create_nv12(gpu: &Gpu, width: u32, height: u32) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        ..Default::default()
    };
    let mut texture = None;
    unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut texture))? };
    Ok(texture.unwrap())
}

/// D3D11 selects the plane from the view's format: R8 is luma, R8G8 is chroma.
fn plane_rtv(
    gpu: &Gpu,
    texture: &ID3D11Texture2D,
    format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
) -> Result<ID3D11RenderTargetView> {
    let desc = D3D11_RENDER_TARGET_VIEW_DESC {
        Format: format,
        ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_RTV { MipSlice: 0 },
        },
    };
    let mut rtv = None;
    unsafe { gpu.device.CreateRenderTargetView(texture, Some(&desc), Some(&mut rtv))? };
    Ok(rtv.unwrap())
}

fn write_constants(gpu: &Gpu, buffer: &ID3D11Buffer, params: &Params) -> Result<()> {
    unsafe {
        let mut mapped = Default::default();
        gpu.context
            .Map(buffer, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))?;
        std::ptr::copy_nonoverlapping(params, mapped.pData as *mut Params, 1);
        gpu.context.Unmap(buffer, 0);
    }
    Ok(())
}

fn compile(source: &str, entry: &str, target: &str) -> Result<ID3DBlob> {
    let entry = std::ffi::CString::new(entry).unwrap();
    let target = std::ffi::CString::new(target).unwrap();
    let mut code = None;
    let mut errors = None;

    let result = unsafe {
        D3DCompile(
            source.as_ptr() as *const _,
            source.len(),
            PCSTR(b"convert.hlsl\0".as_ptr()),
            None,
            None,
            PCSTR(entry.as_ptr() as *const u8),
            PCSTR(target.as_ptr() as *const u8),
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            Some(&mut errors),
        )
    };

    if let Err(e) = result {
        let detail = errors
            .map(|blob| unsafe {
                let bytes =
                    std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize());
                String::from_utf8_lossy(bytes).trim().to_string()
            })
            .unwrap_or_default();
        return Err(windows::core::Error::new(e.code(), detail));
    }
    Ok(code.unwrap())
}

fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    unsafe { std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize()) }
}
