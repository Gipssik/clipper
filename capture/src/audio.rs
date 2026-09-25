//! Audio: WASAPI in, one stereo track out.
//!
//! Two sources, one track. `Endpoint` is a single WASAPI stream — the render endpoint read in
//! loopback, or a capture endpoint, which is to say a microphone. `Mixer` runs both and hands the
//! encoder one contiguous stereo stream.
//!
//! **One track rather than two.** MPEG-TS and MP4 would both carry a second audio stream happily,
//! and it would be the better answer for editing. It is the worse answer for everything this tool
//! is for: a clip goes to Discord or a group chat, and most of what plays it there plays the first
//! track and silently ignores the rest — so "separate tracks" would mean a lot of people posting
//! clips with no voice in them and no way to tell. Mixed is what you can share.
//!
//! **The silence problem.** Loopback delivers packets only while the audio engine is running, and
//! the engine stops when nothing is playing. A naive capture therefore produces no samples during
//! quiet passages, and since the muxer lays samples down consecutively, every silent gap makes the
//! audio track shorter than the video — the two drift apart by exactly the length of the silence.
//! Two defences, both needed:
//!
//! 1. A render stream on the same endpoint, writing silence. The engine then never stops, so
//!    loopback keeps delivering. This is the standard fix and it costs nothing audible.
//! 2. Gap detection from the device's own QPC timestamps. If a real gap appears anyway — the
//!    engine restarting, a device that overran and says so — we insert exactly enough silence to
//!    cover it. Only a real one: timestamps that merely jitter, or a device on a clock of its own,
//!    are a different problem with a different answer, and `Pacer` is where the difference is made.
//!
//! **Timestamps come from the device, not from us.** `GetBuffer` hands back the QPC position the
//! first sample of each packet was captured at, in the same 100 ns units the video leg uses. That
//! shared clock is the whole basis of A/V sync; nothing here infers time from arrival order.

use std::collections::VecDeque;

use windows::core::{Result, HSTRING};
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, EDataFlow, IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDevice,
    IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, DEVICE_STATE_ACTIVE,
    WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL, STGM_READ};

/// From mmreg.h. Not re-exported by the `windows` crate's Audio module.
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// How much the endpoint buffers. 100 ms is generous — we poll far more often — and it means a
/// scheduling hiccup cannot lose samples.
const BUFFER_HNS: i64 = 1_000_000;

pub(crate) const HNS_PER_SECOND: i64 = 10_000_000;

/// Everything downstream works in this format: the AAC encoder wants 16-bit PCM, and stereo is
/// what a game clip needs.
pub const OUT_CHANNELS: usize = 2;

/// How quickly a rate-matched source is pulled back onto the timeline, as the time constant of a
/// first-order loop. One second: half a millisecond out of step asks for a 0.05% rate change, which
/// is about one cent of pitch and inaudible on anything.
const RATE_MATCH_TAU_HNS: f64 = HNS_PER_SECOND as f64;

/// The most a rate-matched source may be sped up or slowed down. A device further out than this is
/// not drifting, it is broken, and the dropout path below deals with it instead.
const RATE_MATCH_MAX: f64 = 0.01;

/// Past this much, a hole is a hole rather than a clock difference, and silence is the honest thing
/// to put in it.
const DROPOUT_HNS: i64 = HNS_PER_SECOND / 20;

/// How far the desktop leg may sit from its own timestamps before anything is done about it.
///
/// Zero for a microphone, whose clock is always somewhere else and always being pulled back. Ten
/// milliseconds for loopback, because a healthy render endpoint never gets near it — measured here
/// at ±0.02 ms of packet jitter and 1 ppm of drift — and inside it the samples go through bit for
/// bit, exactly as they did before any of this existed. It is also far inside what anybody can see
/// as lip sync, so a jittery device can wander around in it without costing anything.
pub(crate) const LOOPBACK_DEADBAND_HNS: i64 = HNS_PER_SECOND / 100;

#[derive(Clone, Copy, serde::Serialize)]
pub struct MixFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits: u16,
    pub float: bool,
}

#[derive(Default, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PollStats {
    /// Samples taken from the endpoint.
    pub captured_frames: u64,
    /// Samples we invented to cover a gap the endpoint left.
    pub filled_frames: u64,
    /// Packets the endpoint flagged as silent, which it may hand over without real data.
    pub silent_packets: u64,
    pub discontinuities: u64,
}

impl PollStats {
    pub fn add(&mut self, other: &PollStats) {
        self.captured_frames += other.captured_frames;
        self.filled_frames += other.filled_frames;
        self.silent_packets += other.silent_packets;
        self.discontinuities += other.discontinuities;
    }
}

pub struct Endpoint {
    _capture_client: IAudioClient,
    capture: IAudioCaptureClient,
    _render: Option<Silence>,
    format: MixFormat,
    /// Per-source-channel gain into left and right, derived from the endpoint's channel mask.
    downmix: Vec<(f32, f32)>,
    /// QPC just past the last sample of the most recent packet — the end of the device's timeline,
    /// so it can be compared against a sample count without being one packet short.
    pub last_packet_hns: i64,
    pub first_hns: Option<i64>,
    /// Constant correction added to every device timestamp. See `anchor` below: normally zero.
    pub skew_hns: i64,
    /// What keeps the samples on the capture clock. See `Pacer`.
    pacer: Pacer,
    /// One packet, folded to stereo, on its way into the pacer. Kept to save an allocation a packet.
    pairs: Vec<[f32; 2]>,
    /// Frames the device has handed over, for measuring its clock against ours.
    pub captured: u64,
}

impl Endpoint {
    /// Whatever Windows currently calls the default output, read in loopback.
    ///
    /// `tone_hz` makes the keep-alive render stream emit a sine instead of silence, which is how
    /// the capture path is tested without needing anything else to be playing.
    ///
    /// `rate` is the mix rate when one is already running, which is the case when the default has
    /// moved and the desktop leg is reopening on a different device. A device that runs at that
    /// rate is read exactly as the first one was. One that does not is handed to the audio
    /// engine's converter, the same way a microphone is; `None` takes the device as it is and
    /// makes its rate the mix rate.
    ///
    /// Returns the endpoint with the device's friendly name and endpoint id, the id being how the
    /// mixer tells that the default has since moved somewhere else.
    pub fn loopback(
        tone_hz: Option<f32>,
        keep_alive: bool,
        rate: Option<u32>,
    ) -> Result<(Self, String, String)> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
            let name = friendly_name(&device);
            let id = device.GetId().map(|id| unsafe_string(id)).unwrap_or_default();

            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            let wave = client.GetMixFormat()?;
            let native = read_format(wave);
            let (format, downmix, started) = match rate {
                Some(rate) if rate != native.sample_rate => {
                    let wanted = WAVEFORMATEX {
                        wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
                        nChannels: OUT_CHANNELS as u16,
                        nSamplesPerSec: rate,
                        nAvgBytesPerSec: rate * (OUT_CHANNELS as u32) * 4,
                        nBlockAlign: (OUT_CHANNELS * 4) as u16,
                        wBitsPerSample: 32,
                        cbSize: 0,
                    };
                    let format = MixFormat {
                        sample_rate: rate,
                        channels: OUT_CHANNELS as u16,
                        bits: 32,
                        float: true,
                    };
                    let started = client
                        .Initialize(
                            AUDCLNT_SHAREMODE_SHARED,
                            AUDCLNT_STREAMFLAGS_LOOPBACK
                                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                            BUFFER_HNS,
                            0,
                            &wanted,
                            None,
                        )
                        .map_err(|e| {
                            windows::core::Error::new(
                                e.code(),
                                format!(
                                    "{name} runs at {} Hz and cannot be converted to {rate}: {}",
                                    native.sample_rate,
                                    e.message()
                                ),
                            )
                        });
                    (format, vec![(1.0, 0.0), (0.0, 1.0)], started)
                }
                // Loopback does not support event-driven capture, so this stream is polled.
                _ => (
                    native,
                    downmix_weights(wave),
                    client.Initialize(
                        AUDCLNT_SHAREMODE_SHARED,
                        AUDCLNT_STREAMFLAGS_LOOPBACK,
                        BUFFER_HNS,
                        0,
                        wave,
                        None,
                    ),
                ),
            };
            windows::Win32::System::Com::CoTaskMemFree(Some(wave as *const _));
            started?;
            let capture: IAudioCaptureClient = client.GetService()?;
            client.Start()?;

            let render = if keep_alive {
                Silence::start(&device, tone_hz).ok()
            } else {
                None
            };

            Ok((Endpoint {
                _capture_client: client,
                capture,
                _render: render,
                format,
                downmix,
                last_packet_hns: 0,
                first_hns: None,
                skew_hns: 0,
                pacer: Pacer::new(format.sample_rate, LOOPBACK_DEADBAND_HNS),
                pairs: Vec::new(),
                captured: 0,
            }, name, id))
        }
    }

    /// Every application except Clipper, on every output, as float stereo at `rate`. See
    /// `procloop.rs` for why this is the desktop leg's first choice.
    ///
    /// No keep-alive: the virtual device delivers packets through silence on its own — 29,999 of
    /// 30,000 polls over five silent minutes — and its timestamps held to 0.007 ms of the capture
    /// clock over the same run.
    pub fn process_loopback(rate: u32) -> Result<Self> {
        unsafe {
            let client = crate::procloop::activate()?;
            let wanted = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
                nChannels: OUT_CHANNELS as u16,
                nSamplesPerSec: rate,
                nAvgBytesPerSec: rate * (OUT_CHANNELS as u32) * 4,
                nBlockAlign: (OUT_CHANNELS * 4) as u16,
                wBitsPerSample: 32,
                cbSize: 0,
            };
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                BUFFER_HNS,
                0,
                &wanted,
                None,
            )?;
            let capture: IAudioCaptureClient = client.GetService()?;
            client.Start()?;
            let format = MixFormat { sample_rate: rate, channels: OUT_CHANNELS as u16, bits: 32, float: true };
            Ok(Endpoint {
                _capture_client: client,
                capture,
                _render: None,
                format,
                downmix: vec![(1.0, 0.0), (0.0, 1.0)],
                last_packet_hns: 0,
                first_hns: None,
                skew_hns: 0,
                pacer: Pacer::new(rate, LOOPBACK_DEADBAND_HNS),
                pairs: Vec::new(),
                captured: 0,
            })
        }
    }

    /// A capture endpoint — a microphone — resampled by the audio engine to the rate the mixer
    /// already works in.
    ///
    /// `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM` is the whole reason this is short. Mixing two streams
    /// means they have to agree on a sample rate, and a headset at 44.1 kHz beside a desktop
    /// endpoint at 48 does not. Rather than carry a resampler in here, we hand the audio engine the
    /// format we want and let it do the conversion it already knows how to do; when the mic is
    /// natively at our rate, which is the common case, the flag costs nothing.
    ///
    /// Returns the endpoint and the device's friendly name, which the settings panel shows so a mic
    /// that opened is distinguishable from a mic that did not, and its endpoint id, which is how the
    /// mixer tells that the system default has since moved somewhere else.
    pub fn microphone(
        device_id: Option<&str>,
        rate: u32,
    ) -> Result<(Self, String, String, &'static str)> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            // An empty id means "whatever Windows currently calls the default", which is a
            // different promise from "this device": it follows the user plugging in a headset.
            let device = match device_id {
                Some(id) if !id.is_empty() => enumerator.GetDevice(&HSTRING::from(id))?,
                _ => enumerator.GetDefaultAudioEndpoint(eCapture, eConsole)?,
            };
            let name = friendly_name(&device);
            let id = device.GetId().map(|id| unsafe_string(id)).unwrap_or_default();

            let wanted = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
                nChannels: OUT_CHANNELS as u16,
                nSamplesPerSec: rate,
                nAvgBytesPerSec: rate * (OUT_CHANNELS as u32) * 4,
                nBlockAlign: (OUT_CHANNELS * 4) as u16,
                wBitsPerSample: 32,
                cbSize: 0,
            };

            // Take the device as it is when it already agrees with the mix rate, which is the
            // usual case — almost every endpoint runs at 48 kHz. `AUTOCONVERTPCM` asks the audio
            // engine to insert a converter, and `SRC_DEFAULT_QUALITY`, which it has to be paired
            // with, is by name the cheap one; a resampler in the path that has nothing to resample
            // is a stage that can only cost quality. Reading the endpoint's own samples and folding
            // them here is what the desktop leg has always done.
            let native = {
                let probe: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
                let wave = probe.GetMixFormat()?;
                let format = read_format(wave);
                windows::Win32::System::Com::CoTaskMemFree(Some(wave as *const _));
                format
            };

            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            let converted = if native.sample_rate == rate {
                Err(windows::core::Error::new(
                    windows::Win32::Foundation::E_ABORT,
                    "no conversion needed",
                ))
            } else {
                client.Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                    BUFFER_HNS,
                    0,
                    &wanted,
                    None,
                )
            };

            let how = if converted.is_ok() { "converted" } else { "native" };
            let (client, format, downmix) = if converted.is_ok() {
                let format = MixFormat {
                    sample_rate: rate,
                    channels: OUT_CHANNELS as u16,
                    bits: 32,
                    float: true,
                };
                (client, format, vec![(1.0, 0.0), (0.0, 1.0)])
            } else {
                // A client that failed to initialise cannot be initialised again, so the fallback
                // starts from a fresh one. Either the rate already matched and no conversion was
                // asked for, or the driver refused the conversion flags — and in that second case
                // this can only work if the device agrees on the rate anyway.
                drop(client);
                let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
                let wave = client.GetMixFormat()?;
                let format = read_format(wave);
                let downmix = downmix_weights(wave);
                let native = format.sample_rate;
                let started =
                    client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, BUFFER_HNS, 0, wave, None);
                windows::Win32::System::Com::CoTaskMemFree(Some(wave as *const _));
                started?;
                if native != rate {
                    // Taking it anyway would put the voice at the wrong pitch and drifting, which
                    // is worse than saying so.
                    return Err(windows::core::Error::new(
                        windows::Win32::Foundation::E_FAIL,
                        format!("microphone runs at {native} Hz and cannot be converted to {rate}"),
                    ));
                }
                (client, format, downmix)
            };

            let capture: IAudioCaptureClient = client.GetService()?;
            client.Start()?;

            Ok((
                Endpoint {
                    _capture_client: client,
                    capture,
                    _render: None,
                    format,
                    downmix,
                    last_packet_hns: 0,
                    first_hns: None,
                    skew_hns: 0,
                    pacer: Pacer::new(format.sample_rate, 0),
                    pairs: Vec::new(),
                    captured: 0,
                },
                name,
                id,
                how,
            ))
        }
    }

    pub fn format(&self) -> MixFormat {
        self.format
    }

    /// How far this device's own sample clock runs from the capture clock, in parts per million.
    ///
    /// Positive means the device produces samples faster than its timestamps advance; negative,
    /// that it produces fewer than it claims — which is the case that used to be papered over with
    /// silence. Measured against the device's own reported timestamps, so it is a property of the
    /// hardware rather than of anything here.
    pub fn clock_ppm(&self) -> i64 {
        let Some(first) = self.first_hns else { return 0 };
        let span = self.last_packet_hns - first;
        if span <= 0 {
            return 0;
        }
        let nominal = self.captured as i64 * HNS_PER_SECOND / self.format.sample_rate as i64;
        (((nominal - span) as f64 / span as f64) * 1_000_000.0).round() as i64
    }

    /// QPC of the next sample this endpoint will hand over, which is also the timestamp of the
    /// first sample the next `poll` appends — gap filling starts here too. `None` before the first
    /// packet has arrived.
    pub fn position_hns(&self) -> Option<i64> {
        self.pacer.position_hns()
    }

    /// The rate correction currently being applied, as a ratio: exactly 1.0 for a device that is
    /// keeping time. Reported, because a device silently running half a percent slow is the sort
    /// of thing worth being able to see.
    pub fn rate_ratio(&self) -> f64 {
        self.pacer.ratio
    }

    /// Appends interleaved stereo 16-bit samples for everything the endpoint has ready.
    pub fn poll(&mut self, out: &mut Vec<i16>) -> Result<PollStats> {
        let mut pacer = std::mem::replace(&mut self.pacer, Pacer::new(1, 0));
        let mut filled = 0;
        let result = self.read(|hns, pairs, discontinuity| {
            filled += pacer.packet(hns, pairs, discontinuity, out);
        });
        self.pacer = pacer;
        let mut stats = result?;
        stats.filled_frames = filled;
        Ok(stats)
    }

    /// Everything the endpoint has ready, one packet at a time, as its timestamp on the capture
    /// clock, its frames folded to stereo, and whether the device flagged samples lost before it.
    ///
    /// The raw half of `poll`, without the pacer. The desktop leg reads on a thread of its own and
    /// paces on the tick, because opening and reading a device can block for a second and the
    /// tick is also what drives the video.
    pub fn read(&mut self, mut packet: impl FnMut(i64, &[[f32; 2]], bool)) -> Result<PollStats> {
        let mut stats = PollStats::default();
        let rate = self.format.sample_rate as i64;

        loop {
            let available = unsafe { self.capture.GetNextPacketSize()? };
            if available == 0 {
                break;
            }

            let mut data = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            let mut device_pos = 0u64;
            let mut qpc_pos = 0u64;
            unsafe {
                self.capture.GetBuffer(
                    &mut data,
                    &mut frames,
                    &mut flags,
                    Some(&mut device_pos),
                    Some(&mut qpc_pos),
                )?
            };

            if self.first_hns.is_none() {
                self.skew_hns = anchor(qpc_pos as i64, frames as i64 * HNS_PER_SECOND / rate);
            }
            let packet_hns = qpc_pos as i64 + self.skew_hns;
            self.last_packet_hns = packet_hns + frames as i64 * HNS_PER_SECOND / rate;
            if self.first_hns.is_none() {
                self.first_hns = Some(packet_hns);
            }

            let discontinuity = flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0;
            if discontinuity {
                stats.discontinuities += 1;
            }
            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
            if silent {
                // The data pointer is allowed to be meaningless when this flag is set.
                stats.silent_packets += 1;
            }

            let mut pairs = std::mem::take(&mut self.pairs);
            pairs.clear();
            self.decode_pairs(data, frames, silent, &mut pairs);
            packet(packet_hns, &pairs, discontinuity);
            self.pairs = pairs;

            stats.captured_frames += frames as u64;
            self.captured += frames as u64;

            unsafe { self.capture.ReleaseBuffer(frames)? };
        }
        Ok(stats)
    }

    /// One packet, folded to stereo and left in floating point so the resampler has something to
    /// interpolate between.
    fn decode_pairs(&self, data: *const u8, frames: u32, silent: bool, out: &mut Vec<[f32; 2]>) {
        let channels = self.format.channels as usize;
        out.reserve(frames as usize);
        for f in 0..frames as usize {
            if silent {
                out.push([0.0, 0.0]);
                continue;
            }
            let (mut left, mut right) = (0.0f32, 0.0f32);
            for (c, (wl, wr)) in self.downmix.iter().enumerate().take(channels) {
                let v = unsafe { self.sample(data, f * channels + c) };
                left += v * wl;
                right += v * wr;
            }
            out.push([left, right]);
        }
    }

    unsafe fn sample(&self, data: *const u8, index: usize) -> f32 {
        match (self.format.float, self.format.bits) {
            (true, 32) => *(data as *const f32).add(index),
            (false, 16) => *(data as *const i16).add(index) as f32 / 32768.0,
            (false, 32) => *(data as *const i32).add(index) as f32 / 2147483648.0,
            (false, 24) => {
                let p = data.add(index * 3);
                let v = ((*p as i32) | ((*p.add(1) as i32) << 8) | ((*p.add(2) as i8 as i32) << 16))
                    as f32;
                v / 8388608.0
            }
            _ => 0.0,
        }
    }
}

/// Keeps one endpoint's samples on the capture clock, which is the clock the video is stamped with.
///
/// Every packet arrives with the QPC time its first sample was captured at, and this compares that
/// against where our own output has got to. The difference is one of three things, and each wants
/// a different answer:
///
/// * **Jitter.** Packet timestamps scatter either side of where a sample count says they belong,
///   and come back. Nothing is missing, so the answer is to do nothing.
/// * **A clock of its own.** A USB microphone has its own crystal and no reason to match anything:
///   the one on this machine runs **0.545% slow**, which over twenty seconds is 110 ms of samples
///   that never arrive. The answer is to resample by that fraction of a percent.
/// * **A real hole** — the engine stopped, or the device overran and says so with
///   `DATA_DISCONTINUITY`. Samples really are missing, and silence is the honest thing to put
///   there.
///
/// The first version of this treated all three as the third, for both legs: any packet more than
/// half a millisecond later than the last one ended got silence in front of it. Filling only ever
/// adds and never takes away, so every late excursion punched a hole of digital silence into the
/// middle of the waveform — on a USB microphone one every 92 ms, audible as a continuous crackle
/// and reported as "it sounds like interference when I speak". The desktop leg kept that rule for
/// longer, because the render endpoint here keeps perfect time; a render endpoint that does not —
/// a USB or wireless headset, a virtual mixer — crackled the game audio the same way.
///
/// Both legs now share this. They differ only in the deadband: how far out the device may sit
/// before it is corrected at all. Inside it the ratio is exactly one and the samples pass through
/// bit for bit, which is what keeps a healthy desktop leg identical to what it always was.
pub(crate) struct Pacer {
    rate: i64,
    deadband_hns: i64,
    /// Where the first sample belongs. `None` before the first packet.
    base: Option<i64>,
    /// Output frames emitted since the stream began. With resampling this no longer equals the
    /// number of frames the device handed over, so the timeline has to count them separately.
    emitted: u64,
    /// Input frames waiting to be resampled, and the fractional read position within them. The
    /// interpolator reads one frame behind the position and two ahead, so a frame of zeros stands
    /// in for the frame before the first and the last two are always held back for the next packet.
    input: Vec<[f32; 2]>,
    pos: f64,
    /// The correction currently being applied, as a ratio of output frames to input frames.
    pub(crate) ratio: f64,
    /// Set by `resume` until the first packet arrives. See there.
    resuming: bool,
}

impl Pacer {
    pub(crate) fn new(rate: u32, deadband_hns: i64) -> Pacer {
        Pacer {
            rate: rate as i64,
            deadband_hns,
            base: None,
            emitted: 0,
            input: vec![[0.0; 2]],
            pos: 1.0,
            ratio: 1.0,
            resuming: false,
        }
    }

    /// Continues a timeline that another stream began: the next sample emitted belongs at `hns`.
    ///
    /// This is how the desktop leg changes device without the audio sliding against the video. A
    /// fresh pacer starts its timeline at its first packet, so the new device's samples would
    /// follow straight on from the old one's and the gap between them would vanish from the
    /// track — every clip after the switch early by however long it took. Resumed, the gap is a
    /// hole like any other and is filled. The one thing a hole never is, overlap, is handled once
    /// here: a first packet stamped before `hns` loses the part that is already on the timeline,
    /// rather than the rate matching spending seconds working it back off at one percent.
    ///
    /// The rate matching starts over too: it was measuring the old device's clock.
    pub(crate) fn resume(&mut self, hns: i64) {
        self.base = Some(hns);
        self.emitted = 0;
        self.ratio = 1.0;
        self.input = vec![[0.0; 2]];
        self.pos = 1.0;
        self.resuming = true;
    }

    /// Silence up to `hns`, for a source that has stopped delivering: a device between one default
    /// and the next, or a read blocked while a device starts. The track keeps pace with the video
    /// rather than stalling and then arriving all at once, and whatever the source hands over
    /// late for the stretch already covered is trimmed, exactly as after `resume`.
    pub(crate) fn pad_to(&mut self, hns: i64, out: &mut Vec<i16>) -> u64 {
        let Some(position) = self.position_hns() else { return 0 };
        let frames = (hns - position) * self.rate / HNS_PER_SECOND;
        if frames <= 0 {
            return 0;
        }
        out.extend(std::iter::repeat(0).take(frames as usize * OUT_CHANNELS));
        let at = position + frames * HNS_PER_SECOND / self.rate;
        // The frames the resampler held back belong before the silence; two of them are not
        // worth reordering the stream for.
        self.input = vec![[0.0; 2]];
        self.pos = 1.0;
        self.base = Some(at);
        self.emitted = 0;
        self.resuming = true;
        frames as u64
    }

    pub(crate) fn position_hns(&self) -> Option<i64> {
        self.base
            .map(|base| base + self.emitted as i64 * HNS_PER_SECOND / self.rate)
    }

    /// Places one packet on the timeline and appends whatever can be emitted, as interleaved 16-bit
    /// stereo. Returns the frames of silence that had to be invented in front of it.
    pub(crate) fn packet(
        &mut self,
        packet_hns: i64,
        samples: &[[f32; 2]],
        discontinuity: bool,
        out: &mut Vec<i16>,
    ) -> u64 {
        self.base.get_or_insert(packet_hns);
        let mut error = packet_hns - self.position_hns().unwrap_or(packet_hns);
        let mut samples = samples;
        if self.resuming {
            // All of it, not just past the deadband: the deadband is room for a device's jitter
            // around its own timeline, and there is no timeline of its own to jitter around yet.
            if error < 0 {
                let skip = ((-error * self.rate / HNS_PER_SECOND) as usize).min(samples.len());
                samples = &samples[skip..];
                error += skip as i64 * HNS_PER_SECOND / self.rate;
            }
            // A packet that was all overlap leaves the next one to be trimmed too.
            self.resuming = samples.is_empty();
        }

        // A hole: cover it and carry on from here. A flagged discontinuity is the device saying in
        // so many words that samples were lost, so it does not have to be large to be believed.
        let mut filled = 0;
        if error > DROPOUT_HNS || (discontinuity && error > self.deadband_hns) {
            filled = (error * self.rate / HNS_PER_SECOND) as u64;
            out.extend(std::iter::repeat(0).take(filled as usize * OUT_CHANNELS));
            self.emitted += filled;
            error = packet_hns - self.position_hns().unwrap_or(packet_hns);
        }

        // Anything else is the device keeping its own time, and the way to agree with it is to
        // resample rather than to punch holes in what it sent.
        let excess = error.abs() - self.deadband_hns;
        self.ratio = if excess <= 0 {
            1.0
        } else {
            let drift = excess as f64 * error.signum() as f64 / RATE_MATCH_TAU_HNS;
            1.0 + drift.clamp(-RATE_MATCH_MAX, RATE_MATCH_MAX)
        };

        self.input.extend_from_slice(samples);
        self.emitted += self.resample(out);
        filled
    }

    /// Cubic (Catmull-Rom) interpolation at `ratio` output frames per input frame.
    ///
    /// At a ratio of exactly one the read position stays on whole frames, where the interpolator
    /// returns the frame itself, so a device inside its deadband comes out untouched. Away from
    /// one, cubic rather than linear because the desktop leg is music and effects rather than
    /// speech: linear interpolation halfway between two samples is already 3 dB down at 12 kHz,
    /// and as the position slides through each frame that dulling comes and goes several times a
    /// second — a shimmer on every cymbal. Cubic holds it to 1 dB there. A windowed-sinc kernel
    /// would buy nothing audible past that for a correction of a fraction of a percent.
    fn resample(&mut self, out: &mut Vec<i16>) -> u64 {
        let step = 1.0 / self.ratio;
        let mut produced = 0u64;
        while (self.pos as usize) + 2 < self.input.len() {
            let i = self.pos as usize;
            let t = (self.pos - i as f64) as f32;
            let [p0, p1, p2, p3] = [
                self.input[i - 1],
                self.input[i],
                self.input[i + 1],
                self.input[i + 2],
            ];
            for c in 0..OUT_CHANNELS {
                let (a, b, c2, d) = (p0[c], p1[c], p2[c], p3[c]);
                let v = 0.5
                    * (2.0 * b
                        + (c2 - a) * t
                        + (2.0 * a - 5.0 * b + 4.0 * c2 - d) * t * t
                        + (3.0 * (b - c2) + d - a) * t * t * t);
                out.push(to_i16(v));
            }
            self.pos += step;
            produced += 1;
        }
        // Keep the frame before the next read position, which the interpolator reaches back for.
        let consumed = (self.pos as usize).saturating_sub(1);
        if consumed > 0 {
            self.input.drain(..consumed);
            self.pos -= consumed as f64;
        }
        produced
    }
}

/// How far the device's idea of "now" has to be nudged to agree with ours.
///
/// A/V sync rests entirely on `GetBuffer` handing back the QPC value the first sample of a packet
/// was captured at, and on that being the same clock the video leg stamps frames with. Almost
/// always it is. But the timestamp comes from the audio driver, and a driver that reports it
/// wrongly hands every clip a fixed audio lead or lag — which is exactly what a second of delay
/// that a reboot cured looks like from the outside.
///
/// The rate was measured as correct to 0.0 ms over a minute, so this only ever removes a constant
/// offset, and only when the first packet's stamp is somewhere it cannot honestly be: a packet
/// arriving now was captured at most one endpoint buffer ago and certainly not in the future.
/// Inside that window — which is where every healthy device sits — the correction is zero and the
/// device's own timestamps are used untouched.
fn anchor(first_packet_hns: i64, packet_duration_hns: i64) -> i64 {
    let now = crate::clock::qpc_to_hns(crate::clock::qpc_now(), crate::clock::qpc_frequency());
    let plausible = (now - HNS_PER_SECOND / 2)..=(now + HNS_PER_SECOND / 20);
    if plausible.contains(&first_packet_hns) {
        return 0;
    }
    let skew = (now - packet_duration_hns) - first_packet_hns;
    crate::lifecycle::log(&format!(
        "audio endpoint timestamps are {:.0} ms out of step with the capture clock; correcting",
        -skew as f64 / 10_000.0
    ));
    skew
}

fn to_i16(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * 32767.0) as i16
}

/// Keeps the audio engine running so loopback keeps delivering.
struct Silence {
    client: IAudioClient,
    render: IAudioRenderClient,
    buffer_frames: u32,
    format: MixFormat,
    tone_hz: Option<f32>,
    phase: f64,
}

impl Silence {
    fn start(
        device: &windows::Win32::Media::Audio::IMMDevice,
        tone_hz: Option<f32>,
    ) -> Result<Self> {
        unsafe {
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            let wave = client.GetMixFormat()?;
            let format = read_format(wave);
            client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, BUFFER_HNS, 0, wave, None)?;
            let render: IAudioRenderClient = client.GetService()?;
            let buffer_frames = client.GetBufferSize()?;
            windows::Win32::System::Com::CoTaskMemFree(Some(wave as *const _));
            client.Start()?;

            Ok(Silence {
                client,
                render,
                buffer_frames,
                format,
                tone_hz,
                phase: 0.0,
            })
        }
    }

    /// Tops the render buffer back up. Must be called often enough that it never runs dry, or the
    /// engine stops and loopback goes quiet again.
    fn pump(&mut self) -> Result<()> {
        unsafe {
            let padding = self.client.GetCurrentPadding()?;
            let free = self.buffer_frames.saturating_sub(padding);
            if free == 0 {
                return Ok(());
            }
            let ptr = self.render.GetBuffer(free)?;

            let flags = match self.tone_hz {
                None => windows::Win32::Media::Audio::AUDCLNT_BUFFERFLAGS_SILENT.0 as u32,
                Some(hz) => {
                    let channels = self.format.channels as usize;
                    let step = hz as f64 * std::f64::consts::TAU / self.format.sample_rate as f64;
                    for f in 0..free as usize {
                        // Quiet on purpose: loud enough to measure, soft enough not to startle
                        // anyone running the test with speakers on.
                        let v = (self.phase.sin() * 0.2) as f32;
                        self.phase += step;
                        for c in 0..channels {
                            write_sample(ptr, self.format, f * channels + c, v);
                        }
                    }
                    self.phase %= std::f64::consts::TAU;
                    0
                }
            };
            self.render.ReleaseBuffer(free, flags)?;
        }
        Ok(())
    }
}

impl Drop for Silence {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

unsafe fn write_sample(data: *mut u8, format: MixFormat, index: usize, value: f32) {
    match (format.float, format.bits) {
        (true, 32) => *(data as *mut f32).add(index) = value,
        (false, 16) => *(data as *mut i16).add(index) = to_i16(value),
        (false, 32) => *(data as *mut i32).add(index) = (value * 2147483647.0) as i32,
        _ => {}
    }
}

pub(crate) fn read_format(wave: *const WAVEFORMATEX) -> MixFormat {
    unsafe {
        let base = &*wave;
        let mut float = base.wFormatTag == WAVE_FORMAT_IEEE_FLOAT;
        if base.wFormatTag == WAVE_FORMAT_EXTENSIBLE {
            // WAVEFORMATEXTENSIBLE is packed, so the GUID has to be read out rather than borrowed.
            let sub = std::ptr::addr_of!((*(wave as *const WAVEFORMATEXTENSIBLE)).SubFormat)
                .read_unaligned();
            float = sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
        }
        MixFormat {
            sample_rate: base.nSamplesPerSec,
            channels: base.nChannels,
            bits: base.wBitsPerSample,
            float,
        }
    }
}

/// Per-channel gain into left and right.
///
/// WAVEFORMATEXTENSIBLE guarantees channels appear in channel-mask bit order, so the mask is
/// enough to fold 5.1 or 7.1 down without guessing. Centre carries dialogue and surrounds carry
/// most of a game's atmosphere, so dropping them and keeping only front L/R — the easy
/// implementation — would quietly ruin the clip.
fn downmix_weights(wave: *const WAVEFORMATEX) -> Vec<(f32, f32)> {
    const SIDE: f32 = std::f32::consts::FRAC_1_SQRT_2; // -3 dB

    unsafe {
        let base = &*wave;
        let channels = base.nChannels as usize;
        // A mono source goes to both ears. Falling through to the stereo weights would put a
        // headset microphone — and plenty of them report one channel — entirely in the left one.
        if channels == 1 {
            return vec![(1.0, 1.0)];
        }
        if channels == 2 {
            return vec![(1.0, 0.0), (0.0, 1.0)];
        }

        let mask = if base.wFormatTag == WAVE_FORMAT_EXTENSIBLE {
            std::ptr::addr_of!((*(wave as *const WAVEFORMATEXTENSIBLE)).dwChannelMask)
                .read_unaligned()
        } else {
            0
        };
        if mask == 0 {
            // No mask to go on: assume the first two channels are front left and right.
            let mut weights = vec![(0.0, 0.0); channels];
            weights[0] = (1.0, 0.0);
            weights[1] = (0.0, 1.0);
            return weights;
        }

        // Bit order of SPEAKER_* in ksmedia.h.
        let per_bit: [(f32, f32); 11] = [
            (1.0, 0.0),   // front left
            (0.0, 1.0),   // front right
            (SIDE, SIDE), // front centre — dialogue
            (0.0, 0.0),   // low frequency: dropped, it would only muddy a stereo fold
            (SIDE, 0.0),  // back left
            (0.0, SIDE),  // back right
            (SIDE, 0.0),  // front left of centre
            (0.0, SIDE),  // front right of centre
            (SIDE, SIDE), // back centre
            (SIDE, 0.0),  // side left
            (0.0, SIDE),  // side right
        ];

        let mut weights = Vec::with_capacity(channels);
        let mut gain_l = 0.0f32;
        let mut gain_r = 0.0f32;
        for bit in 0..11 {
            if mask & (1 << bit) != 0 && weights.len() < channels {
                let w = per_bit[bit];
                gain_l += w.0;
                gain_r += w.1;
                weights.push(w);
            }
        }
        while weights.len() < channels {
            weights.push((0.0, 0.0));
        }

        // Normalise so a fully-correlated signal cannot clip the fold-down.
        let norm = gain_l.max(gain_r).max(1.0);
        for w in &mut weights {
            w.0 /= norm;
            w.1 /= norm;
        }
        weights
    }
}

const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: windows::core::GUID =
    windows::core::GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

/// Drives the keep-alive stream. Separate from `poll` so the caller can run it on whatever cadence
/// it likes without the borrow fighting.
impl Endpoint {
    pub fn pump_silence(&mut self) -> Result<()> {
        if let Some(render) = &mut self._render {
            render.pump()?;
        }
        Ok(())
    }
}

// ── Devices ───────────────────────────────────────────────────────────────────

/// One capture endpoint, as the settings panel needs to show it.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InputDevice {
    /// The endpoint id, which is what gets stored in the config. Opaque and stable across reboots,
    /// unlike the friendly name, which changes the moment somebody renames the device.
    pub id: String,
    pub name: String,
    /// Whether this is the one "use the default device" currently resolves to. Shown next to that
    /// entry so choosing it is not blind.
    pub default: bool,
    /// The rate the endpoint actually runs at. Worth knowing because a device that disagrees with
    /// the desktop endpoint has to be resampled, and how that is done is audible.
    pub rate: u32,
    pub channels: u16,
}

/// Every active microphone on the machine.
///
/// Active only: an endpoint that is unplugged or disabled would be offered, chosen, and then fail
/// to open, which is a worse experience than not being in the list at all.
pub fn input_devices() -> Vec<InputDevice> {
    let mut devices = Vec::new();
    unsafe {
        let Ok(enumerator) =
            CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
        else {
            return devices;
        };
        let default_id = enumerator
            .GetDefaultAudioEndpoint(eCapture, eConsole)
            .ok()
            .and_then(|d| d.GetId().ok())
            .map(unsafe_string)
            .unwrap_or_default();

        let Ok(collection) = enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) else {
            return devices;
        };
        for index in 0..collection.GetCount().unwrap_or(0) {
            let Ok(device) = collection.Item(index) else {
                continue;
            };
            let Ok(id) = device.GetId().map(|id| unsafe_string(id)) else {
                continue;
            };
            let name = friendly_name(&device);
            let format = device
                .Activate::<IAudioClient>(CLSCTX_ALL, None)
                .and_then(|client| {
                    let wave = client.GetMixFormat()?;
                    let format = read_format(wave);
                    windows::Win32::System::Com::CoTaskMemFree(Some(wave as *const _));
                    Ok(format)
                })
                .unwrap_or(MixFormat {
                    sample_rate: 0,
                    channels: 0,
                    bits: 0,
                    float: false,
                });
            devices.push(InputDevice {
                default: id == default_id,
                id,
                name,
                rate: format.sample_rate,
                channels: format.channels,
            });
        }
    }
    devices.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    devices
}

/// A PWSTR from an endpoint, copied out and freed. `GetId` allocates with the COM task allocator
/// and the caller owns it.
pub(crate) fn unsafe_string(ptr: windows::core::PWSTR) -> String {
    unsafe {
        let text = ptr.to_string().unwrap_or_default();
        windows::Win32::System::Com::CoTaskMemFree(Some(ptr.0 as *const _));
        text
    }
}

fn friendly_name(device: &IMMDevice) -> String {
    use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
    unsafe {
        let Ok(store) = device.OpenPropertyStore(STGM_READ) else {
            return String::new();
        };
        match store.GetValue(&PKEY_Device_FriendlyName) {
            Ok(value) => value.to_string(),
            Err(_) => String::new(),
        }
    }
}

// ── Mixer ─────────────────────────────────────────────────────────────────────

/// How far behind a source may fall before the mix stops waiting for it.
///
/// Both endpoints are polled on the same tick, but they are different devices with different
/// buffering, so one routinely has a few milliseconds the other does not yet. Waiting for the
/// slower one is right, and it is what keeps a voice lined up with the game. Waiting *forever* is
/// not: a microphone that has stopped delivering — unplugged, asleep, taken by an exclusive-mode
/// application — would otherwise stall the whole audio track and, through it, the clip. Past this
/// much lag the mixer gives up on the laggard for that round, fills its share with silence, and
/// carries on.
pub(crate) const MAX_LAG_HNS: i64 = HNS_PER_SECOND / 5;

/// How often to try a microphone that went away. Headsets get unplugged mid-session and plugged
/// back in, and the recorder is supposed to still be recording when that happens.
const MIC_RETRY_HNS: i64 = 3 * HNS_PER_SECOND;

/// How often a leg following the system default checks whether the default has moved — the
/// desktop always, a microphone when none was chosen. One `GetDefaultAudioEndpoint` on a cached
/// enumerator, which is a lookup inside the process, and what the interval trades is the silence
/// after somebody switches devices: measured at up to 750 ms of lost game audio when this was a
/// second, most of it spent waiting to look.
pub(crate) const DEFAULT_CHECK_HNS: i64 = HNS_PER_SECOND / 4;

/// How often to try again when the desktop leg has no device. Shorter than the microphone's,
/// because an output going away almost always means Windows has already picked the next one.
pub(crate) const DESK_RETRY_HNS: i64 = HNS_PER_SECOND;

/// What one source has delivered but the mix has not yet emitted.
#[derive(Default)]
struct Track {
    /// Interleaved stereo, and contiguous: `Endpoint::poll` has already filled any gap the device
    /// left behind.
    buf: VecDeque<i16>,
    /// QPC of the first sample still in `buf`.
    start_hns: i64,
    /// False until the source's first packet arrives, which is the only point at which
    /// `start_hns` means anything.
    started: bool,
    /// Frames of silence the mix had to invent because this source had not produced them, and
    /// frames of this source's audio the mix threw away as too late to use. Both are splices in
    /// the middle of a waveform, so both are worth counting.
    padded: u64,
    dropped: u64,
    /// How many separate times each happened. One splice of 3000 frames at the moment a source
    /// joins is a click nobody will hear; three hundred splices of ten frames each is a crackle
    /// through the whole recording, and the frame totals alone cannot tell them apart.
    pad_events: u64,
    drop_events: u64,
    /// Silence emitted for the stretch before this source existed. Legitimate, and counted apart
    /// from `padded` so that a microphone which opened a tenth of a second after the desktop does
    /// not look like a microphone that is glitching.
    lead: u64,
}

impl Track {
    fn end_hns(&self, rate: i64) -> i64 {
        self.start_hns + (self.buf.len() / OUT_CHANNELS) as i64 * HNS_PER_SECOND / rate
    }

    /// Reads `frames` frames starting at `at_hns`, dropping anything older and padding with silence
    /// where the source has not got that far. Aligning by timestamp on every call, rather than
    /// assuming the two sources stay in lockstep, is what stops two independent device clocks from
    /// drifting apart over a long session.
    fn take(&mut self, at_hns: i64, frames: usize, rate: i64, out: &mut Vec<i16>) {
        out.clear();
        out.reserve(frames * OUT_CHANNELS);
        let want = frames * OUT_CHANNELS;
        if !self.started {
            out.resize(want, 0);
            self.lead += frames as u64;
            return;
        }

        // This source has not reached the span being emitted yet — it opened later than the one
        // that set the timeline. Silence until it did, and then its samples land where they
        // belong. Taking from the front regardless, which is what this used to do, would place
        // every one of them early by however long the source took to arrive, and then spend the
        // rest of the session dropping samples to work the error back off.
        if self.start_hns > at_hns {
            let lead = (((self.start_hns - at_hns) * rate / HNS_PER_SECOND) as usize).min(frames);
            out.resize(lead * OUT_CHANNELS, 0);
            self.lead += lead as u64;
            if lead == frames {
                return;
            }
            let rest = (frames - lead) * OUT_CHANNELS;
            let have = rest.min(self.buf.len());
            out.extend(self.buf.drain(..have));
            self.start_hns += (have / OUT_CHANNELS) as i64 * HNS_PER_SECOND / rate;
            if rest > have {
                self.padded += ((rest - have) / OUT_CHANNELS) as u64;
                self.pad_events += 1;
            }
            out.resize(want, 0);
            return;
        }

        let behind = ((at_hns - self.start_hns) * rate / HNS_PER_SECOND).max(0) as usize;
        let drop = (behind * OUT_CHANNELS).min(self.buf.len());
        self.buf.drain(..drop);
        if drop > 0 {
            self.dropped += (drop / OUT_CHANNELS) as u64;
            self.drop_events += 1;
        }
        self.start_hns += (drop / OUT_CHANNELS) as i64 * HNS_PER_SECOND / rate;

        let have = want.min(self.buf.len());
        out.extend(self.buf.drain(..have));
        self.start_hns += (have / OUT_CHANNELS) as i64 * HNS_PER_SECOND / rate;
        if want > have {
            self.padded += ((want - have) / OUT_CHANNELS) as u64;
            self.pad_events += 1;
        }
        out.resize(want, 0);
    }
}

/// Desktop and microphone, summed into the one stereo track the encoder takes.
pub struct Mixer {
    /// The desktop leg, read on a thread of its own and paced here. See `desktop.rs`: it follows
    /// Windows' default output, and nothing it does to get there can hold the video up.
    desktop: Option<crate::desktop::Desktop>,
    mic: Option<Endpoint>,
    /// Set when a microphone was asked for. Deliberately separate from `mic` being present: a mic
    /// that failed to open is something to report and retry, not something to forget.
    mic_wanted: bool,
    mic_request: String,
    mic_retry_hns: i64,
    /// Endpoint id of the microphone that is open, and when to next ask Windows whether its default
    /// is still that one. Only consulted when `mic_request` is empty — "follow the default" is a
    /// promise that has to hold after the mic is open, not just at the moment it opens.
    mic_id: String,
    mic_default_check_hns: i64,
    enumerator: Option<IMMDeviceEnumerator>,
    /// The gain in `audio.micGain`, as a multiplier.
    mic_gain: f32,
    /// Noise suppression on the microphone leg, when asked for and when the mix runs at the rate
    /// the network needs. `noise_strength` is kept separately so a suppressor rebuilt after the mic
    /// reopens comes back at the same setting.
    noise: Option<crate::denoise::Suppressor>,
    /// Timestamp of the first sample the current suppressor was fed. See `drain_into`.
    noise_origin: Option<i64>,
    noise_wanted: bool,
    noise_strength: f32,
    /// Why suppression is asked for and not running. Only ever the sample rate, today.
    pub noise_error: Option<String>,
    /// Loudest microphone frame in the last second, as a fraction of full scale. The settings panel
    /// shows it: "set the boost so this reads about -12 dB" is advice somebody can act on, and
    /// "turn it up until it sounds right" is not.
    mic_peak: f32,
    mic_peak_frames: u64,
    mic_peak_running: f32,
    rate: u32,
    desk: Track,
    mike: Track,
    /// QPC of the next sample the mix will emit. `None` until the first source speaks.
    out_hns: Option<i64>,
    scratch_a: Vec<i16>,
    scratch_b: Vec<i16>,
    scratch_pcm: Vec<i16>,
    scratch_sum: Vec<i32>,
    pub limiter: Limiter,

    pub first_hns: Option<i64>,
    pub last_packet_hns: i64,
    /// What the microphone is called, and why it is not running when it is not. Both end up in
    /// `status`, so a mic that quietly failed looks different from a mic that is simply quiet.
    pub mic_name: String,
    pub mic_error: Option<String>,
    /// How the endpoint was opened. "converted" means the audio engine is doing the format work,
    /// "native" means it refused the conversion flags and we took its own format.
    pub mic_open: &'static str,
    pub mic_frames: u64,
    /// Dev only: keep each leg as it went into the sum. Two sources summed into one track is the
    /// one place in this pipeline where a fault in the result cannot be traced to a source by
    /// looking at the result, so the sources have to be keepable.
    pub keep_legs: bool,
    pub leg_desktop: Vec<i16>,
    pub leg_mic: Vec<i16>,
    /// The microphone's own gap and discontinuity counters, kept apart from the desktop's. Silence
    /// invented to cover a hole the device left is exactly what a chopped-up voice sounds like, and
    /// summing the two sources' statistics together would hide which one was doing it.
    pub mic_stats: PollStats,
    /// And the desktop's. Zero filled frames on a render endpoint that keeps time; anything else
    /// is the engine stopping or the device overrunning, and each one is a splice in the game audio.
    pub desk_stats: PollStats,
    /// Rounds in which the mic was too far behind to wait for. Zero on a healthy machine.
    pub mic_dropouts: u64,
}

/// What to open. A struct rather than five positional booleans, because three of them are dev
/// hooks and a call site reading `start(true, None, true, false, "")` tells you nothing.
pub struct MixerConfig<'a> {
    pub desktop: bool,
    pub mic: bool,
    /// Endpoint id, or empty for whatever Windows currently calls the default.
    pub mic_device: &'a str,
    /// Decibels of gain on the microphone before the sum.
    pub mic_gain_db: f32,
    /// Noise suppression on the microphone, and how hard: 0–100, see `denoise::setting`.
    pub noise_suppression: bool,
    pub noise_strength: f32,
    /// Makes the keep-alive stream emit a sine instead of silence, which is how the capture path
    /// is tested without needing anything else to be playing.
    pub tone_hz: Option<f32>,
    /// Off drops the keep-alive stream, to show what it prevents.
    pub keep_alive: bool,
}

impl Default for MixerConfig<'_> {
    fn default() -> Self {
        MixerConfig {
            desktop: true,
            mic: false,
            mic_device: "",
            mic_gain_db: 0.0,
            noise_suppression: false,
            noise_strength: 70.0,
            tone_hz: None,
            keep_alive: true,
        }
    }
}

impl Mixer {
    pub fn start(config: MixerConfig) -> Result<Mixer> {
        let MixerConfig {
            desktop,
            mic,
            mic_device,
            mic_gain_db,
            noise_suppression,
            noise_strength,
            tone_hz,
            keep_alive,
        } = config;
        let desktop = if desktop {
            Some(crate::desktop::Desktop::start(crate::desktop::DesktopConfig {
                tone_hz,
                keep_alive,
            })?)
        } else {
            None
        };
        // The desktop endpoint's own rate is the mix rate whenever there is one, so the common path
        // converts nothing. Without it, 48 kHz — what essentially every endpoint runs at, and what
        // the mic's own engine will convert to if it does not.
        let rate = desktop
            .as_ref()
            .map(|d| d.format().sample_rate)
            .unwrap_or(48_000);

        let mut mixer = Mixer {
            desktop,
            mic: None,
            mic_wanted: mic,
            mic_request: mic_device.to_string(),
            mic_retry_hns: 0,
            mic_id: String::new(),
            mic_default_check_hns: 0,
            enumerator: None,
            mic_gain: 10f32.powf(mic_gain_db / 20.0),
            noise: None,
            noise_origin: None,
            noise_wanted: false,
            noise_strength,
            noise_error: None,
            mic_peak: 0.0,
            mic_peak_frames: 0,
            mic_peak_running: 0.0,
            rate,
            desk: Track::default(),
            mike: Track::default(),
            out_hns: None,
            scratch_a: Vec::new(),
            scratch_b: Vec::new(),
            scratch_pcm: Vec::new(),
            scratch_sum: Vec::new(),
            limiter: Limiter::new(),
            first_hns: None,
            last_packet_hns: 0,
            mic_name: String::new(),
            mic_error: None,
            mic_open: "",
            mic_frames: 0,
            mic_stats: PollStats::default(),
            desk_stats: PollStats::default(),
            keep_legs: false,
            leg_desktop: Vec::new(),
            leg_mic: Vec::new(),
            mic_dropouts: 0,
        };
        mixer.set_noise(noise_suppression, noise_strength);
        if mic {
            mixer.open_mic();
        }
        Ok(mixer)
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Frames the mix invented, or threw away, because a source was not where the timeline said.
    /// Zero on a healthy mix; anything else is a splice somebody can hear.
    pub fn mic_padded(&self) -> u64 {
        self.mike.padded
    }
    pub fn mic_dropped(&self) -> u64 {
        self.mike.dropped
    }
    pub fn desk_padded(&self) -> u64 {
        self.desk.padded
    }
    pub fn mic_splices(&self) -> (u64, u64) {
        (self.mike.pad_events, self.mike.drop_events)
    }
    /// Silence before the microphone opened, which is not a fault.
    pub fn mic_lead(&self) -> u64 {
        self.mike.lead
    }
    /// The same three for the desktop leg, so a fault can be attributed rather than guessed at.
    pub fn desk_lead(&self) -> u64 {
        self.desk.lead
    }
    pub fn desk_dropped(&self) -> u64 {
        self.desk.dropped
    }
    pub fn desk_splices(&self) -> (u64, u64) {
        (self.desk.pad_events, self.desk.drop_events)
    }

    /// The desktop endpoint's own format, which is what the mix rate is derived from.
    pub fn desktop_format(&self) -> Option<MixFormat> {
        self.desktop.as_ref().map(|d| d.format())
    }

    pub fn mic_active(&self) -> bool {
        self.mic.is_some()
    }

    /// Changes the microphone's boost without disturbing anything else. Gain is one multiply per
    /// sample, so there is nothing to rebuild for it.
    pub fn set_mic_gain(&mut self, db: f32) {
        self.mic_gain = 10f32.powf(db / 20.0);
    }

    /// Turns suppression on or off, or moves its strength, in place.
    ///
    /// Strength is a blend weight and changes nothing else. Switching it on or off does: the
    /// suppressor delays the voice by a frame and the track's timestamps have to say so, so the
    /// microphone track starts over at the next packet. That costs one splice of a few
    /// milliseconds in the voice, and keeps the buffered minute and the game audio untouched.
    pub fn set_noise(&mut self, on: bool, strength: f32) {
        self.noise_strength = strength;
        if on == self.noise_wanted {
            if let Some(n) = &mut self.noise {
                n.set_strength(strength);
            }
            return;
        }
        self.noise_wanted = on;
        self.noise_error = None;
        self.noise = None;
        self.noise_origin = None;
        if on {
            if self.rate == crate::denoise::RATE {
                self.noise = Some(crate::denoise::Suppressor::new(strength));
            } else {
                self.noise_error = Some(format!(
                    "noise suppression needs 48 kHz and the mix runs at {} Hz — set your output device to 48 kHz in Windows' sound settings",
                    self.rate
                ));
            }
        }
        self.mike = Track::default();
    }

    pub fn noise_active(&self) -> bool {
        self.noise.is_some()
    }

    /// A fresh network for a fresh stream. The old one's state describes audio that is gone, and
    /// its origin timestamp would place the new stream's first sample wherever the old one began.
    fn reset_noise(&mut self) {
        self.noise_origin = None;
        if self.noise.is_some() {
            self.noise = Some(crate::denoise::Suppressor::new(self.noise_strength));
        }
    }

    /// The loudest the microphone has been in the last second, in dBFS after the boost. `None`
    /// when there is no microphone. Silence reads as a large negative number rather than infinity.
    pub fn mic_peak_db(&self) -> Option<f32> {
        self.mic.as_ref()?;
        Some(20.0 * self.mic_peak.max(1e-6).log10())
    }

    /// How far off nominal the microphone's own clock is running, in parts per million. Zero when
    /// there is no microphone; a few thousand is normal for USB and is exactly what the rate
    /// matching is there to absorb.
    pub fn mic_rate_ppm(&self) -> i64 {
        match &self.mic {
            Some(mic) => ((mic.rate_ratio() - 1.0) * 1_000_000.0).round() as i64,
            None => 0,
        }
    }

    /// The same for the desktop leg, which is zero whenever the render endpoint keeps time.
    pub fn desk_rate_ppm(&self) -> i64 {
        match &self.desktop {
            Some(desk) => ((desk.rate_ratio() - 1.0) * 1_000_000.0).round() as i64,
            None => 0,
        }
    }

    /// What the microphone's clock is actually doing, and the desktop endpoint's for comparison.
    pub fn mic_clock_ppm(&self) -> i64 {
        self.mic.as_ref().map(|m| m.clock_ppm()).unwrap_or(0)
    }
    pub fn desk_clock_ppm(&self) -> i64 {
        self.desktop.as_ref().map(|d| d.clock_ppm()).unwrap_or(0)
    }

    /// How the desktop leg is recording: "every app" through process loopback, or "default output"
    /// through the endpoint fallback.
    pub fn desk_method(&self) -> Option<&'static str> {
        self.desktop.as_ref().map(|d| d.method())
    }

    /// What the desktop leg is recording, and why it is not when it is not.
    pub fn desk_name(&self) -> Option<String> {
        self.desktop.as_ref().map(|d| d.name())
    }
    pub fn desk_error(&self) -> Option<String> {
        self.desktop.as_ref().and_then(|d| d.error())
    }

    /// The default output moved to a device the audio engine will not convert to the rate this
    /// mix was built at. Nothing short of a rebuild fixes that, and the daemon does one.
    pub fn desk_rate_changed(&self) -> bool {
        self.desktop.as_ref().is_some_and(|d| d.rate_changed())
    }

    fn enumerator(&mut self) -> Option<&IMMDeviceEnumerator> {
        if self.enumerator.is_none() {
            self.enumerator =
                unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok() };
        }
        self.enumerator.as_ref()
    }

    fn open_mic(&mut self) {
        let device = if self.mic_request.is_empty() {
            None
        } else {
            Some(self.mic_request.as_str())
        };
        match Endpoint::microphone(device, self.rate) {
            Ok((endpoint, name, id, how)) => {
                let format = endpoint.format();
                self.mic_open = how;
                crate::lifecycle::log(&format!(
                    "microphone: {name} ({} Hz, {} ch, {}-bit{}, {})",
                    format.sample_rate,
                    format.channels,
                    format.bits,
                    if format.float { " float" } else { "" },
                    self.mic_open,
                ));
                self.mic = Some(endpoint);
                self.mic_name = name;
                self.mic_id = id;
                self.mic_error = None;
                self.mike = Track::default();
                self.reset_noise();
            }
            Err(e) => {
                let message = e.message().to_string();
                // Worth a line the first time only. The retry runs every few seconds and would
                // otherwise fill the log with the same sentence.
                if self.mic_error.as_deref() != Some(message.as_str()) {
                    crate::lifecycle::log(&format!("microphone unavailable: {message}"));
                }
                self.mic_error = Some(message);
            }
        }
    }

    /// The endpoint id Windows currently calls the default output or input. `None` when there is
    /// none, or when asking failed — neither is a reason to drop a device that is working.
    fn default_id(&mut self, flow: EDataFlow) -> Option<String> {
        unsafe {
            let device = self.enumerator()?.GetDefaultAudioEndpoint(flow, eConsole).ok()?;
            device.GetId().ok().map(|id| unsafe_string(id))
        }
    }

    /// Appends everything both sources have ready, summed, as interleaved stereo.
    pub fn poll(&mut self, out: &mut Vec<i16>) -> Result<PollStats> {
        let now = now_hns();

        // With no microphone asked for there is nothing to align, so the desktop stream goes
        // straight through — the same path, byte for byte, that this had before mixing existed.
        if !self.mic_wanted {
            let Some(desktop) = &mut self.desktop else {
                return Ok(PollStats::default());
            };
            let stats = desktop.poll(out)?;
            self.desk_stats.add(&stats);
            if self.first_hns.is_none() {
                self.first_hns = desktop.first_hns;
            }
            self.last_packet_hns = desktop.last_packet_hns;
            return Ok(stats);
        }
        // An id that could not be read would never match, and the mic would reopen every second.
        if self.mic.is_some()
            && self.mic_request.is_empty()
            && !self.mic_id.is_empty()
            && now >= self.mic_default_check_hns
        {
            self.mic_default_check_hns = now + DEFAULT_CHECK_HNS;
            if let Some(default) = self.default_id(eCapture) {
                if default != self.mic_id {
                    // Treated exactly like an unplug and replug, because that is what it amounts
                    // to: the old stream goes, the retry below opens the new default on this same
                    // poll, and the lag cap fills the few milliseconds between with silence. The
                    // buffer, the video and the desktop leg are not touched.
                    crate::lifecycle::log(&format!(
                        "microphone: system default moved away from {}",
                        self.mic_name
                    ));
                    self.mic = None;
                    self.mike = Track::default();
                    self.reset_noise();
                    self.mic_retry_hns = now;
                }
            }
        }
        if self.mic.is_none() && now >= self.mic_retry_hns {
            self.mic_retry_hns = now + MIC_RETRY_HNS;
            self.open_mic();
        }

        let mut stats = PollStats::default();
        let rate = self.rate as i64;

        let mut scratch = std::mem::take(&mut self.scratch_pcm);
        let desk_stats = drain_into(&mut self.desktop, &mut self.desk, &mut scratch, None)?;
        stats.add(&desk_stats);
        self.desk_stats.add(&desk_stats);

        let noise = self.noise.as_mut().map(|n| (n, &mut self.noise_origin));
        match drain_into(&mut self.mic, &mut self.mike, &mut scratch, noise) {
            Ok(mic_stats) => {
                self.mic_frames += mic_stats.captured_frames;
                self.mic_stats.add(&mic_stats);
            }
            Err(e) => {
                // A microphone that errors mid-stream has usually been unplugged. Drop it, keep
                // recording, and let the retry above find it again when it comes back.
                crate::lifecycle::log(&format!("microphone stopped: {}", e.message()));
                self.mic = None;
                self.mic_error = Some(e.message().to_string());
                self.mike = Track::default();
                self.reset_noise();
                self.mic_retry_hns = now + MIC_RETRY_HNS;
            }
        }
        self.scratch_pcm = scratch;

        // The timeline starts at the first sample anybody produced.
        if self.out_hns.is_none() {
            let start = [&self.desk, &self.mike]
                .iter()
                .filter(|t| t.started)
                .map(|t| t.start_hns)
                .min();
            match start {
                Some(hns) => {
                    self.out_hns = Some(hns);
                    self.first_hns = Some(hns);
                }
                None => return Ok(stats),
            }
        }
        let out_hns = self.out_hns.unwrap();

        // Emit only as far as every source that is keeping up has reached. Waiting for the slower
        // device is what keeps a voice lined up with the game; the lag cap is what stops a device
        // that has stopped delivering from taking the audio track down with it.
        let mut limit = i64::MAX;
        if self.desktop.is_some() && self.desk.started {
            limit = limit.min(self.desk.end_hns(rate));
        }
        if self.mic.is_some() && self.mike.started {
            let end = self.mike.end_hns(rate);
            if end < out_hns - MAX_LAG_HNS {
                self.mic_dropouts += 1;
            } else {
                limit = limit.min(end);
            }
        }
        if limit == i64::MAX || limit <= out_hns {
            return Ok(stats);
        }

        let frames = ((limit - out_hns) * rate / HNS_PER_SECOND) as usize;
        if frames == 0 {
            return Ok(stats);
        }

        let mut a = std::mem::take(&mut self.scratch_a);
        let mut b = std::mem::take(&mut self.scratch_b);
        self.desk.take(out_hns, frames, rate, &mut a);
        self.mike.take(out_hns, frames, rate, &mut b);

        // Summed at unity rather than each halved. Halving would quietly make every clip's game
        // audio 6 dB softer than it was before microphones existed, including through the stretches
        // where nobody says anything. What unity costs is headroom — a microphone set hot in
        // Windows on top of a loud game does reach full scale — and keeping the sum inside it is
        // the limiter's job rather than the clamp's. A clamp turns the loud parts of somebody's own
        // voice into a rasp, which is the one thing they are certain to notice.
        if self.keep_legs {
            self.leg_desktop.extend_from_slice(&a[..frames * OUT_CHANNELS]);
            self.leg_mic.extend_from_slice(&b[..frames * OUT_CHANNELS]);
        }
        // The microphone's boost goes on here, in the sum, where there is room for it. Applying it
        // back at the endpoint would clip against full scale before the limiter could do anything
        // about it, which is the whole reason the limiter is downstream of this.
        let gain = self.mic_gain;
        let mut peak = self.mic_peak_running;
        let mut sum = std::mem::take(&mut self.scratch_sum);
        sum.clear();
        sum.extend((0..frames * OUT_CHANNELS).map(|i| {
            let mic = b[i] as f32 * gain;
            peak = peak.max((mic / 32768.0).abs());
            a[i] as i32 + mic as i32
        }));
        self.mic_peak_running = peak;
        self.mic_peak_frames += frames as u64;
        if self.mic_peak_frames >= self.rate as u64 {
            self.mic_peak = self.mic_peak_running;
            self.mic_peak_running = 0.0;
            self.mic_peak_frames = 0;
        }
        self.limiter.process(&sum, out);
        self.scratch_sum = sum;
        self.scratch_a = a;
        self.scratch_b = b;

        let advanced = frames as i64 * HNS_PER_SECOND / rate;
        self.out_hns = Some(out_hns + advanced);
        self.last_packet_hns = out_hns + advanced;
        Ok(stats)
    }
}

pub(crate) fn now_hns() -> i64 {
    crate::clock::qpc_to_hns(crate::clock::qpc_now(), crate::clock::qpc_frequency())
}

pub(crate) fn log_desktop(name: &str, format: MixFormat) {
    crate::lifecycle::log(&format!(
        "desktop audio: {name} ({} Hz, {} ch, {}-bit{})",
        format.sample_rate,
        format.channels,
        format.bits,
        if format.float { " float" } else { "" },
    ));
}

/// Polls one endpoint, if it is there, into its track.
///
/// With a suppressor in the way, what lands in the track is its output, which is aligned sample for
/// sample with what the endpoint delivered but arrives later. The track is therefore started at
/// the timestamp of the first sample the *endpoint* produced, remembered in `noise_origin`, and not
/// at wherever the endpoint had got to by the time the suppressor had a frame ready.
/// A source the mix can drain: a device read on the tick, or the desktop leg's worker.
trait Leg {
    fn position_hns(&self) -> Option<i64>;
    fn first_hns(&self) -> Option<i64>;
    fn poll(&mut self, out: &mut Vec<i16>) -> Result<PollStats>;
}

impl Leg for Endpoint {
    fn position_hns(&self) -> Option<i64> {
        Endpoint::position_hns(self)
    }
    fn first_hns(&self) -> Option<i64> {
        self.first_hns
    }
    fn poll(&mut self, out: &mut Vec<i16>) -> Result<PollStats> {
        Endpoint::poll(self, out)
    }
}

impl Leg for crate::desktop::Desktop {
    fn position_hns(&self) -> Option<i64> {
        crate::desktop::Desktop::position_hns(self)
    }
    fn first_hns(&self) -> Option<i64> {
        self.first_hns
    }
    fn poll(&mut self, out: &mut Vec<i16>) -> Result<PollStats> {
        crate::desktop::Desktop::poll(self, out)
    }
}

fn drain_into<L: Leg>(
    endpoint: &mut Option<L>,
    track: &mut Track,
    scratch: &mut Vec<i16>,
    noise: Option<(&mut crate::denoise::Suppressor, &mut Option<i64>)>,
) -> Result<PollStats> {
    let Some(endpoint) = endpoint else {
        return Ok(PollStats::default());
    };
    // Where the samples about to be appended belong, read *before* polling. Not `first_hns`: that
    // is where the endpoint's whole stream began, which is only the same thing on the very first
    // call. Getting this from the position each time is what lets a source join a mix that is
    // already running.
    let at = endpoint.position_hns();
    scratch.clear();
    let stats = endpoint.poll(scratch)?;
    if scratch.is_empty() {
        return Ok(stats);
    }
    let mut origin = at.or(endpoint.first_hns());
    if let Some((noise, noise_origin)) = noise {
        if noise_origin.is_none() {
            *noise_origin = origin;
        }
        let raw = std::mem::take(scratch);
        noise.process(&raw, scratch);
        origin = *noise_origin;
        // The suppressor holds up to four frames before it releases anything. Starting the track
        // now, empty, is what makes the mix wait for them: a track that has not started is one the
        // mix does not wait for, so it would run on past `origin` and then throw away all 40 ms
        // when they arrived.
        if scratch.is_empty() {
            if !track.started {
                if let Some(hns) = origin {
                    track.start_hns = hns;
                    track.started = true;
                }
            }
            return Ok(stats);
        }
    }
    if !track.started {
        match origin {
            Some(hns) => {
                track.start_hns = hns;
                track.started = true;
            }
            None => return Ok(stats),
        }
    }
    track.buf.extend(scratch.iter().copied());
    Ok(stats)
}

// ── Limiter ───────────────────────────────────────────────────────────────────

/// Frames the limiter decides gain for at a time. 256 at 48 kHz is 5.3 ms: short enough that the
/// gain follows a syllable, long enough that computing a peak per block costs nothing.
const LIMIT_BLOCK: usize = 256;

/// Where the ceiling sits, a hair under full scale so rounding cannot push a sample over.
const LIMIT_PEAK: f32 = 32600.0;

/// Fraction of the remaining distance back to unity recovered per block. 0.02 over 5.3 ms blocks
/// is a time constant of about a quarter of a second — slow enough not to pump, fast enough that
/// one shout does not hold the game quiet afterwards.
const LIMIT_RELEASE: f32 = 0.02;

/// Keeps the sum of two sources inside 16-bit full scale without chopping the tops off.
///
/// Summing at unity is the right default — see the note where the sum is taken — but a microphone
/// set hot in Windows on top of a loud game does reach full scale, and a hard clamp there is
/// audible as a rasp on exactly the part anybody cares about: their own voice. This turns that into
/// a fraction of a decibel of gain movement nobody can hear.
///
/// **One block of lookahead**, which is the whole point. The gain a block needs is reached by the
/// end of the block *before* it, so a transient can never arrive ahead of the reduction meant for
/// it. The invariant that makes it safe: on entry, `gain` is already no higher than the current
/// block can take, because the previous call ended its ramp there.
#[derive(Default)]
pub struct Limiter {
    /// Summed samples not yet written out: always at least one block held back as lookahead.
    pending: Vec<i32>,
    gain: f32,
    primed: bool,
    /// Frames written with any gain reduction at all, and the deepest reduction reached. Both are
    /// reported, because "was the mix ever actually clipping" is otherwise unanswerable.
    pub limited_frames: u64,
    /// Frames that would have clipped had they simply been clamped, which is what used to happen.
    pub would_clip_frames: u64,
    pub min_gain: f32,
}

impl Limiter {
    pub fn new() -> Limiter {
        Limiter {
            pending: Vec::new(),
            gain: 1.0,
            primed: false,
            limited_frames: 0,
            would_clip_frames: 0,
            min_gain: 1.0,
        }
    }

    /// Appends limited 16-bit samples for everything it can decide on, holding one block back.
    pub fn process(&mut self, input: &[i32], out: &mut Vec<i16>) {
        for frame in input.chunks_exact(OUT_CHANNELS) {
            if frame.iter().any(|s| s.unsigned_abs() > 32767) {
                self.would_clip_frames += 1;
            }
        }
        self.pending.extend_from_slice(input);

        let block = LIMIT_BLOCK * OUT_CHANNELS;
        while self.pending.len() >= 2 * block {
            let here = peak_of(&self.pending[..block]);
            let next = peak_of(&self.pending[block..2 * block]);
            let ceiling = |peak: f32| if peak > LIMIT_PEAK { LIMIT_PEAK / peak } else { 1.0 };
            let here_gain = ceiling(here);
            let next_gain = ceiling(next);

            // The first block has nothing behind it to have ramped during, so it starts wherever
            // it needs to be rather than at unity.
            if !self.primed {
                self.gain = here_gain;
                self.primed = true;
            }

            let start = self.gain.min(here_gain);
            // Down to whatever the next block needs, up no faster than the release allows, and
            // never above what this block itself can take.
            let end = next_gain
                .min(here_gain)
                .min(start + (1.0 - start) * LIMIT_RELEASE);

            for (i, frame) in self.pending[..block].chunks_exact(OUT_CHANNELS).enumerate() {
                let t = i as f32 / LIMIT_BLOCK as f32;
                let gain = start + (end - start) * t;
                for sample in frame {
                    let scaled = *sample as f32 * gain;
                    out.push(scaled.clamp(i16::MIN as f32, i16::MAX as f32) as i16);
                }
            }
            if start < 1.0 || end < 1.0 {
                self.limited_frames += LIMIT_BLOCK as u64;
                self.min_gain = self.min_gain.min(start.min(end));
            }
            self.gain = end;
            self.pending.drain(..block);
        }
    }

    /// Everything still held back, at the gain last settled on. For the end of a stream.
    pub fn flush(&mut self, out: &mut Vec<i16>) {
        for sample in self.pending.drain(..) {
            let scaled = sample as f32 * self.gain;
            out.push(scaled.clamp(i16::MIN as f32, i16::MAX as f32) as i16);
        }
    }
}

fn peak_of(samples: &[i32]) -> f32 {
    samples.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0) as f32
}

#[cfg(test)]
mod pacer_tests {
    use super::*;

    const RATE: i64 = 48_000;
    /// 10 ms, which is what a shared-mode endpoint hands over at a time.
    const PACKET: usize = 480;

    /// A deterministic scatter in ±`spread_hns`, so a failure reproduces.
    struct Jitter(u64, i64);
    impl Jitter {
        fn next(&mut self) -> i64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            if self.1 == 0 {
                return 0;
            }
            ((self.0 >> 33) as i64 % (2 * self.1 + 1)) - self.1
        }
    }

    /// A sine as the device hands it over, and the QPC stamp of each packet. `clock_ppm` is how
    /// fast the device's samples run against the capture clock: negative is a device that delivers
    /// fewer samples than its timestamps say, which is the case that used to be filled.
    fn device(
        seconds: usize,
        clock_ppm: f64,
        jitter_hns: i64,
    ) -> Vec<(i64, Vec<[f32; 2]>)> {
        let mut jitter = Jitter(7, jitter_hns);
        let per_frame = HNS_PER_SECOND as f64 / RATE as f64 / (1.0 + clock_ppm / 1e6);
        (0..seconds * 100)
            .map(|k| {
                let first = k * PACKET;
                let stamp = 1_000_000_000 + (first as f64 * per_frame) as i64 + jitter.next();
                let samples = (first..first + PACKET)
                    .map(|n| {
                        // Phase from integers: `n * 440 * TAU` in floating point has no precision
                        // left by half a minute in, and the fixture stops being a sine.
                        let phase = (n as i64 * 440 % RATE) as f64 / RATE as f64;
                        let v = ((phase * std::f64::consts::TAU).sin() * 0.5) as f32;
                        [v, v]
                    })
                    .collect();
                (stamp, samples)
            })
            .collect()
    }

    fn run(pacer: &mut Pacer, packets: &[(i64, Vec<[f32; 2]>)]) -> (Vec<i16>, u64) {
        let mut out = Vec::new();
        let mut filled = 0;
        for (stamp, samples) in packets {
            filled += pacer.packet(*stamp, samples, false, &mut out);
        }
        (out, filled)
    }

    /// Packets the old rule — silence in front of anything more than half a millisecond later than
    /// the previous packet ended — would have punched a hole before. The fixtures below have to
    /// trip it, or passing proves nothing.
    fn old_rule_holes(packets: &[(i64, Vec<[f32; 2]>)]) -> usize {
        let dur = PACKET as i64 * HNS_PER_SECOND / RATE;
        packets
            .windows(2)
            .filter(|w| w[1].0 - (w[0].0 + dur) > HNS_PER_SECOND / 2000)
            .count()
    }

    /// The largest step between neighbouring samples. A 440 Hz sine at half scale never moves more
    /// than about 950 per sample; a splice of silence into it moves up to 16 000 in one.
    fn worst_step(out: &[i16]) -> i32 {
        out.chunks_exact(OUT_CHANNELS)
            .map(|f| f[0] as i32)
            .collect::<Vec<_>>()
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .max()
            .unwrap()
    }

    fn input_i16(packets: &[(i64, Vec<[f32; 2]>)]) -> Vec<i16> {
        packets
            .iter()
            .flat_map(|(_, s)| s.iter().flat_map(|f| [to_i16(f[0]), to_i16(f[1])]))
            .collect()
    }

    /// A render endpoint that keeps time — this machine's, measured — must come out bit for bit
    /// what it went in as. This is the promise that the desktop leg did not change for anybody
    /// whose audio was already fine.
    #[test]
    fn a_device_that_keeps_time_passes_through_untouched() {
        let packets = device(20, 1.0, 200);
        let mut pacer = Pacer::new(RATE as u32, LOOPBACK_DEADBAND_HNS);
        let (out, filled) = run(&mut pacer, &packets);
        assert_eq!(filled, 0);
        assert_eq!(pacer.ratio, 1.0);
        let input = input_i16(&packets);
        // Two frames are held back as the interpolator's lookahead.
        assert_eq!(out.len(), input.len() - 2 * OUT_CHANNELS);
        assert!(out == input[..out.len()], "samples altered on a device inside its deadband");
    }

    /// The crackle. Timestamps scattered by a few milliseconds, as a USB or wireless headset's are:
    /// the old rule put silence in front of a large share of the packets.
    #[test]
    fn jitter_is_not_filled_with_silence() {
        let packets = device(20, 0.0, 30_000);
        assert!(old_rule_holes(&packets) > 500, "fixture does not jitter enough to matter");

        let mut pacer = Pacer::new(RATE as u32, LOOPBACK_DEADBAND_HNS);
        let (out, filled) = run(&mut pacer, &packets);
        assert_eq!(filled, 0, "invented silence for jitter");
        assert!(out == input_i16(&packets)[..out.len()], "resampled what was only jitter");
    }

    /// A render endpoint on a crystal of its own, a quarter of a percent either way. The old rule
    /// looked only from one packet to the next, so drift never tripped it — the desktop leg simply
    /// slid against the video, 150 ms a minute at this rate. Now it is resampled, the stream stays
    /// continuous, and the timeline ends up where the device's timestamps say it should.
    #[test]
    fn a_slow_clock_is_resampled_rather_than_holed() {
        for clock_ppm in [-2500.0, 2500.0] {
            let packets = device(60, clock_ppm, 2_000);
            let mut pacer = Pacer::new(RATE as u32, LOOPBACK_DEADBAND_HNS);
            let (out, filled) = run(&mut pacer, &packets);
            assert_eq!(filled, 0, "{clock_ppm} ppm: invented silence for drift");
            assert!(worst_step(&out) < 1_100, "{clock_ppm} ppm: discontinuity of {}", worst_step(&out));

            let (last, samples) = packets.last().unwrap();
            let device_end = last + samples.len() as i64 * HNS_PER_SECOND / RATE;
            let error = device_end - pacer.position_hns().unwrap();
            assert!(
                error.abs() < LOOPBACK_DEADBAND_HNS + HNS_PER_SECOND / 200,
                "{clock_ppm} ppm: {} ms off the device's timeline after a minute",
                error as f64 / 10_000.0
            );
        }
    }

    /// Where the engine really did stop, the silence still goes in, and at the right length: this
    /// is what keeps the audio track as long as the video.
    #[test]
    fn a_real_hole_is_still_filled() {
        let mut packets = device(4, 0.0, 0);
        for p in packets.iter_mut().skip(200) {
            p.0 += 2_000_000; // 200 ms with nothing delivered
        }
        let mut pacer = Pacer::new(RATE as u32, LOOPBACK_DEADBAND_HNS);
        let (_, filled) = run(&mut pacer, &packets);
        assert!((9_590..=9_610).contains(&filled), "filled {filled} frames for a 9600-frame hole");
    }

    /// A device that says it lost samples is believed, even when the hole is inside the deadband.
    #[test]
    fn a_flagged_discontinuity_is_filled() {
        let packets = device(1, 0.0, 0);
        let mut pacer = Pacer::new(RATE as u32, LOOPBACK_DEADBAND_HNS);
        let mut out = Vec::new();
        for (stamp, samples) in &packets[..10] {
            pacer.packet(*stamp, samples, false, &mut out);
        }
        let (stamp, samples) = &packets[10];
        let filled = pacer.packet(stamp + 150_000, samples, true, &mut out);
        assert!((718..=722).contains(&filled), "filled {filled} frames for a 15 ms overrun");
    }

    /// The microphone keeps no deadband, as before: a 0.545% slow USB mic, which is the one this
    /// machine has, is pulled onto the timeline without a single frame of silence.
    /// The desktop leg changing device: a second stream carrying on the first one's timeline.
    /// Whatever the gap or overlap between them, the samples have to land at their own stamps, or
    /// every clip after the switch is out of sync by that much.
    fn switch_devices(offset_hns: i64) -> (usize, i64, i64, Pacer) {
        let first = device(2, 0.0, 0);
        let mut a = Pacer::new(RATE as u32, LOOPBACK_DEADBAND_HNS);
        let (out_a, _) = run(&mut a, &first);
        let handover = a.position_hns().unwrap();

        // The next device's packets, stamped from wherever it actually started.
        let second: Vec<_> = device(2, 0.0, 0)
            .into_iter()
            .map(|(stamp, samples)| (stamp - first[0].0 + handover + offset_hns, samples))
            .collect();
        let mut b = Pacer::new(RATE as u32, LOOPBACK_DEADBAND_HNS);
        b.resume(handover);
        let (out_b, _) = run(&mut b, &second);
        let last = second.last().unwrap();
        let end = last.0 + last.1.len() as i64 * HNS_PER_SECOND / RATE;
        (out_a.len() / OUT_CHANNELS + out_b.len() / OUT_CHANNELS, first[0].0, end, b)
    }

    #[test]
    fn a_new_device_after_a_gap_keeps_its_place() {
        let (frames, start, end, pacer) = switch_devices(HNS_PER_SECOND * 3 / 10);
        let expected = ((end - start) * RATE / HNS_PER_SECOND) as usize;
        // The 300 ms between the devices is in the track as silence, not squeezed out of it.
        assert!(frames.abs_diff(expected) <= 2, "{frames} frames, expected {expected}");
        assert_eq!(pacer.ratio, 1.0);
    }

    #[test]
    fn a_new_device_that_overlaps_is_trimmed_not_stretched() {
        let (frames, start, end, pacer) = switch_devices(-HNS_PER_SECOND * 3 / 100);
        let expected = ((end - start) * RATE / HNS_PER_SECOND) as usize;
        // 30 ms stamped before the handover is already on the timeline: dropped, and the rate
        // matching has nothing left to work off.
        assert!(frames.abs_diff(expected) <= 2, "{frames} frames, expected {expected}");
        assert_eq!(pacer.ratio, 1.0);
        assert!(!pacer.resuming);
    }

    #[test]
    fn a_microphone_is_still_rate_matched() {
        let packets = device(20, -5450.0, 3_000);
        let mut pacer = Pacer::new(RATE as u32, 0);
        let (out, filled) = run(&mut pacer, &packets);
        assert_eq!(filled, 0);
        assert!(worst_step(&out) < 1_100);
        assert!((pacer.ratio - 1.00545).abs() < 0.0005, "ratio settled at {}", pacer.ratio);
    }
}

#[cfg(test)]
mod limiter_tests {
    use super::*;

    /// Interleaved stereo sine at a given peak amplitude.
    fn sine(frames: usize, hz: f32, rate: f32, peak: f32) -> Vec<i32> {
        (0..frames)
            .flat_map(|i| {
                let v = (i as f32 * hz * std::f32::consts::TAU / rate).sin() * peak;
                [v as i32, v as i32]
            })
            .collect()
    }

    fn run(limiter: &mut Limiter, input: &[i32], chunk: usize) -> Vec<i16> {
        let mut out = Vec::new();
        for part in input.chunks(chunk * OUT_CHANNELS) {
            limiter.process(part, &mut out);
        }
        limiter.flush(&mut out);
        out
    }

    /// Everything that goes in comes out, once, in order. A limiter that loses or repeats a block
    /// would drift the audio against the video for the rest of the session.
    #[test]
    fn conserves_every_sample() {
        let input = sine(50_000, 440.0, 48_000.0, 8_000.0);
        let mut limiter = Limiter::new();
        let out = run(&mut limiter, &input, 137); // deliberately not a multiple of the block
        assert_eq!(out.len(), input.len(), "sample count changed");
        for (i, (a, b)) in input.iter().zip(out.iter()).enumerate() {
            assert_eq!(*a as i16, *b, "sample {i} altered while there was headroom");
        }
    }

    /// With headroom to spare the limiter is not merely quiet, it is absent: no gain movement at
    /// all. This is the promise that adding a microphone does not make the game quieter.
    #[test]
    fn transparent_below_the_ceiling() {
        let input = sine(30_000, 220.0, 48_000.0, 30_000.0);
        let mut limiter = Limiter::new();
        let out = run(&mut limiter, &input, 512);
        assert_eq!(limiter.limited_frames, 0, "reduced gain with headroom to spare");
        assert_eq!(limiter.would_clip_frames, 0);
        assert!((limiter.min_gain - 1.0).abs() < 1e-6);
        assert_eq!(out.len(), input.len());
    }

    /// The case this exists for: a hot microphone on top of loud game audio. Nothing may reach the
    /// rails, and the reduction has to be in place *before* the loud part rather than after it.
    #[test]
    fn never_clips_a_summed_overload() {
        let game = sine(60_000, 110.0, 48_000.0, 20_000.0);
        let voice = sine(60_000, 700.0, 48_000.0, 26_000.0);
        let summed: Vec<i32> = game.iter().zip(&voice).map(|(a, b)| a + b).collect();
        assert!(
            summed.iter().any(|s| s.unsigned_abs() > 32_767),
            "the fixture has to actually overload, or the test proves nothing"
        );

        let mut limiter = Limiter::new();
        let out = run(&mut limiter, &summed, 256);
        assert_eq!(out.len(), summed.len());
        assert!(limiter.would_clip_frames > 0, "overload went unnoticed");
        assert!(limiter.limited_frames > 0, "limiter never engaged");
        assert!(limiter.min_gain < 1.0);
        // Nothing on the rails, not one sample.
        let peak = out.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!(peak <= 32_700, "peak reached {peak}, which is at or over the ceiling");
    }

    /// A sudden shout after a quiet stretch is the transient that catches limiters without
    /// lookahead. The first loud sample must already be under the ceiling.
    #[test]
    fn catches_a_transient_from_silence() {
        let mut input = vec![0i32; 4_000 * OUT_CHANNELS];
        input.extend(sine(20_000, 500.0, 48_000.0, 60_000.0));
        let mut limiter = Limiter::new();
        let out = run(&mut limiter, &input, 1_000);
        let peak = out.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!(peak <= 32_700, "transient peaked at {peak}");
    }

    /// Gain has to move smoothly. A step between blocks would be a click, which is the artefact
    /// this whole change is meant to remove rather than relocate.
    #[test]
    fn gain_moves_without_steps() {
        let mut input = sine(10_000, 300.0, 48_000.0, 4_000.0);
        input.extend(sine(10_000, 300.0, 48_000.0, 60_000.0));
        input.extend(sine(10_000, 300.0, 48_000.0, 4_000.0));
        let mut limiter = Limiter::new();
        let out = run(&mut limiter, &input, 480);

        // Compare against the input envelope: the ratio between them is the gain, and it must not
        // jump by more than the ramp allows between neighbouring blocks.
        let mut last = 1.0f32;
        for block in 0..(out.len() / OUT_CHANNELS) / LIMIT_BLOCK {
            let lo = block * LIMIT_BLOCK * OUT_CHANNELS;
            let hi = lo + LIMIT_BLOCK * OUT_CHANNELS;
            let inp = input[lo..hi].iter().map(|s| s.unsigned_abs()).max().unwrap() as f32;
            let outp = out[lo..hi].iter().map(|s| s.unsigned_abs()).max().unwrap() as f32;
            if inp < 1.0 {
                continue;
            }
            let gain = outp / inp;
            assert!(
                (gain - last).abs() < 0.6,
                "gain jumped from {last} to {gain} at block {block}"
            );
            last = gain;
        }
    }
}
