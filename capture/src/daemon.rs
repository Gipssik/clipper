//! The recorder as a long-running service: config-driven, controllable, and able to stop and start
//! the pipeline without the process going away.
//!
//! In `game` record mode the pipeline only exists while a game has focus. That is the whole point
//! of the mode — encode silicon is cheap but not free, and neither is a steady trickle of disk
//! writes while you read email. Tearing the pipeline down and building it back up is what the
//! foreground watcher is for.
//!
//! Almost every setting is applied without a restart, because a restart drops the ring and losing
//! the last minute of footage to a slider is a bad trade. `Config::needs_restart` names the few
//! that cannot be.
//!
//! **The pipeline is disposable and the daemon is not.** Everything the recorder depends on can
//! change underneath it while it runs: a display switched to HDR, a monitor turned off to spare an
//! OLED, a resolution change, a driver hiccup. None of those is an error worth exiting on — each
//! one is a rebuild, and the loop below is written so that every path out of a broken pipeline
//! leads back to a working one rather than to a dead process. That is why `tick()` errors are
//! counted instead of propagated: a recorder that quits silently when a screen is switched off is
//! worse than one that loses the buffered minute and carries on.

use std::path::PathBuf;

use windows::Graphics::DirectX::DirectXPixelFormat;

use crate::config::Config;
use crate::ipc::{Command, Control};
use crate::record::{self, Recording};
use crate::gamepad::{self, Bind, Pads};
use crate::{aac, audio, capture, clock, config, convert, d3d, display, encoder, foreground, hotkey, monitors, ring};

/// The timeline starts a second in rather than at zero: a PCR of 0 upsets some players.
const PTS_OFFSET_HNS: i64 = 10_000_000;

/// How often the foreground window is checked. Cheap, and half a second is faster than anyone can
/// alt-tab and start doing something worth keeping.
const FOREGROUND_POLL_MS: f64 = 500.0;

/// How often the display underneath the pipeline is re-examined — HDR state, whether the monitor
/// is still there, what size it is now. A second is well inside the time it takes anyone to notice
/// a gap, and the probe costs a DXGI enumeration.
const HEALTH_POLL_MS: f64 = 1000.0;

/// Consecutive failing ticks before the pipeline is considered lost and rebuilt. A couple of
/// frames can fail transiently while a display mode settles; a wall of them means the device the
/// pipeline was built against is gone.
const TICK_ERROR_LIMIT: u32 = 30;

/// How long the pipeline may produce no encoded frames before it is treated as wedged, in seconds.
///
/// Every stage here can stop without failing. `pump` returns "nothing changed" when the capture is
/// dead, which is exactly what it returns on a still screen. `submit` drops a frame and counts it
/// when the encoder will not take input, and still returns Ok. So `tick()` goes on returning Ok
/// forever while nothing reaches the ring, and the recorder reports itself as recording, with a
/// plausible frame rate, holding a buffer that quietly stopped advancing. That is the worst failure
/// available here, and milestone 6 already paid for the lesson once: a recorder that has stopped
/// has to say so.
///
/// Five seconds is far longer than any legitimate gap. The ticker submits a frame every 16.7 ms
/// whether or not the screen changed, so even a completely still desktop keeps producing packets,
/// and `submit` drops a frame rather than waiting when the encoder will not take one. The margin is
/// for a machine briefly starved of CPU: a rebuild costs the buffered footage, so the bar to call a
/// pipeline dead has to sit well clear of anything a busy moment can produce.
const STALL_SECONDS: f64 = 5.0;

pub struct Options {
    /// Stop after this many seconds. Zero runs until told to quit.
    pub run_seconds: f64,
    /// Fire a save this far in, so the path can be exercised without a keyboard.
    pub save_after: f64,
    /// Stop feeding the pipeline this far in, for up to ten seconds, without telling it. Only that
    /// pipeline: the one the watchdog replaces it with runs normally.
    ///
    /// This exists for the same reason `--no-keepalive` does: the watchdog's whole claim is that a
    /// pipeline which quietly stops producing gets noticed and rebuilt, and a claim that cannot be
    /// made to happen on demand is not a claim, it is a hope. Zero means never.
    pub stall_after: f64,
    /// Start a recording this far in, and stop it `record_for` seconds later (zero: at exit).
    pub record_after: f64,
    pub record_for: f64,
    pub ffmpeg: PathBuf,
    pub config_path: Option<PathBuf>,
    pub ipc: bool,
}

struct Pipeline {
    gpu: d3d::Gpu,
    capture: capture::Capture,
    converter: convert::Converter,
    encoder: encoder::Encoder,
    audio: Option<audio::Mixer>,
    aac: Option<aac::AacEncoder>,
    /// The replay buffer. Absent when instant replay is off and the pipeline exists only because a
    /// recording is running.
    ring: Option<ring::Ring>,
    ticker: clock::Ticker,
    epoch_hns: i64,
    audio_base: Option<i64>,
    freq: i64,
    width: u32,
    height: u32,
    fps: u32,
    /// What the display was doing when this pipeline was built. The pixel format, the white-level
    /// divide and the tone curve are all derived from it, so a change here is a rebuild.
    panel: display::DisplayHdr,
    /// Capture size at build time; the converter and encoder are sized against it.
    src: (i32, i32),
    tone_map: bool,
    encoder_name: String,
    bitrate: u32,
    max_bitrate: u32,
    video_packets: u64,
    audio_packets: u64,
    started: i64,
    last_video_pts_hns: i64,
    /// Consecutive failed ticks. Reset by the first one that works.
    errors: u32,
    /// When a frame last actually reached the ring, and the count it was at. Together these are the
    /// only honest answer to "is this thing still recording".
    last_progress: i64,
    last_progress_packets: u64,
}

impl Pipeline {
    fn start(config: &Config, monitor: &monitors::Monitor, replay: bool) -> crate::Fallible<Self> {
        let panel = display::probe(monitor.handle, &monitor.device);
        let tone_map = match config.tone_map.as_str() {
            "always" => true,
            "off" => false,
            _ => panel.enabled,
        };
        let format = if tone_map {
            DirectXPixelFormat::R16G16B16A16Float
        } else {
            DirectXPixelFormat::B8G8R8A8UIntNormalized
        };

        let tier = config.tier();
        let gpu = d3d::create()?;
        let capture = capture::Capture::start(&gpu, monitor.handle, format)?;
        let src = capture.size();

        // A tier of 0 means "whatever the display is".
        let target_height = if tier.max_height == 0 {
            src.Height as u32
        } else {
            (src.Height as u32).min(tier.max_height)
        };
        let width = (src.Width as u32 * target_height / src.Height.max(1) as u32 + 1) & !1;

        let converter = convert::Converter::new(
            &gpu,
            src.Width as u32,
            src.Height as u32,
            width,
            target_height,
            tone_map,
            panel.white_scale(),
            if tone_map { panel.peak() } else { 1.0 },
        )?;
        let (width, height) = converter.size();

        let encoder = encoder::Encoder::new(
            &gpu,
            &encoder::EncoderConfig {
                width,
                height,
                fps: tier.fps,
                bitrate: tier.bitrate,
                max_bitrate: tier.max_bitrate,
                gop_seconds: config::GOP_SECONDS,
                quality_vs_speed: config::QUALITY_VS_SPEED,
            },
        )?;

        // A microphone that will not open is not a reason to refuse to record, so the mixer keeps
        // asking for it in the background rather than failing here. What is fatal is neither source
        // being available when one was asked for, which `silent()` reports.
        let audio = if config.audio.desktop || config.audio.mic {
            Some(audio::Mixer::start(audio::MixerConfig {
                desktop: config.audio.desktop,
                mic: config.audio.mic,
                mic_device: &config.audio.mic_device,
                mic_gain_db: config.audio.mic_gain_db,
                noise_suppression: config.audio.noise_suppression,
                noise_strength: config.audio.noise_strength,
                ..Default::default()
            })?)
        } else {
            None
        };
        let aac = match &audio {
            Some(mixer) => Some(aac::AacEncoder::new(
                mixer.rate(),
                audio::OUT_CHANNELS as u32,
                24_000,
            )?),
            None => None,
        };

        let ring = if replay {
            Some(new_ring(config, audio.is_some())?)
        } else {
            None
        };

        let freq = clock::qpc_frequency();
        let encoder_name = encoder.name.clone();

        Ok(Pipeline {
            ticker: clock::Ticker::new(tier.fps)?,
            epoch_hns: clock::qpc_to_hns(clock::qpc_now(), freq),
            gpu,
            capture,
            converter,
            encoder,
            audio,
            aac,
            ring,
            audio_base: None,
            freq,
            width,
            height,
            fps: tier.fps,
            panel,
            src: (src.Width, src.Height),
            tone_map,
            encoder_name,
            bitrate: tier.bitrate,
            max_bitrate: tier.max_bitrate,
            video_packets: 0,
            audio_packets: 0,
            started: clock::qpc_now(),
            last_video_pts_hns: 0,
            errors: 0,
            last_progress: clock::qpc_now(),
            last_progress_packets: 0,
        })
    }

    /// What a recording fed by this pipeline is made of. A recording can carry on across a rebuild
    /// only into a pipeline that agrees.
    fn format(&self) -> record::Format {
        record::Format {
            width: self.width,
            height: self.height,
            fps: self.fps,
            audio: self.audio.is_some(),
        }
    }

    /// Starts or stops the replay buffer without touching the rest of the pipeline, for replay
    /// being switched while a recording keeps the pipeline up.
    fn set_replay(&mut self, on: bool, config: &Config) -> crate::Fallible<()> {
        if on && self.ring.is_none() {
            self.ring = Some(new_ring(config, self.audio.is_some())?);
        } else if !on {
            if let Some(mut ring) = self.ring.take() {
                let _ = ring.finish();
                ring.discard();
            }
        }
        Ok(())
    }

    /// Frames actually encoded per second since this pipeline started. The tick rate is supposed
    /// to be the configured fps; anything below it means frames are being lost somewhere.
    fn measured_fps(&self) -> f64 {
        let seconds = clock::qpc_to_ms(clock::qpc_now() - self.started, self.freq) / 1000.0;
        if seconds < 0.5 { 0.0 } else { self.video_packets as f64 / seconds }
    }

    /// How far behind the video the audio currently is, in milliseconds, measured on the one clock
    /// both legs are stamped against.
    ///
    /// This exists because a lip-sync complaint is otherwise unfalsifiable after the fact. A few
    /// tens of milliseconds is the healthy resting state — WASAPI hands over a packet slightly
    /// after the samples in it were captured. Anything near a whole second is the audio endpoint's
    /// timestamps being wrong, which `audio::anchor` corrects and logs.
    fn audio_offset_ms(&self) -> Option<f64> {
        let lb = self.audio.as_ref()?;
        if lb.first_hns.is_none() {
            return None;
        }
        let audio_end = lb.last_packet_hns - self.epoch_hns;
        Some((audio_end - self.last_video_pts_hns) as f64 / 10_000.0)
    }

    /// Where the audio track begins relative to the video, in milliseconds. This is the fixed
    /// offset baked into every clip this pipeline produces.
    fn audio_start_ms(&self) -> Option<f64> {
        self.audio_base.map(|base| base as f64 / 10_000.0)
    }

    fn to_90k(&self, hns: i64) -> u64 {
        ((hns + PTS_OFFSET_HNS).max(0) as u64) * 9 / 1000
    }

    /// One frame. Every packet goes to the replay ring and, while one is running, the recording —
    /// the same bytes to both, which is why a recording costs no encoding at all.
    fn tick(&mut self, mut rec: Option<&mut Recording>) -> crate::Fallible<()> {
        let scheduled = self.ticker.wait();

        // Converted only when the screen changed; a repeated frame is a copy of the last
        // conversion. See "Repeated frames are copied, not converted" in DESIGN.md — including why
        // handing the encoder the same surface twice is not the cheaper option it looks like.
        let fresh = self.capture.pump(&self.gpu)?;
        if let Some(texture) = self.capture.latest() {
            if fresh {
                self.converter.convert(&self.gpu, texture)?;
            } else {
                self.converter.repeat(&self.gpu);
            }
            let pts = clock::qpc_to_hns(scheduled, self.freq) - self.epoch_hns;
            self.last_video_pts_hns = pts;
            self.encoder.submit(self.converter.nv12(), pts)?;
        }

        if let (Some(mixer), Some(enc)) = (&mut self.audio, &mut self.aac) {
            mixer.pump_silence()?;
            let mut pcm = Vec::new();
            mixer.poll(&mut pcm)?;
            if !pcm.is_empty() {
                if self.audio_base.is_none() {
                    self.audio_base = mixer.first_hns.map(|f| f - self.epoch_hns);
                }
                for packet in enc.submit(&pcm, self.audio_base.unwrap_or(0))? {
                    self.audio_packets += 1;
                    let pts = ((packet.pts_hns + PTS_OFFSET_HNS).max(0) as u64) * 9 / 1000;
                    if let Some(r) = rec.as_deref_mut() {
                        r.push_audio(&packet.data, pts);
                    }
                    if let Some(ring) = &mut self.ring {
                        ring.push_audio(packet.data, pts);
                    }
                }
            }
        }

        for packet in self.encoder.take() {
            self.video_packets += 1;
            let pts = self.to_90k(packet.pts_hns);
            if let Some(r) = rec.as_deref_mut() {
                r.push_video(&packet.data, pts, packet.keyframe);
            }
            if let Some(ring) = &mut self.ring {
                ring.push_video(&packet.data, pts, packet.keyframe)?;
            }
        }
        Ok(())
    }

    /// Seconds since a frame last reached the ring.
    fn since_progress(&self) -> f64 {
        clock::qpc_to_ms(clock::qpc_now() - self.last_progress, self.freq) / 1000.0
    }

    /// Notices that the packet count has moved, and remembers when. Called once a second.
    fn note_progress(&mut self) {
        if self.video_packets != self.last_progress_packets {
            self.last_progress_packets = self.video_packets;
            self.last_progress = clock::qpc_now();
        }
    }

    /// Why this pipeline can no longer be trusted, if it cannot. Everything here is either a change
    /// to the thing being captured or the pipeline having quietly stopped.
    fn stale(&self, config: &Config, panel: &display::DisplayHdr) -> Option<&'static str> {
        if self.since_progress() > STALL_SECONDS {
            return Some("the recorder stopped producing frames");
        }
        if self.capture.closed() {
            return Some("the display went away");
        }
        let size = self.capture.size();
        if (size.Width, size.Height) != self.src {
            return Some("the display changed resolution");
        }
        let wants_tone_map = match config.tone_map.as_str() {
            "always" => true,
            "off" => false,
            _ => panel.enabled,
        };
        if wants_tone_map != self.tone_map {
            return Some("the display switched HDR");
        }
        // The SDR white-level divide and the tone curve's peak are baked into the shader's
        // constants at build time. Moving the brightness slider on an HDR display changes the
        // first of those, and a capture that ignores it comes out at the wrong exposure.
        if self.tone_map
            && ((panel.sdr_white_nits - self.panel.sdr_white_nits).abs() > 1.0
                || (panel.max_nits - self.panel.max_nits).abs() > 1.0)
        {
            return Some("the display's brightness changed");
        }
        // The default output moved to a device the audio engine will not convert to the rate this
        // pipeline's encoder was built for. Every other device change is followed in place.
        if self.audio.as_ref().is_some_and(|m| m.desk_rate_changed) {
            return Some("the audio output changed sample rate");
        }
        None
    }

    fn finish(&mut self, mut rec: Option<&mut Recording>) {
        if let Ok(packets) = self.encoder.finish() {
            for packet in packets {
                let pts = self.to_90k(packet.pts_hns);
                if let Some(r) = rec.as_deref_mut() {
                    r.push_video(&packet.data, pts, packet.keyframe);
                }
                if let Some(ring) = &mut self.ring {
                    let _ = ring.push_video(&packet.data, pts, packet.keyframe);
                }
            }
        }
        if let (Some(mixer), Some(enc)) = (&mut self.audio, &mut self.aac) {
            // The limiter holds a block back as lookahead; at the end of a stream there is nothing
            // coming to look ahead at, so it goes out at the gain last settled on.
            let mut tail = Vec::new();
            mixer.limiter.flush(&mut tail);
            if !tail.is_empty() {
                if let Ok(packets) = enc.submit(&tail, self.audio_base.unwrap_or(0)) {
                    for packet in packets {
                        let pts = ((packet.pts_hns + PTS_OFFSET_HNS).max(0) as u64) * 9 / 1000;
                        if let Some(r) = rec.as_deref_mut() {
                            r.push_audio(&packet.data, pts);
                        }
                        if let Some(ring) = &mut self.ring {
                            ring.push_audio(packet.data, pts);
                        }
                    }
                }
            }
        }
        if let Some(enc) = &mut self.aac {
            if let Ok(packets) = enc.finish() {
                for packet in packets {
                    let pts = ((packet.pts_hns + PTS_OFFSET_HNS).max(0) as u64) * 9 / 1000;
                    if let Some(r) = rec.as_deref_mut() {
                        r.push_audio(&packet.data, pts);
                    }
                    if let Some(ring) = &mut self.ring {
                        ring.push_audio(packet.data, pts);
                    }
                }
            }
        }
        if let Some(ring) = &mut self.ring {
            let _ = ring.finish();
        }
    }
}

pub fn run(mut config: Config, options: Options) -> crate::Fallible<serde_json::Value> {
    let control = if options.ipc {
        Some(Control::start())
    } else {
        None
    };
    let emitter = control.as_ref().map(|c| c.emitter());
    let emit = |event: serde_json::Value| {
        if let Some(e) = &emitter {
            e.emit(&event);
        }
    };

    // The chosen display being absent at startup is the same situation as it going away later, and
    // gets the same answer: wait for it. It is the ordinary case at boot on a desk where the main
    // screen is switched off at the wall — exiting here left nothing recording once it came on.
    // Until then `monitor` stands in with whatever display exists, so the loop has a handle to hold;
    // nothing is captured from it, because `display_present` gates the pipeline.
    let mut selector = monitor_selector(&config);
    let all = monitors::list()?;
    let wanted = monitors::resolve(&all, &selector);
    let mut display_present = wanted.is_some();
    let mut monitor = wanted
        .or_else(|| monitors::resolve(&all, ""))
        .ok_or("no displays at all")?;
    if !display_present {
        crate::lifecycle::log(&format!("{selector} is not connected; waiting for it"));
    }

    // Each hotkey exists only while its feature is on: a combination held for a switched-off
    // feature would be taken away from every other program for nothing.
    let register = |wanted: bool, spec: &str, id: i32| {
        if !wanted {
            return None;
        }
        let key = hotkey::Hotkey::register(spec, id);
        if let Err(e) = &key {
            crate::lifecycle::log(&format!("hotkey {spec} unavailable: {}", e.message()));
            emit(serde_json::json!({ "event": "error", "message": format!("hotkey: {}", e.message()) }));
        }
        Some(key)
    };
    // An alternative bind that is a controller button is not a hotkey at all, and is read by the
    // controller reader instead; only a key combination is registered here.
    let key_alt = |spec: &str| !spec.trim().is_empty() && !gamepad::is_pad_spec(spec);
    let mut save_key = register(config.enabled, &config.hotkey, hotkey::SAVE);
    let mut record_key = register(config.record.enabled, &config.record.hotkey, hotkey::RECORD);
    let mut save_alt_key = register(config.enabled && key_alt(&config.alt_hotkey), &config.alt_hotkey, hotkey::SAVE_ALT);
    let mut record_alt_key = register(
        config.record.enabled && key_alt(&config.record.alt_hotkey),
        &config.record.alt_hotkey,
        hotkey::RECORD_ALT,
    );

    // The controller reader, while a bind needs it or the panel is asking which button to use. A
    // wheel base reports hundreds of times a second, so it does not run for nothing.
    let mut pads: Option<Pads> = None;
    let mut pads_retry_at = 0.0f64;
    // The panel asked for a controller button as well as a key, and the capture is still open.
    let mut pad_listen = false;

    let mut watcher = foreground::Watcher::new();
    crate::lifecycle::log(&format!(
        "foreground classifier: {}, {} game(s) already registered with Windows",
        if watcher.measures_gpu() {
            "per-process GPU counter available"
        } else {
            "no GPU counter, falling back to the window heuristic"
        },
        watcher.registered_count()
    ));

    let freq = clock::qpc_frequency();
    let started = clock::qpc_now();
    let mut pipeline: Option<Pipeline> = None;
    let mut front = foreground::Foreground::default();
    let mut last_foreground_check = -1.0e9;
    let mut last_health_check = -1.0e9;
    let mut saves: Vec<std::sync::mpsc::Receiver<Result<ring::SaveOutcome, String>>> = Vec::new();
    let mut save_fired = false;
    let mut results: Vec<serde_json::Value> = Vec::new();
    let mut totals = (0u64, 0u64, 0u64, 0u64);
    let mut quit = false;
    let mut last_heartbeat = 0.0f64;
    let mut panel = display::probe(monitor.handle, &monitor.device);
    let mut last_error = String::new();
    // What the replay was last reported as doing, so `state` goes out on a change rather than from
    // every place that can cause one.
    let mut buffering = false;
    // Which pipeline `--stall-after` wedges, identified by when it started.
    let mut stall_victim: Option<i64> = None;

    // A recording is *wanted* from the moment it is asked for, and *exists* once there is a
    // pipeline to feed it — which is a second or so later when nothing was running, and never
    // while the chosen display is missing. The gap between the two is what `waiting` reports.
    let mut record_wanted = false;
    let mut record_since: Option<std::time::Instant> = None;
    let mut record_fired = false;
    let mut record_started_at = 0.0f64;
    let mut recording: Option<Recording> = None;
    // Set between one file of a recording ending on a format change and the next one starting.
    let mut rolling_over = false;
    let mut finishing: Vec<std::sync::mpsc::Receiver<Result<record::Outcome, String>>> = Vec::new();
    if let Some(rx) = record::recover(&config.recordings_dir(), options.ffmpeg.clone()) {
        finishing.push(rx);
    }

    while !quit {
        let elapsed = clock::qpc_to_ms(clock::qpc_now() - started, freq) / 1000.0;
        if options.run_seconds > 0.0 && elapsed >= options.run_seconds {
            break;
        }

        // A background process with no window has to leave evidence of its own health, or
        // "it recorded almost nothing" is unfalsifiable after the fact. A minute is short enough
        // that a session somebody gave up on after four still leaves a trail.
        if elapsed - last_heartbeat >= 60.0 {
            last_heartbeat = elapsed;
            if let Some(p) = &pipeline {
                crate::lifecycle::log(&format!(
                    "heartbeat: {:.0}s recorded {} frames at {:.1} fps, {} dropped, {} missed ticks, {} segments, audio {:+.0} ms{}{}",
                    elapsed,
                    p.video_packets,
                    p.measured_fps(),
                    p.encoder.dropped,
                    p.ticker.missed,
                    p.ring.as_ref().map_or(0, |r| r.segments_written),
                    p.audio_offset_ms().unwrap_or(0.0),
                    // Only when there is something to say: a healthy desktop leg reads 0 ppm and
                    // nothing filled, and a log that repeats that every minute is one nobody reads.
                    p.audio
                        .as_ref()
                        .filter(|m| m.desk_clock_ppm().abs() > 50 || m.desk_stats.filled_frames > 0)
                        .map(|m| format!(
                            ", desktop clock {:+} ppm, corrected {:+} ppm, {} frames filled",
                            m.desk_clock_ppm(),
                            m.desk_rate_ppm(),
                            m.desk_stats.filled_frames
                        ))
                        .unwrap_or_default(),
                    recording
                        .as_ref()
                        .map(|r| format!(", recording {} s, {} MB", r.duration_ms() / 1000, r.bytes / 1_000_000))
                        .unwrap_or_default()
                ));
            }
        }

        // ── what should be running ────────────────────────────────────────────────
        // Only the replay asks what is in front. A recording records the screen, whatever is on
        // it, so with replay off there is nothing to classify and no GPU counter worth reading.
        if config.enabled && elapsed * 1000.0 - last_foreground_check >= FOREGROUND_POLL_MS {
            last_foreground_check = elapsed * 1000.0;
            let now = watcher.poll(
                monitor.handle,
                &config.include_processes,
                &config.exclude_processes,
                config.game_detection != "fullscreen",
            );
            if now != front {
                front = now;
                emit(serde_json::json!({
                    "event": "foreground",
                    "process": front.process,
                    "category": front.category(),
                    "isGame": front.is_game,
                    "worthRecording": front.worth_recording,
                    "reason": front.reason,
                }));
            }
        }

        // ── is the display still the one we built against ─────────────────────────
        if elapsed * 1000.0 - last_health_check >= HEALTH_POLL_MS {
            last_health_check = elapsed * 1000.0;

            // Re-resolve every time: a monitor that was switched off and back on comes back with a
            // different HMONITOR, and a capture built against the old one will never see a frame.
            match monitors::list()
                .ok()
                .and_then(|all| monitors::resolve(&all, &selector))
            {
                Some(found) => {
                    if !display_present {
                        crate::lifecycle::log(&format!("{} is back", found.friendly));
                        emit(serde_json::json!({ "event": "display", "present": true, "monitor": found.friendly }));
                    }
                    if found.handle != monitor.handle && pipeline.is_some() {
                        retire(pipeline.take(), &mut totals, &mut recording);
                        crate::lifecycle::log("the display came back on a new handle; rebuilding");
                    }
                    monitor = found;
                    display_present = true;
                    panel = display::probe(monitor.handle, &monitor.device);
                }
                None => {
                    if display_present {
                        crate::lifecycle::log(&format!("{} is gone; waiting for it", selector));
                        emit(serde_json::json!({ "event": "display", "present": false, "monitor": selector }));
                    }
                    display_present = false;
                    retire(pipeline.take(), &mut totals, &mut recording);
                }
            }

            if let Some(p) = &mut pipeline {
                p.note_progress();
            }
            if let Some(reason) = pipeline.as_ref().and_then(|p| p.stale(&config, &panel)) {
                // Every counter, because a rebuild is the one moment when what the pipeline was
                // doing beforehand is still knowable.
                let detail = pipeline
                    .as_ref()
                    .map(|p| {
                        format!(
                            " after {} frames at {:.1} fps, {} dropped, {} missed, {} capture frames in, {} empty polls, {} pool rebuilds, {} segments, encoder asked {} / fed {}",
                            p.video_packets,
                            p.measured_fps(),
                            p.encoder.dropped,
                            p.ticker.missed,
                            p.capture.frames_in,
                            p.capture.empty_polls,
                            p.capture.recreates,
                            p.ring.as_ref().map_or(0, |r| r.segments_written),
                            p.encoder.need_input_events,
                            p.encoder.inputs
                        )
                    })
                    .unwrap_or_default();
                crate::lifecycle::log(&format!("rebuilding: {reason}{detail}"));
                emit(serde_json::json!({ "event": "rebuilding", "reason": reason }));
                retire(pipeline.take(), &mut totals, &mut recording);
            }
        }

        // ── recording on demand ───────────────────────────────────────────────────
        let mut toggles = 0u32;
        let mut saves_asked = 0u32;
        for id in hotkey::fired() {
            match id {
                hotkey::SAVE | hotkey::SAVE_ALT => saves_asked += 1,
                hotkey::RECORD | hotkey::RECORD_ALT => toggles += 1,
                _ => {}
            }
        }

        // ── controller buttons ────────────────────────────────────────────────────
        if pad_listen && !hotkey::listening() {
            pad_listen = false;
        }
        let save_pad = if config.enabled { Bind::parse(&config.alt_hotkey) } else { None };
        let record_pad = if config.record.enabled { Bind::parse(&config.record.alt_hotkey) } else { None };
        let want_pads = pad_listen || save_pad.is_some() || record_pad.is_some();
        if want_pads && pads.is_none() && elapsed >= pads_retry_at {
            pads = Pads::start();
            match &pads {
                Some(_) => crate::lifecycle::log("controller reader started"),
                None => pads_retry_at = elapsed + 10.0,
            }
        } else if !want_pads && pads.is_some() {
            pads = None;
            crate::lifecycle::log("controller reader stopped");
        }
        if let Some(reader) = &pads {
            for press in reader.poll() {
                // While the panel is asking, a button is the answer, never an action.
                if pad_listen {
                    crate::lifecycle::log(&format!("controller button offered: {}", press.spec()));
                    hotkey::offer(press.spec());
                    pad_listen = false;
                    continue;
                }
                if save_pad.as_ref().is_some_and(|b| b.matches(&press)) {
                    saves_asked += 1;
                }
                if record_pad.as_ref().is_some_and(|b| b.matches(&press)) {
                    toggles += 1;
                }
            }
        }
        if options.record_after > 0.0 && !record_fired && elapsed >= options.record_after {
            record_fired = true;
            toggles += 1;
        }
        if options.record_for > 0.0
            && record_wanted
            && record_fired
            && elapsed >= record_started_at + options.record_for
        {
            toggles += 1;
        }
        let mut record_asked: Vec<Option<bool>> = (0..toggles).map(|_| None).collect();

        // ── control ───────────────────────────────────────────────────────────────
        while let Some(command) = control.as_ref().and_then(|c| c.try_recv()) {
            if !matches!(command, Command::Status) {
                crate::lifecycle::log(&format!("command: {command:?}"));
            }
            match command {
                Command::Quit => quit = true,
                Command::Save => saves_asked += 1,
                Command::Record(on) => record_asked.push(on),
                Command::Status => emit(status(
                    &config,
                    shown(&monitor, &selector, display_present),
                    &panel,
                    display_present,
                    pipeline.as_ref(),
                    &front,
                    &watcher,
                    &save_key,
                    &record_key,
                    record_status(record_wanted, record_since, recording.as_ref()),
                    alt_status(&save_alt_key, &record_alt_key, pads.is_some()),
                )),
                Command::Reload => {
                    let Some(path) = &options.config_path else { continue };
                    let next = Config::load(path);
                    let restart = config.needs_restart(&next);

                    // All at once, whenever any changes: swapping two combinations over would
                    // otherwise find the new one still held by the old registration.
                    let binds = |c: &Config| {
                        (
                            c.enabled,
                            c.hotkey.clone(),
                            c.alt_hotkey.clone(),
                            c.record.enabled,
                            c.record.hotkey.clone(),
                            c.record.alt_hotkey.clone(),
                        )
                    };
                    if binds(&config) != binds(&next) {
                        drop(save_key.take());
                        drop(record_key.take());
                        drop(save_alt_key.take());
                        drop(record_alt_key.take());
                        save_key = register(next.enabled, &next.hotkey, hotkey::SAVE);
                        record_key = register(next.record.enabled, &next.record.hotkey, hotkey::RECORD);
                        save_alt_key =
                            register(next.enabled && key_alt(&next.alt_hotkey), &next.alt_hotkey, hotkey::SAVE_ALT);
                        record_alt_key = register(
                            next.record.enabled && key_alt(&next.record.alt_hotkey),
                            &next.record.alt_hotkey,
                            hotkey::RECORD_ALT,
                        );
                    }
                    if restart {
                        retire(pipeline.take(), &mut totals, &mut recording);
                        selector = monitor_selector(&next);
                        match monitors::list().ok().and_then(|all| monitors::resolve(&all, &selector)) {
                            Some(m) => {
                                monitor = m;
                                panel = display::probe(monitor.handle, &monitor.device);
                                display_present = true;
                            }
                            None => display_present = false,
                        }
                    }
                    // Applied in place rather than through a rebuild, so dragging the slider
                    // does not cost the footage already buffered.
                    if let Some(mixer) = pipeline.as_mut().and_then(|p| p.audio.as_mut()) {
                        mixer.set_mic_gain(next.audio.mic_gain_db);
                        mixer.set_noise(next.audio.noise_suppression, next.audio.noise_strength);
                    }
                    // Switching the feature off is the one way to be sure nothing is recording.
                    if !next.record.enabled && record_wanted {
                        record_asked.push(Some(false));
                    }
                    crate::lifecycle::log(&format!(
                        "config reloaded{}",
                        if restart { " (pipeline restarted)" } else { "" }
                    ));
                    config = next;
                    // The panel is waiting on this to redraw. Sending the new state straight away
                    // is the difference between a control that responds and one that looks stuck
                    // until the next poll happens to come round.
                    emit(serde_json::json!({ "event": "reloaded", "restarted": restart }));
                    emit(status(
                        &config,
                        shown(&monitor, &selector, display_present),
                        &panel,
                        display_present,
                        pipeline.as_ref(),
                        &front,
                        &watcher,
                        &save_key,
                        &record_key,
                        record_status(record_wanted, record_since, recording.as_ref()),
                        alt_status(&save_alt_key, &record_alt_key, pads.is_some()),
                    ));
                }
                // The settings panel cannot capture Alt+F-key itself — Windows eats those above
                // every layer Electron can reach — so it asks the daemon, which can install a
                // low-level hook. See hotkey.rs.
                Command::Listen(with_pads) => {
                    pad_listen = with_pads;
                    let reply = emitter.clone();
                    hotkey::listen(std::time::Duration::from_secs(15), move |spec| {
                        if let Some(reply) = reply {
                            reply.emit(&serde_json::json!({
                                "event": "hotkey-captured",
                                "spec": spec,
                            }));
                        }
                    });
                }
                Command::Unknown(what) => {
                    emit(serde_json::json!({ "event": "error", "message": format!("unknown command: {what}") }));
                }
            }
        }

        for on in record_asked {
            let start = on.unwrap_or(!record_wanted);
            if start == record_wanted {
                continue;
            }
            if start {
                record_wanted = true;
                record_since = Some(std::time::Instant::now());
                record_started_at = elapsed;
                crate::lifecycle::log("recording requested");
            } else {
                record_wanted = false;
                record_since = None;
                rolling_over = false;
                if let Some(rec) = recording.take() {
                    stop_recording(rec, false, &mut finishing, &options.ffmpeg, &emit);
                } else {
                    // Asked for and stopped before there was ever a picture to put in it.
                    emit(serde_json::json!({ "event": "record-stopped", "path": null }));
                }
            }
        }

        // `worth_recording`, not `is_game`: a clip filed in the wrong folder can be moved, and a
        // clip that was never recorded cannot. A recording overrides all of it — it was asked for.
        let replay_wants = config.enabled && (config.record_mode != "game" || front.worth_recording);
        let should_run = display_present && (replay_wants || record_wanted);

        if should_run && pipeline.is_none() {
            match Pipeline::start(&config, &monitor, config.enabled) {
                Ok(p) => {
                    crate::lifecycle::log(&format!(
                        "capturing {}x{} from {} ({}, {}-{} Mbps, {}{}, {} candidate(s) rejected first){}",
                        p.width,
                        p.height,
                        monitor.friendly,
                        p.encoder_name,
                        p.bitrate / 1_000_000,
                        p.max_bitrate / 1_000_000,
                        p.encoder.applied.join(" "),
                        if p.converter.resampling { " resampled" } else { "" },
                        p.encoder.rejected,
                        if record_wanted && !replay_wants { " for a recording" } else { "" }
                    ));
                    last_error.clear();
                    // A recording carries on across a rebuild only into a pipeline that makes the
                    // same kind of picture and sound; otherwise this file ends here and the next
                    // one starts below.
                    if recording.as_ref().is_some_and(|r| r.format != p.format()) {
                        crate::lifecycle::log("the picture or sound changed shape; the recording continues in a new file");
                        if let Some(rec) = recording.take() {
                            stop_recording(rec, true, &mut finishing, &options.ffmpeg, &emit);
                            rolling_over = true;
                        }
                    }
                    pipeline = Some(p);
                }
                Err(e) => {
                    let message = e.to_string();
                    // A display that has just come back rejects a capture for a moment. Saying so
                    // once is useful; saying it every five seconds is noise in a log somebody will
                    // have to read later.
                    if message != last_error {
                        crate::lifecycle::log(&format!("failed to start pipeline: {message}"));
                        emit(serde_json::json!({ "event": "error", "message": message.clone() }));
                        last_error = message;
                    }
                    // Do not spin on a failure that will simply recur.
                    std::thread::sleep(std::time::Duration::from_secs(5));
                }
            }
        } else if !should_run && pipeline.is_some() {
            // Replay switched off: the buffer is not coming back, and with the daemon now staying
            // up for the record hotkey, nothing else would sweep it until replay is switched on.
            if !config.enabled {
                if let Some(p) = &mut pipeline {
                    let _ = p.set_replay(false, &config);
                }
            }
            retire(pipeline.take(), &mut totals, &mut recording);
            crate::lifecycle::log(if config.enabled {
                "stopped capturing (nothing in focus worth keeping)"
            } else {
                "stopped capturing (instant replay is off and nothing is being recorded)"
            });
        }

        // Replay switched while a recording holds the pipeline up: only the ring changes.
        if let Some(p) = &mut pipeline {
            if p.ring.is_some() != config.enabled {
                if let Err(e) = p.set_replay(config.enabled, &config) {
                    crate::lifecycle::log(&format!("replay buffer: {e}"));
                    emit(serde_json::json!({ "event": "error", "message": format!("replay buffer: {e}") }));
                }
            }
        }

        if record_wanted && recording.is_none() {
            if let Some(p) = &mut pipeline {
                match Recording::start(&config.recordings_dir(), p.format()) {
                    Ok(rec) => {
                        // Otherwise the file opens on the stream's next natural keyframe, up to
                        // two seconds after the hotkey.
                        let forced = p.encoder.force_keyframe();
                        crate::lifecycle::log(&format!(
                            "recording to {}{}",
                            rec.path().display(),
                            if forced { "" } else { " (keyframe not forced; starts on the next one)" }
                        ));
                        // `continued` marks the next file of a recording that rolled over, so the
                        // app does not announce a start nobody asked for.
                        emit(serde_json::json!({
                            "event": "record-started",
                            "path": rec.path().to_string_lossy(),
                            "continued": rolling_over,
                        }));
                        rolling_over = false;
                        recording = Some(rec);
                    }
                    Err(e) => {
                        crate::lifecycle::log(&format!("cannot start a recording: {e}"));
                        emit(serde_json::json!({ "event": "error", "message": format!("recording: {e}") }));
                        record_wanted = false;
                        record_since = None;
                    }
                }
            }
        }

        let now_buffering = pipeline.as_ref().is_some_and(|p| p.ring.is_some());
        if now_buffering != buffering {
            buffering = now_buffering;
            emit(serde_json::json!({ "event": "state", "recording": buffering }));
        }

        // ── one frame, or idle ────────────────────────────────────────────────────
        // The deliberate stall. Everything else carries on exactly as it would: the loop runs, the
        // daemon answers, nothing errors — only the frames stop. It is the pipeline running when
        // the stall begins that wedges, and the one the watchdog builds to replace it runs
        // normally, as it would after a real wedge; stalling that one too left it built but
        // unticked for seconds, and its first frames then came out with a hole between them.
        let in_stall_window = options.stall_after > 0.0
            && elapsed >= options.stall_after
            && elapsed < options.stall_after + 10.0;
        if in_stall_window && stall_victim.is_none() {
            stall_victim = pipeline.as_ref().map(|p| p.started);
        }
        let faking_a_stall = in_stall_window
            && stall_victim.is_some()
            && pipeline.as_ref().map(|p| p.started) == stall_victim;
        if faking_a_stall {
            std::thread::sleep(std::time::Duration::from_millis(16));
        }

        match &mut pipeline {
            Some(_) if faking_a_stall => {}
            Some(p) => match p.tick(recording.as_mut()) {
                Ok(()) => p.errors = 0,
                Err(e) => {
                    p.errors += 1;
                    if p.errors >= TICK_ERROR_LIMIT {
                        crate::lifecycle::log(&format!(
                            "pipeline failed {TICK_ERROR_LIMIT} ticks running ({e}); rebuilding"
                        ));
                        emit(serde_json::json!({ "event": "rebuilding", "reason": e.to_string() }));
                        retire(pipeline.take(), &mut totals, &mut recording);
                    }
                }
            },
            None => std::thread::sleep(std::time::Duration::from_millis(200)),
        }

        // A recording that cannot write — a full disk, a drive pulled out — is stopped and what it
        // holds is kept, rather than going on "recording" into nothing.
        if let Some(message) = recording.as_ref().and_then(|r| r.error()).map(str::to_string) {
            crate::lifecycle::log(&format!("recording stopped: {message}"));
            emit(serde_json::json!({ "event": "error", "message": format!("recording stopped: {message}") }));
            record_wanted = false;
            record_since = None;
            if let Some(rec) = recording.take() {
                stop_recording(rec, false, &mut finishing, &options.ffmpeg, &emit);
            }
        }

        // ── saving ────────────────────────────────────────────────────────────────
        let auto = options.save_after > 0.0 && !save_fired && elapsed >= options.save_after;
        if auto {
            save_fired = true;
            saves_asked += 1;
        }
        for _ in 0..saves_asked {
            match pipeline.as_mut().and_then(|p| p.ring.as_mut()) {
                Some(ring) => {
                    let out = save_path(&config, &front);
                    crate::lifecycle::log(&format!("saving {}", out.display()));
                    saves.push(ring.save(
                        (config.buffer_seconds * 1000.0) as u64,
                        out,
                        options.ffmpeg.clone(),
                    ));
                }
                None => {
                    emit(serde_json::json!({
                        "event": "error",
                        "message": if config.enabled {
                            "nothing is being recorded right now"
                        } else {
                            "instant replay is off"
                        },
                    }));
                }
            }
        }

        saves.retain(|rx| match rx.try_recv() {
            Ok(Ok(outcome)) => {
                crate::lifecycle::log(&format!(
                    "saved {} ({} segments, {:.0} ms)",
                    outcome.path.display(),
                    outcome.segments,
                    outcome.elapsed_ms
                ));
                emit(serde_json::json!({
                    "event": "clip-saved",
                    "path": outcome.path.to_string_lossy(),
                    "durationMs": outcome.actual_ms,
                    "game": front.category(),
                }));
                results.push(serde_json::json!({
                    "path": outcome.path.to_string_lossy(),
                    "segments": outcome.segments,
                    "requestedMs": outcome.requested_ms,
                    "actualMs": outcome.actual_ms,
                    "elapsedMs": outcome.elapsed_ms,
                }));
                false
            }
            Ok(Err(message)) => {
                crate::lifecycle::log(&format!("save failed: {message}"));
                emit(serde_json::json!({ "event": "error", "message": message }));
                results.push(serde_json::json!({ "error": message }));
                false
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => false,
            Err(_) => true,
        });

        finishing.retain(|rx| loop {
            match rx.try_recv() {
                Ok(outcome) => {
                    let value = recorded(outcome, &emit);
                    results.push(value);
                    // A recovery sends one result per orphan; keep reading the same channel.
                    continue;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break false,
                Err(_) => break true,
            }
        });
    }

    if let Some(rec) = recording.take() {
        // Whatever the pipeline still holds belongs in the file before it closes.
        let mut rec = rec;
        if let Some(mut p) = pipeline.take() {
            p.finish(Some(&mut rec));
            collect(&mut totals, &p);
        }
        stop_recording(rec, false, &mut finishing, &options.ffmpeg, &emit);
    }
    retire(pipeline.take(), &mut totals, &mut recording);

    // A save started near the end is a background copy and has to be allowed to finish rather than
    // be killed by the daemon shutting down. A recording's remux is the same, and if Clipper exits
    // first and takes this process with it, the `.ts` is left for `record::recover` to finish.
    for rx in saves {
        match rx.recv() {
            Ok(Ok(outcome)) => results.push(serde_json::json!({
                "path": outcome.path.to_string_lossy(),
                "segments": outcome.segments,
                "requestedMs": outcome.requested_ms,
                "actualMs": outcome.actual_ms,
                "elapsedMs": outcome.elapsed_ms,
            })),
            Ok(Err(message)) => results.push(serde_json::json!({ "error": message })),
            Err(_) => {}
        }
    }
    for rx in finishing {
        for outcome in rx.iter() {
            results.push(recorded(outcome, &emit));
        }
    }

    Ok(serde_json::json!({
        "monitor": monitor.friendly,
        "elapsedMs": clock::qpc_to_ms(clock::qpc_now() - started, freq),
        "videoPackets": totals.0,
        "audioPackets": totals.1,
        "segmentsWritten": totals.2,
        "bytesWritten": totals.3,
        "saves": results,
    }))
}

fn new_ring(config: &Config, has_audio: bool) -> std::io::Result<ring::Ring> {
    ring::Ring::new(
        &config.segment_dir(),
        config::GOP_SECONDS as f64,
        config.buffer_seconds,
        has_audio,
    )
}

/// Takes a pipeline out of service. What it still holds goes to the ring and to the recording, and
/// the recording is left waiting for whichever pipeline comes next.
fn retire(p: Option<Pipeline>, totals: &mut (u64, u64, u64, u64), rec: &mut Option<Recording>) {
    if let Some(mut p) = p {
        p.finish(rec.as_mut());
        collect(totals, &p);
    }
    if let Some(r) = rec {
        r.detach();
    }
}

/// `continuing` is a file ending because the recording rolls over into a new one, not because it
/// was stopped — the app keeps its clock running rather than showing it stopped.
fn stop_recording(
    rec: Recording,
    continuing: bool,
    finishing: &mut Vec<std::sync::mpsc::Receiver<Result<record::Outcome, String>>>,
    ffmpeg: &std::path::Path,
    emit: &dyn Fn(serde_json::Value),
) {
    crate::lifecycle::log(&format!(
        "recording stopped at {} s, {} MB; finishing {}",
        rec.duration_ms() / 1000,
        rec.bytes / 1_000_000,
        rec.path().display()
    ));
    emit(serde_json::json!({
        "event": "record-stopped",
        "path": rec.path().to_string_lossy(),
        "durationMs": rec.duration_ms(),
        "continuing": continuing,
    }));
    finishing.push(rec.finish(ffmpeg.to_path_buf()));
}

/// Reports one finished recording, for the log, the UI and the `record` subcommand's summary.
fn recorded(outcome: Result<record::Outcome, String>, emit: &dyn Fn(serde_json::Value)) -> serde_json::Value {
    match outcome {
        Ok(o) => {
            crate::lifecycle::log(&format!(
                "recording saved: {} ({} s, {} MB, remuxed in {:.0} ms)",
                o.path.display(),
                o.duration_ms / 1000,
                o.bytes / 1_000_000,
                o.elapsed_ms
            ));
            emit(serde_json::json!({
                "event": "record-saved",
                "path": o.path.to_string_lossy(),
                "durationMs": o.duration_ms,
                "bytes": o.bytes,
            }));
            serde_json::json!({
                "recording": o.path.to_string_lossy(),
                "durationMs": o.duration_ms,
                "bytes": o.bytes,
                "elapsedMs": o.elapsed_ms,
            })
        }
        Err(message) => {
            crate::lifecycle::log(&format!("recording could not be finished: {message}"));
            emit(serde_json::json!({ "event": "error", "message": format!("recording: {message}") }));
            serde_json::json!({ "recordingError": message })
        }
    }
}

/// The alternative binds. A key combination can be refused like any hotkey; a controller button
/// cannot be refused, but its device can be unplugged, which the panel says under the field —
/// `controllers` is what is connected right now, by the id a bind matches on.
fn alt_status(
    save_alt: &Option<windows::core::Result<hotkey::Hotkey>>,
    record_alt: &Option<windows::core::Result<hotkey::Hotkey>>,
    reading: bool,
) -> serde_json::Value {
    serde_json::json!({
        "hotkeyOk": !matches!(save_alt, Some(Err(_))),
        "recordHotkeyOk": !matches!(record_alt, Some(Err(_))),
        "reading": reading,
        "controllers": gamepad::list(),
    })
}

/// What the panel and the titlebar need to show a recording: whether one was asked for, whether
/// it has a picture yet, and how far it has got.
fn record_status(
    wanted: bool,
    since: Option<std::time::Instant>,
    rec: Option<&Recording>,
) -> serde_json::Value {
    serde_json::json!({
        "active": wanted,
        // Asked for, but no frame written yet: the display is missing, or the pipeline is still
        // coming up. Worth saying, since the hotkey was pressed and the file is not growing.
        "waiting": wanted && rec.map_or(true, |r| r.waiting()),
        "elapsedMs": since.map(|t| t.elapsed().as_millis() as u64),
        "durationMs": rec.map(|r| r.duration_ms()),
        "bytes": rec.map(|r| r.bytes),
        "path": rec.map(|r| r.path().to_string_lossy().to_string()),
    })
}

fn collect(totals: &mut (u64, u64, u64, u64), p: &Pipeline) {
    totals.0 += p.video_packets;
    totals.1 += p.audio_packets;
    if let Some(ring) = &p.ring {
        totals.2 += ring.segments_written;
        totals.3 += ring.bytes_written;
    }
}

/// Friendly name first — it survives a display being replugged, which the GDI device name does not.
fn monitor_selector(config: &Config) -> String {
    if !config.monitor.friendly.is_empty() {
        config.monitor.friendly.clone()
    } else {
        config.monitor.device.clone()
    }
}

/// The display the status line talks about. While the chosen one is missing, `monitor` is only a
/// stand-in, and "waiting for <the screen you can see>" would be exactly backwards.
fn shown<'a>(monitor: &'a monitors::Monitor, selector: &'a str, present: bool) -> &'a str {
    if present || selector.is_empty() { &monitor.friendly } else { selector }
}

fn save_path(config: &Config, front: &foreground::Foreground) -> PathBuf {
    let mut dir = config.output_dir();
    if config.per_game_subfolder {
        dir = dir.join(front.category());
    }
    dir.join(crate::timestamp_name("clip"))
}

/// Everything the settings panel needs to say something true about what is happening.
///
/// Built fresh on every request rather than cached: a daemon that has stopped answering is exactly
/// the failure worth seeing, and a cache hides it behind numbers that look fine.
#[allow(clippy::too_many_arguments)]
fn status(
    config: &Config,
    monitor: &str,
    panel: &display::DisplayHdr,
    display_present: bool,
    pipeline: Option<&Pipeline>,
    front: &foreground::Foreground,
    watcher: &foreground::Watcher,
    save_key: &Option<windows::core::Result<hotkey::Hotkey>>,
    record_key: &Option<windows::core::Result<hotkey::Hotkey>>,
    record: serde_json::Value,
    alt: serde_json::Value,
) -> serde_json::Value {
    let tier = config.tier();
    serde_json::json!({
        "event": "status",
        // The replay buffer is running. The pipeline can also be up for a recording alone, which is
        // `capturing` without `recording`.
        "recording": pipeline.is_some_and(|p| p.ring.is_some()),
        "capturing": pipeline.is_some(),
        "enabled": config.enabled,
        "recordMode": config.record_mode,
        "bufferedMs": pipeline.and_then(|p| p.ring.as_ref()).map(|r| r.buffered_90k() * 1000 / ring::HZ).unwrap_or(0),
        "bufferSeconds": config.buffer_seconds,
        "monitor": monitor,
        "displayPresent": display_present,
        "hdr": panel,
        "encoder": pipeline.map(|p| p.encoder_name.clone()),
        "encoderSettings": pipeline.map(|p| p.encoder.applied.join(" ")),
        "size": pipeline.map(|p| serde_json::json!({ "width": p.width, "height": p.height })),
        "resampling": pipeline.map(|p| p.converter.resampling),
        "bitrate": tier.bitrate,
        "maxBitrate": tier.max_bitrate,
        "toneMap": pipeline.map(|p| p.tone_map),
        "framesEncoded": pipeline.map(|p| p.video_packets),
        "fps": pipeline.map(|p| (p.measured_fps() * 10.0).round() / 10.0),
        "missedTicks": pipeline.map(|p| p.ticker.missed),
        "framesDropped": pipeline.map(|p| p.encoder.dropped),
        "captureFramesIn": pipeline.map(|p| p.capture.frames_in),
        "poolRebuilds": pipeline.map(|p| p.capture.recreates),
        "segmentsWritten": pipeline.and_then(|p| p.ring.as_ref()).map(|r| r.segments_written),
        "sinceProgressMs": pipeline.map(|p| (p.since_progress() * 1000.0).round()),
        "desktopAudio": config.audio.desktop,
        // Which output the desktop leg is following right now, and why it has none when it has
        // none. It moves with Windows' default, so the panel says where it went.
        "deskDevice": pipeline.and_then(|p| p.audio.as_ref()).map(|m| m.desk_name.clone()),
        "deskError": pipeline.and_then(|p| p.audio.as_ref()).and_then(|m| m.desk_error.clone()),
        "micWanted": config.audio.mic,
        // Three separate facts, because "no voice in my clip" has three different causes and the
        // settings panel should be able to tell them apart: not asked for, asked for and running,
        // asked for and refused.
        "micActive": pipeline.map(|p| p.audio.as_ref().is_some_and(|m| m.mic_active())),
        "micDevice": pipeline.and_then(|p| p.audio.as_ref()).map(|m| m.mic_name.clone()),
        "micError": pipeline.and_then(|p| p.audio.as_ref()).and_then(|m| m.mic_error.clone()),
        "micDropouts": pipeline.and_then(|p| p.audio.as_ref()).map(|m| m.mic_dropouts),
        // Whether the two sources together are actually reaching full scale. "The voice sounds
        // rough" is a different bug depending on the answer, and guessing at it from outside cost
        // an afternoon.
        "micPadded": pipeline.and_then(|p| p.audio.as_ref()).map(|m| m.mic_padded()),
        "micDropped": pipeline.and_then(|p| p.audio.as_ref()).map(|m| m.mic_dropped()),
        // What the microphone's own clock is doing, and how hard the rate matching is working to
        // agree with it. Silence invented to cover the difference is what a crackling voice was.
        "micFilled": pipeline.and_then(|p| p.audio.as_ref()).map(|m| m.mic_stats.filled_frames),
        "micClockPpm": pipeline.and_then(|p| p.audio.as_ref()).map(|m| m.mic_clock_ppm()),
        "micRatePpm": pipeline.and_then(|p| p.audio.as_ref()).map(|m| m.mic_rate_ppm()),
        // The desktop leg's equivalents. A render endpoint that keeps time reads 0 on all three;
        // one that does not — a USB or wireless headset, a virtual mixer — is what crackling game
        // audio was, and these are how to tell it from anything else on somebody else's machine.
        "deskFilled": pipeline.and_then(|p| p.audio.as_ref()).map(|m| m.desk_stats.filled_frames),
        "deskClockPpm": pipeline.and_then(|p| p.audio.as_ref()).map(|m| m.desk_clock_ppm()),
        "deskRatePpm": pipeline.and_then(|p| p.audio.as_ref()).map(|m| m.desk_rate_ppm()),
        "micGainDb": config.audio.mic_gain_db,
        "noiseActive": pipeline.and_then(|p| p.audio.as_ref()).map(|m| m.noise_active()),
        "noiseError": pipeline.and_then(|p| p.audio.as_ref()).and_then(|m| m.noise_error.clone()),
        "micPeakDb": pipeline.and_then(|p| p.audio.as_ref()).and_then(|m| m.mic_peak_db()).map(|v| (v * 10.0).round() / 10.0),
        "limitedFrames": pipeline.and_then(|p| p.audio.as_ref()).map(|m| m.limiter.limited_frames),
        "wouldClipFrames": pipeline.and_then(|p| p.audio.as_ref()).map(|m| m.limiter.would_clip_frames),
        "minGain": pipeline.and_then(|p| p.audio.as_ref()).map(|m| (m.limiter.min_gain * 1000.0).round() / 1000.0),
        "audioOffsetMs": pipeline.and_then(|p| p.audio_offset_ms()).map(|v| (v * 10.0).round() / 10.0),
        "audioStartMs": pipeline.and_then(|p| p.audio_start_ms()).map(|v| (v * 10.0).round() / 10.0),
        "foreground": front.process,
        "isGame": front.is_game,
        "worthRecording": front.worth_recording,
        "reason": front.reason,
        "gpuPercent": (front.gpu_percent * 10.0).round() / 10.0,
        "category": front.category(),
        "gameDetection": config.game_detection,
        "measuresGpu": watcher.measures_gpu(),
        "hotkey": config.hotkey,
        // A hotkey that is not wanted is not failing; only a refused registration is.
        "hotkeyOk": !matches!(save_key, Some(Err(_))),
        "recordEnabled": config.record.enabled,
        "recordHotkey": config.record.hotkey,
        "recordHotkeyOk": !matches!(record_key, Some(Err(_))),
        "record": record,
        "altHotkey": config.alt_hotkey,
        "recordAltHotkey": config.record.alt_hotkey,
        "alt": alt,
    })
}
