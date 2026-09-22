//! Hardware H.264 through a Media Foundation transform.
//!
//! MF rather than NVENC directly: one code path picks up whatever encode silicon the machine has —
//! NVIDIA, AMD or Intel — and falls back to software on a GPU with none. The cost is coarser rate
//! control and no lookahead. A direct NVENC backend can go behind this same interface later.
//!
//! The MFT is handed our D3D11 device, so the NV12 texture the converter just drew is encoded
//! where it already lives. Nothing is copied to system memory except the compressed bytes coming
//! back out.
//!
//! Hardware MFTs are *asynchronous*: you may not push a frame whenever you like, you wait to be
//! asked. The transform raises `METransformNeedInput` when it has room and `METransformHaveOutput`
//! when a frame is ready, and the two are not interleaved in any fixed order. Everything below is
//! shaped by that contract.

use windows::core::{Interface, Result, GUID};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Variant::VARIANT;

use crate::d3d::Gpu;

/// `IMFMediaEventGenerator::GetEvent` flags: block until something arrives, or return immediately.
const BLOCKING: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS = MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0);
const NO_WAIT: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS = MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(1);

/// 100 ns units — Media Foundation's clock, and MPEG-TS's 90 kHz divides into it evenly.
const HNS_PER_SECOND: i64 = 10_000_000;

/// MF packs a pair of 32-bit values into one 64-bit attribute; the C headers hide this behind
/// MFSetAttributeSize / MFSetAttributeRatio, which are inline and so have no Rust binding.
fn set_pair(media_type: &IMFMediaType, key: &GUID, high: u32, low: u32) -> Result<()> {
    unsafe { media_type.SetUINT64(key, ((high as u64) << 32) | low as u64) }
}

fn friendly_name(activate: &IMFActivate) -> String {
    let mut ptr = windows::core::PWSTR::null();
    let mut len = 0u32;
    unsafe {
        if activate
            .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut ptr, &mut len)
            .is_err()
        {
            return String::new();
        }
        let name = ptr.to_string().unwrap_or_default();
        windows::Win32::System::Com::CoTaskMemFree(Some(ptr.0 as *const _));
        name
    }
}

pub struct Packet {
    pub data: Vec<u8>,
    pub pts_hns: i64,
    pub keyframe: bool,
}

pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// What the stream should average over time.
    pub bitrate: u32,
    /// What a hard scene is allowed to spike to. Equal to `bitrate` asks for CBR.
    pub max_bitrate: u32,
    /// Seconds between IDRs. This is the ring buffer's trim accuracy, so it is not a free knob.
    pub gop_seconds: f32,
    /// 0 is "as fast as possible", 100 is "as good as possible". On the NVIDIA MFT this selects
    /// the NVENC preset, which is the single largest quality lever available here and costs
    /// encode-chip cycles rather than the shaders the game is using.
    pub quality_vs_speed: u32,
}

pub struct Encoder {
    transform: IMFTransform,
    events: Option<IMFMediaEventGenerator>,
    provides_samples: bool,
    frame_duration_hns: i64,
    pending: Vec<Packet>,
    pub name: String,
    pub hardware: bool,
    /// Candidates that failed to configure before the one we kept.
    pub rejected: u32,
    /// Frames the encoder would not accept in time. Never expected to move.
    pub dropped: u64,
    pub need_input_events: u64,
    pub have_output_events: u64,
    pub inputs: u64,
    pub null_outputs: u64,
    /// What the codec actually accepted, for the log. Drivers differ in which knobs they honour
    /// and a silently ignored setting is exactly the kind of thing that costs an evening.
    pub applied: Vec<String>,
}

impl Encoder {
    pub fn new(gpu: &Gpu, config: &EncoderConfig) -> Result<Self> {
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)? };

        // Enumeration only proves an MFT is registered, not that this machine can drive it — the
        // same lesson `detectEncoders()` learned on the Electron side. So we try candidates in
        // order and keep the first that survives being configured, ending at software.
        let mut last_error = None;
        let mut rejected = 0u32;
        for hardware in [true, false] {
            for activate in enumerate(hardware)? {
                match Self::try_activate(gpu, config, &activate, hardware) {
                    Ok(mut encoder) => {
                        encoder.rejected = rejected;
                        return Ok(encoder);
                    }
                    Err(e) => {
                        // Releasing the IMFTransform is not enough: until the activate is shut
                        // down the vendor may hold the encode session open, and consumer drivers
                        // allow only a handful. A candidate we rejected must not cost the one we
                        // accept its ability to run.
                        unsafe { let _ = activate.ShutdownObject(); }
                        rejected += 1;
                        last_error = Some(e);
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| {
            windows::core::Error::new(MF_E_TOPO_CODEC_NOT_FOUND, "no H.264 encoder available")
        }))
    }

    fn try_activate(
        gpu: &Gpu,
        config: &EncoderConfig,
        activate: &IMFActivate,
        hardware: bool,
    ) -> Result<Self> {
        let name = friendly_name(activate);
        let transform: IMFTransform = unsafe { activate.ActivateObject() }?;

        let attributes = unsafe { transform.GetAttributes() }.ok();
        let is_async = attributes
            .as_ref()
            .and_then(|a| unsafe { a.GetUINT32(&MF_TRANSFORM_ASYNC) }.ok())
            .unwrap_or(0)
            == 1;
        if is_async {
            // Until unlocked, an async MFT refuses every call with MF_E_TRANSFORM_ASYNC_LOCKED.
            if let Some(a) = &attributes {
                unsafe { a.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)? };
            }
        }

        if hardware {
            // Give the MFT our device so it encodes the texture in place. Without this it would
            // demand system-memory buffers and we would be copying every frame back and forth.
            let mut token = 0u32;
            let mut manager: Option<IMFDXGIDeviceManager> = None;
            unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager)? };
            let manager = manager.unwrap();
            unsafe { manager.ResetDevice(&gpu.device, token)? };
            unsafe {
                transform.ProcessMessage(
                    MFT_MESSAGE_SET_D3D_MANAGER,
                    manager.as_raw() as *const std::ffi::c_void as usize,
                )?
            };
        }

        // Rate control, preset and GOP go on *before* the output type.
        //
        // This is not a style choice and it cost a measurement to find. Set after `SetOutputType`,
        // `CODECAPI_AVEncMPVGOPSize` returns success on the NVIDIA MFT and is then ignored: 20
        // keyframes in 20 seconds with the property set to 120 frames. Set before it, the same
        // call produces exactly the 120-frame spacing it asked for. They are applied again below,
        // because other vendors' transforms reset their codec state when the type changes, and
        // applying them twice costs nothing.
        configure_codec(&transform, config);

        // Output type first: an encoder cannot decide what input it accepts until it knows what it
        // is producing.
        let output = unsafe { MFCreateMediaType()? };
        unsafe {
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            output.SetUINT32(&MF_MT_AVG_BITRATE, config.bitrate)?;
            set_pair(&output, &MF_MT_FRAME_SIZE, config.width, config.height)?;
            set_pair(&output, &MF_MT_FRAME_RATE, config.fps, 1)?;
            set_pair(&output, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
            output.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            output.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
            // Keyframe spacing has to go on the media type, not through ICodecAPI. The NVIDIA MFT
            // accepts `AVEncMPVGOPSize` — `SetValue` returns success — and then ignores it,
            // pinning IDRs at one a second regardless. Measured: 20 keyframes in 20 seconds with
            // the codec property set to 120 frames. This attribute is the one it honours.
            output.SetUINT32(
                &MF_MT_MAX_KEYFRAME_SPACING,
                (config.gop_seconds * config.fps as f32).round() as u32,
            )?;
            // Colour description. The shader has already converted everything to limited-range
            // BT.709, and saying so is not optional: an untagged stream is only *assumed* bt709 by
            // convention, and the one case where that assumption is wrong is the case this whole
            // pipeline exists for — a clip taken off an HDR display, where a player that guesses
            // from the source rather than the stream will still try to treat it as HDR.
            output.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
            output.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
            output.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
            output.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
            transform.SetOutputType(0, &output, 0)?;
        }

        let input = unsafe { MFCreateMediaType()? };
        unsafe {
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            set_pair(&input, &MF_MT_FRAME_SIZE, config.width, config.height)?;
            set_pair(&input, &MF_MT_FRAME_RATE, config.fps, 1)?;
            set_pair(&input, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            input.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
            input.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
            input.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
            input.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
            transform.SetInputType(0, &input, 0)?;
        }

        // Second pass. Whichever of the two the driver actually reads, this is the one whose
        // return value is reported, so the log reflects a call that certainly happened.
        let applied = configure_codec(&transform, config);

        let provides_samples = unsafe { transform.GetOutputStreamInfo(0) }
            .map(|info| {
                info.dwFlags
                    & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0
                        | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32
                    != 0
            })
            .unwrap_or(false);

        unsafe {
            transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }

        let events = if is_async {
            Some(transform.cast::<IMFMediaEventGenerator>()?)
        } else {
            None
        };

        Ok(Encoder {
            transform,
            events,
            provides_samples,
            frame_duration_hns: HNS_PER_SECOND / config.fps.max(1) as i64,
            pending: Vec::new(),
            name,
            hardware,
            rejected: 0,
            dropped: 0,
            need_input_events: 0,
            have_output_events: 0,
            inputs: 0,
            null_outputs: 0,
            applied,
        })
    }

    /// Hands one NV12 texture to the encoder, stamped with the tick it belongs to.
    ///
    /// The texture must not be rewritten until the encoder is done with it, which is why the
    /// converter cycles through a pool rather than reusing one surface.
    /// Returns false when the encoder would not take the frame in time. The frame is dropped
    /// rather than waited on: a recorder that blocks forever on a wedged encoder still reports
    /// itself as recording, which is a far worse failure than a missing frame.
    pub fn submit(&mut self, texture: &ID3D11Texture2D, pts_hns: i64) -> Result<bool> {
        let sample = unsafe {
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false)?;
            // A DXGI buffer starts with a length of zero; an MFT that believes it was handed an
            // empty frame will quietly encode nothing.
            let length = buffer.cast::<IMF2DBuffer>()?.GetContiguousLength()?;
            buffer.SetCurrentLength(length)?;

            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(pts_hns)?;
            sample.SetSampleDuration(self.frame_duration_hns)?;
            sample
        };

        if self.events.is_some() {
            // Async: wait to be asked. Outputs that arrive while we wait get collected on the way.
            if !self.pump_until_need_input()? {
                self.dropped += 1;
                return Ok(false);
            }
            unsafe { self.transform.ProcessInput(0, &sample, 0)? };
            self.inputs += 1;
        } else {
            unsafe { self.transform.ProcessInput(0, &sample, 0)? };
            self.drain_sync()?;
        }
        Ok(true)
    }

    /// Everything the encoder has finished since the last call.
    pub fn take(&mut self) -> Vec<Packet> {
        if self.events.is_some() {
            let _ = self.pump(false);
        }
        std::mem::take(&mut self.pending)
    }

    /// Flushes the encoder's internal queue. Call once at the end of a stream.
    pub fn finish(&mut self) -> Result<Vec<Packet>> {
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)?;
            self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
        }
        if self.events.is_some() {
            self.pump(true)?;
        } else {
            self.drain_sync()?;
        }
        Ok(std::mem::take(&mut self.pending))
    }

    /// Waits, briefly, to be asked for input.
    ///
    /// This used to block on `GetEvent` with no deadline, which is correct right up until the
    /// transform stops raising events — and then the whole recorder wedges silently, still
    /// reporting itself as running. A bounded poll turns that into a dropped frame and a counter
    /// somebody can see. The budget is several frame intervals, so it costs nothing when healthy.
    fn pump_until_need_input(&mut self) -> Result<bool> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
        let events = self.events.clone().unwrap();
        loop {
            match unsafe { events.GetEvent(NO_WAIT) } {
                Ok(event) => match unsafe { event.GetType()? } as i32 {
                    x if x == METransformNeedInput.0 => {
                        self.need_input_events += 1;
                        return Ok(true);
                    }
                    x if x == METransformHaveOutput.0 => {
                        self.have_output_events += 1;
                        self.collect_output()?
                    }
                    _ => {}
                },
                Err(_) => {
                    if std::time::Instant::now() >= deadline {
                        return Ok(false);
                    }
                    std::thread::sleep(std::time::Duration::from_micros(200));
                }
            }
        }
    }

    /// Drains events without blocking, or blocks through to end-of-stream when `to_end`.
    fn pump(&mut self, to_end: bool) -> Result<()> {
        let events = self.events.clone().unwrap();
        loop {
            let flags = if to_end { BLOCKING } else { NO_WAIT };
            let event = match unsafe { events.GetEvent(flags) } {
                Ok(e) => e,
                Err(_) => return Ok(()), // nothing queued
            };
            match unsafe { event.GetType()? } as i32 {
                x if x == METransformHaveOutput.0 => {
                    self.have_output_events += 1;
                    self.collect_output()?
                }
                x if x == METransformDrainComplete.0 => return Ok(()),
                _ => {}
            }
        }
    }

    fn drain_sync(&mut self) -> Result<()> {
        loop {
            match self.collect_output() {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    }

    fn collect_output(&mut self) -> Result<()> {
        let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: if self.provides_samples {
                // A hardware MFT allocates its own output samples; handing it one of ours is an
                // error on some drivers and ignored on others.
                std::mem::ManuallyDrop::new(None)
            } else {
                std::mem::ManuallyDrop::new(Some(unsafe { MFCreateSample()? }))
            },
            dwStatus: 0,
            pEvents: std::mem::ManuallyDrop::new(None),
        }];

        let mut status = 0u32;
        unsafe { self.transform.ProcessOutput(0, &mut buffers, &mut status)? };

        let sample: Option<IMFSample> =
            unsafe { std::mem::ManuallyDrop::take(&mut buffers[0].pSample) };
        let Some(sample) = sample else {
            self.null_outputs += 1;
            return Ok(());
        };

        let pts_hns = unsafe { sample.GetSampleTime() }.unwrap_or(0);
        // Every keyframe is marked clean; the muxer needs this to know where a segment may start.
        let keyframe = unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }.unwrap_or(0) == 1;

        let mut data = Vec::new();
        unsafe {
            let count = sample.GetBufferCount()?;
            for i in 0..count {
                let buffer = sample.GetBufferByIndex(i)?;
                let mut ptr = std::ptr::null_mut();
                let mut len = 0u32;
                buffer.Lock(&mut ptr, None, Some(&mut len))?;
                data.extend_from_slice(std::slice::from_raw_parts(ptr, len as usize));
                buffer.Unlock()?;
            }
        }

        self.pending.push(Packet {
            data,
            pts_hns,
            keyframe,
        });
        Ok(())
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            let _ = MFShutdown();
        }
    }
}

fn enumerate(hardware: bool) -> Result<Vec<IMFActivate>> {
    let output_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let flags = if hardware {
        MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER
    } else {
        MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER
    };

    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            None,
            Some(&output_info),
            &mut activates,
            &mut count,
        )?;
    }

    let mut result = Vec::with_capacity(count as usize);
    unsafe {
        for i in 0..count as usize {
            if let Some(activate) = (*activates.add(i)).take() {
                result.push(activate);
            }
        }
        windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const _));
    }
    Ok(result)
}

/// Rate control and GOP structure — where most of the picture quality actually lives.
///
/// Every one of these is best-effort: drivers differ in which they honour, and a rejected setting
/// is not a reason to give up on an otherwise working encoder. What is *not* optional is knowing
/// which ones took, so each is reported back and ends up in the log.
///
/// Three of these were measured to matter far more than the bitrate number:
///
/// * **Not low-latency.** `AVLowLatencyMode` picks NVENC's low-latency preset, which exists for
///   streaming, where a frame that arrives late is worse than a frame that looks bad. We write to
///   a ring buffer on disk; nothing is waiting on the output. Turning it off is free quality.
/// * **Quality over speed.** On the NVIDIA MFT this selects the preset, and it is the most
///   expensive setting in the whole pipeline — the top band costs twice the encode engine of the
///   middle one for a quarter of a dB. `config::QUALITY_VS_SPEED` carries the numbers.
/// * **Peak-constrained VBR, not CBR.** CBR here clamps but does not pad — measured at 1.70 Mbps
///   against a 2 Mbps target and 7.03 Mbps against 50 — so it only ever costs quality on the hard
///   scenes and never buys back anything on the easy ones. VBR lets a firefight spend what it
///   needs while a menu screen still costs nothing. Eviction is by duration, so this cannot make
///   the ring outgrow its window; it moves bytes to where they show.
fn configure_codec(transform: &IMFTransform, config: &EncoderConfig) -> Vec<String> {
    let mut applied = Vec::new();
    let Ok(codec) = transform.cast::<ICodecAPI>() else {
        return applied;
    };
    let mut set = |label: &str, guid: &GUID, value: VARIANT| -> bool {
        let ok = unsafe { codec.SetValue(guid, &value) }.is_ok();
        if ok {
            applied.push(label.to_string());
        }
        ok
    };

    // Rate control mode has to land before the bitrates, or the numbers are interpreted against
    // whatever mode the MFT defaulted to.
    let vbr = config.max_bitrate > config.bitrate
        && set(
            "vbr",
            &CODECAPI_AVEncCommonRateControlMode,
            VARIANT::from(eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32),
        );
    if !vbr {
        set(
            "cbr",
            &CODECAPI_AVEncCommonRateControlMode,
            VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32),
        );
    }

    set(
        "mean-bitrate",
        &CODECAPI_AVEncCommonMeanBitRate,
        VARIANT::from(config.bitrate),
    );
    if vbr {
        set(
            "max-bitrate",
            &CODECAPI_AVEncCommonMaxBitRate,
            VARIANT::from(config.max_bitrate),
        );
    }

    // `CLIPPER_QVS` overrides the preset for one run. This is the one number here whose cost is
    // entirely a property of somebody else's silicon, so being able to re-measure the curve on a
    // different card without a rebuild is worth the four lines.
    let qvs: u32 = std::env::var("CLIPPER_QVS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(config.quality_vs_speed);
    set(
        "quality-vs-speed",
        &CODECAPI_AVEncCommonQualityVsSpeed,
        VARIANT::from(qvs.min(100)),
    );
    // Off, deliberately. See the note above: this is a streaming knob and we are not streaming.
    // Measured as free either way on this MFT — 22.5% of the encode engine against 22.6% — so it
    // is off for what it means rather than for what it costs.
    set("no-low-latency", &CODECAPI_AVLowLatencyMode, VARIANT::from(false));

    // Some MFTs will quietly drop resolution or frame rate when they think they are behind. The
    // muxer and the ring both assume constant frame rate at a fixed size, so that has to be off.
    set(
        "no-adaptive",
        &CODECAPI_AVEncAdaptiveMode,
        VARIANT::from(eAVEncAdaptiveMode_None.0 as u32),
    );

    // IDR spacing is the trim accuracy at the head of a saved clip.
    set(
        "gop",
        &CODECAPI_AVEncMPVGOPSize,
        VARIANT::from((config.gop_seconds * config.fps as f32).round() as u32),
    );
    // No B-frames: keeps DTS equal to PTS, which keeps the muxer simple and latency low, for about
    // 5% bitrate efficiency.
    set(
        "no-b-frames",
        &CODECAPI_AVEncMPVDefaultBPictureCount,
        VARIANT::from(0u32),
    );

    applied
}
