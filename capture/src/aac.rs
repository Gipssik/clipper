//! AAC encoding, with a resampler in front when the endpoint needs one.
//!
//! The AAC encoder accepts 16-bit PCM at 44.1 or 48 kHz only. A Windows endpoint is usually 48 kHz
//! float, which converts trivially, but 88.2 and 96 kHz devices exist and a capture that simply
//! refused to work on one would be a bad surprise. So an unsupported rate inserts Media
//! Foundation's own resampler rather than failing — no rate conversion code of our own.
//!
//! Output is ADTS framed, because that is what MPEG-TS carries. Raw AAC would need the muxer to
//! synthesise the headers itself.

use windows::core::{Result, GUID};
use windows::Win32::Media::MediaFoundation::*;

use crate::encoder::Packet;

const HNS_PER_SECOND: i64 = 10_000_000;

/// What the AAC encoder will take. Anything else goes through the resampler first.
const NATIVE_RATES: [u32; 2] = [44100, 48000];

pub struct AacEncoder {
    resampler: Option<IMFTransform>,
    encoder: IMFTransform,
    in_rate: u32,
    out_rate: u32,
    channels: u32,
    /// Running count of source samples, which is what input timestamps are derived from — the
    /// audio timeline must be continuous even when the endpoint hands us data in ragged chunks.
    frames_in: u64,
    pub resampled: bool,
}

impl AacEncoder {
    pub fn new(sample_rate: u32, channels: u32, bitrate_bytes_per_sec: u32) -> Result<Self> {
        let out_rate = if NATIVE_RATES.contains(&sample_rate) {
            sample_rate
        } else {
            // 48 kHz is the safer landing point: every endpoint that is not 44.1 is a multiple or
            // near-multiple of 48.
            48000
        };

        let resampler = if out_rate == sample_rate {
            None
        } else {
            Some(create_resampler(sample_rate, out_rate, channels)?)
        };

        Ok(AacEncoder {
            resampler,
            encoder: create_aac(out_rate, channels, bitrate_bytes_per_sec)?,
            in_rate: sample_rate,
            out_rate,
            channels,
            frames_in: 0,
            resampled: out_rate != sample_rate,
        })
    }

    pub fn out_rate(&self) -> u32 {
        self.out_rate
    }

    /// Feeds interleaved 16-bit samples. `base_hns` is where the *first* sample this encoder ever
    /// saw sits on the shared clock; everything after is counted from it, so a late or early poll
    /// cannot shift the audio timeline.
    pub fn submit(&mut self, pcm: &[i16], base_hns: i64) -> Result<Vec<Packet>> {
        if pcm.is_empty() {
            return Ok(Vec::new());
        }
        let frames = pcm.len() as u64 / self.channels as u64;
        let pts = base_hns + (self.frames_in as i64) * HNS_PER_SECOND / self.in_rate as i64;
        let duration = frames as i64 * HNS_PER_SECOND / self.in_rate as i64;
        self.frames_in += frames;

        let sample = pcm_sample(pcm, pts, duration)?;

        let mut out = Vec::new();
        match &self.resampler {
            None => {
                unsafe { self.encoder.ProcessInput(0, &sample, 0)? };
                drain(&self.encoder, &mut out)?;
            }
            Some(resampler) => {
                unsafe { resampler.ProcessInput(0, &sample, 0)? };
                let mut resampled = Vec::new();
                drain_samples(resampler, &mut resampled)?;
                for s in resampled {
                    unsafe { self.encoder.ProcessInput(0, &s, 0)? };
                    drain(&self.encoder, &mut out)?;
                }
            }
        }
        Ok(out)
    }

    pub fn finish(&mut self) -> Result<Vec<Packet>> {
        let mut out = Vec::new();
        unsafe {
            if let Some(resampler) = &self.resampler {
                resampler.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
                let mut resampled = Vec::new();
                drain_samples(resampler, &mut resampled)?;
                for s in resampled {
                    self.encoder.ProcessInput(0, &s, 0)?;
                    drain(&self.encoder, &mut out)?;
                }
            }
            self.encoder.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
        }
        drain(&self.encoder, &mut out)?;
        Ok(out)
    }
}

fn create_aac(sample_rate: u32, channels: u32, bytes_per_sec: u32) -> Result<IMFTransform> {
    let transform = first_transform(
        MFT_CATEGORY_AUDIO_ENCODER,
        &MFAudioFormat_PCM,
        &MFAudioFormat_AAC,
    )?;

    unsafe {
        // Output first, as with the video encoder: the codec cannot describe its input until it
        // knows what it is producing.
        let output = MFCreateMediaType()?;
        output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        output.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
        output.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        output.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, sample_rate)?;
        output.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels)?;
        output.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, bytes_per_sec)?;
        // AAC-LC, level 2 — the profile every player and every container accepts.
        output.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)?;
        // 1 = ADTS. MPEG-TS carries ADTS frames; raw AAC would leave the muxer to build headers.
        output.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 1)?;
        transform.SetOutputType(0, &output, 0)?;

        transform.SetInputType(0, &pcm_type(sample_rate, channels)?, 0)?;
        transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
        transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
    }
    Ok(transform)
}

fn create_resampler(from: u32, to: u32, channels: u32) -> Result<IMFTransform> {
    unsafe {
        let transform: IMFTransform = windows::Win32::System::Com::CoCreateInstance(
            &CLSID_AudioResamplerMediaObject,
            None,
            windows::Win32::System::Com::CLSCTX_INPROC_SERVER,
        )?;
        transform.SetInputType(0, &pcm_type(from, channels)?, 0)?;
        transform.SetOutputType(0, &pcm_type(to, channels)?, 0)?;
        transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
        transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        Ok(transform)
    }
}

fn pcm_type(sample_rate: u32, channels: u32) -> Result<IMFMediaType> {
    unsafe {
        let block_align = channels * 2;
        let media_type = MFCreateMediaType()?;
        media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        media_type.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
        media_type.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        media_type.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, sample_rate)?;
        media_type.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels)?;
        media_type.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, block_align)?;
        media_type.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, block_align * sample_rate)?;
        media_type.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
        Ok(media_type)
    }
}

fn pcm_sample(pcm: &[i16], pts: i64, duration: i64) -> Result<IMFSample> {
    unsafe {
        let bytes = std::mem::size_of_val(pcm);
        let buffer = MFCreateMemoryBuffer(bytes as u32)?;
        let mut ptr = std::ptr::null_mut();
        buffer.Lock(&mut ptr, None, None)?;
        std::ptr::copy_nonoverlapping(pcm.as_ptr() as *const u8, ptr, bytes);
        buffer.Unlock()?;
        buffer.SetCurrentLength(bytes as u32)?;

        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(pts)?;
        sample.SetSampleDuration(duration)?;
        Ok(sample)
    }
}

fn first_transform(
    category: GUID,
    input: &GUID,
    output: &GUID,
) -> Result<IMFTransform> {
    let input_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Audio,
        guidSubtype: *input,
    };
    let output_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Audio,
        guidSubtype: *output,
    };

    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(
            category,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input_info),
            Some(&output_info),
            &mut activates,
            &mut count,
        )?;
    }
    if count == 0 {
        return Err(windows::core::Error::new(
            MF_E_TOPO_CODEC_NOT_FOUND,
            "no matching audio transform",
        ));
    }
    unsafe {
        let transform = (*activates).as_ref().unwrap().ActivateObject::<IMFTransform>();
        for i in 0..count as usize {
            let _ = (*activates.add(i)).take();
        }
        windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const _));
        transform
    }
}

/// Pulls finished compressed frames out of a transform.
fn drain(transform: &IMFTransform, out: &mut Vec<Packet>) -> Result<()> {
    let mut samples = Vec::new();
    drain_samples(transform, &mut samples)?;
    for sample in samples {
        let pts_hns = unsafe { sample.GetSampleTime() }.unwrap_or(0);
        let mut data = Vec::new();
        unsafe {
            for i in 0..sample.GetBufferCount()? {
                let buffer = sample.GetBufferByIndex(i)?;
                let mut ptr = std::ptr::null_mut();
                let mut len = 0u32;
                buffer.Lock(&mut ptr, None, Some(&mut len))?;
                data.extend_from_slice(std::slice::from_raw_parts(ptr, len as usize));
                buffer.Unlock()?;
            }
        }
        if !data.is_empty() {
            out.push(Packet {
                data,
                pts_hns,
                // Every AAC frame is independently decodable, so any of them can start a segment.
                keyframe: true,
            });
        }
    }
    Ok(())
}

fn drain_samples(transform: &IMFTransform, out: &mut Vec<IMFSample>) -> Result<()> {
    // These are software MFTs, so they never allocate output samples for us.
    let info = unsafe { transform.GetOutputStreamInfo(0)? };
    loop {
        let sample = unsafe { MFCreateSample()? };
        let buffer = unsafe { MFCreateMemoryBuffer(info.cbSize.max(4096))? };
        unsafe { sample.AddBuffer(&buffer)? };

        let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: std::mem::ManuallyDrop::new(Some(sample)),
            dwStatus: 0,
            pEvents: std::mem::ManuallyDrop::new(None),
        }];
        let mut status = 0u32;

        match unsafe { transform.ProcessOutput(0, &mut buffers, &mut status) } {
            Ok(()) => {
                if let Some(s) = unsafe { std::mem::ManuallyDrop::take(&mut buffers[0].pSample) } {
                    out.push(s);
                }
            }
            Err(e) => {
                let _ = unsafe { std::mem::ManuallyDrop::take(&mut buffers[0].pSample) };
                if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT {
                    return Ok(());
                }
                return Err(e);
            }
        }
    }
}
