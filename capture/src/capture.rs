//! Windows.Graphics.Capture, pulled at a fixed rate.
//!
//! WGC rather than Desktop Duplication: it survives fullscreen-exclusive games, display mode
//! changes and HDR without re-acquire logic, and on Win11 its capture border can be turned off.
//!
//! WGC delivers frames when the screen *changes*, which is not a video's idea of time. So nothing
//! here waits for a frame: the caller ticks on a clock, and each tick encodes whatever the latest
//! texture is, repeating the previous one when the game is running below target. Constant frame
//! rate keeps the muxer and any later trim in Clipper trivial, and a repeated frame costs almost
//! nothing in the bitstream.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use windows::core::{IInspectable, Interface, Result};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess;
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

use crate::d3d::Gpu;

/// Two buffers is enough for a fixed-rate puller: we copy out and release immediately, so a deeper
/// pool would only add latency between what is on screen and what lands in the buffer.
const POOL_BUFFERS: i32 = 2;

pub struct Capture {
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    item: GraphicsCaptureItem,
    /// Set from WGC's own `Closed` event. A monitor switched off at the wall, or unplugged, takes
    /// its capture item with it and every later `TryGetNextFrame` simply returns nothing —
    /// indistinguishable from a still screen, and so invisible without this. It is the signal the
    /// recorder needs to tear the pipeline down and wait for the display to come back.
    closed: Arc<AtomicBool>,
    closed_token: i64,
    format: DirectXPixelFormat,
    size: SizeInt32,
    /// Our own copy of the most recent frame. Capture buffers have to go back to the pool
    /// promptly, so we never hold one across a tick.
    latest: Option<ID3D11Texture2D>,
    /// True once at least one frame has arrived.
    pub have_frame: bool,
    /// How many times the frame pool has been rebuilt. Should be ~0; anything else means the
    /// content size keeps disagreeing with the pool's, which throws away the latest frame.
    pub recreates: u64,
    pub last_content: (i32, i32),
    /// Frames WGC has actually handed over.
    pub frames_in: u64,
    /// Times TryGetNextFrame had nothing for us.
    pub empty_polls: u64,
}

impl Capture {
    pub fn start(gpu: &Gpu, monitor: HMONITOR, format: DirectXPixelFormat) -> Result<Self> {
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(monitor)? };
        let size = item.Size()?;

        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &gpu.winrt,
            format,
            POOL_BUFFERS,
            size,
        )?;
        let session = pool.CreateCaptureSession(&item)?;

        let closed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&closed);
        let closed_token = item.Closed(&TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(
            move |_, _| {
                flag.store(true, Ordering::Relaxed);
                Ok(())
            },
        ))?;

        // A recording of your own cursor is rarely what you want in a game clip, and the yellow
        // capture border is Win11-only — older builds reject it, which is not an error worth
        // failing a capture over.
        let _ = session.SetIsCursorCaptureEnabled(false);
        let _ = session.SetIsBorderRequired(false);

        session.StartCapture()?;

        Ok(Capture {
            pool,
            session,
            item,
            closed,
            closed_token,
            format,
            size,
            latest: None,
            have_frame: false,
            recreates: 0,
            last_content: (0, 0),
            frames_in: 0,
            empty_polls: 0,
        })
    }

    pub fn size(&self) -> SizeInt32 {
        self.size
    }

    /// True once the display this was capturing has gone away.
    pub fn closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    pub fn set_cursor(&self, enabled: bool) {
        let _ = self.session.SetIsCursorCaptureEnabled(enabled);
    }

    /// Takes whatever WGC has ready, without blocking. Returns true when a genuinely new frame
    /// arrived; false means the screen has not changed and `latest()` still holds the right
    /// picture.
    pub fn pump(&mut self, gpu: &Gpu) -> Result<bool> {
        // A null return is modelled as an error, and it is the normal "nothing changed" case —
        // on a still screen it is what we get every tick.
        let frame = match self.pool.TryGetNextFrame() {
            Ok(frame) => frame,
            Err(_) => {
                self.empty_polls += 1;
                return Ok(false);
            }
        };
        self.frames_in += 1;

        let content_size = frame.ContentSize()?;
        let surface = frame.Surface()?;
        let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
        let source: ID3D11Texture2D = unsafe { access.GetInterface()? };

        if self.latest.is_none() {
            self.latest = Some(self.make_copy_target(gpu, &source)?);
        }
        if let Some(dest) = &self.latest {
            unsafe { gpu.context.CopyResource(dest, &source) };
        }

        frame.Close()?;
        self.have_frame = true;

        // A resolution change (alt-tabbing out of a game that changed the mode) invalidates the
        // pool. Recreate it and drop our copy target so the next frame rebuilds at the new size.
        self.last_content = (content_size.Width, content_size.Height);
        if content_size.Width != self.size.Width || content_size.Height != self.size.Height {
            self.recreates += 1;
            self.size = content_size;
            self.pool
                .Recreate(&gpu.winrt, self.format, POOL_BUFFERS, content_size)?;
            self.latest = None;
            self.have_frame = false;
        }

        Ok(true)
    }

    /// The most recent frame, or None before the first one arrives.
    pub fn latest(&self) -> Option<&ID3D11Texture2D> {
        self.latest.as_ref()
    }

    fn make_copy_target(&self, gpu: &Gpu, source: &ID3D11Texture2D) -> Result<ID3D11Texture2D> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { source.GetDesc(&mut desc) };
        desc.Usage = D3D11_USAGE_DEFAULT;
        // Shader-readable because the tone map / NV12 pack shader samples it in the next
        // milestone; render target so a plain blit stays available.
        desc.BindFlags = (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32;
        desc.CPUAccessFlags = 0;
        desc.MiscFlags = 0;

        let mut texture: Option<ID3D11Texture2D> = None;
        unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut texture))? };
        Ok(texture.unwrap())
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.item.RemoveClosed(self.closed_token);
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}
