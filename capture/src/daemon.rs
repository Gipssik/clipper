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
    /// Stop feeding the pipeline this far in, for ten seconds, without telling it.
    ///
    /// This exists for the same reason `--no-keepalive` does: the watchdog's whole claim is that a
    /// pipeline which quietly stops producing gets noticed and rebuilt, and a claim that cannot be
    /// made to happen on demand is not a claim, it is a hope. Zero means never.
    pub stall_after: f64,
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
    ring: ring::Ring,
    ticker: clock::Ticker,
    epoch_hns: i64,
    audio_base: Option<i64>,
    freq: i64,
    width: u32,
    height: u32,
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
    fn start(config: &Config, monitor: &monitors::Monitor) -> crate::Fallible<Self> {
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

        let ring = ring::Ring::new(
            &config.segment_dir(),
            config::GOP_SECONDS as f64,
            config.buffer_seconds,
            audio.is_some(),
        )?;

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

    fn tick(&mut self) -> crate::Fallible<()> {
        let scheduled = self.ticker.wait();

        self.capture.pump(&self.gpu)?;
        if let Some(texture) = self.capture.latest() {
            self.converter.convert(&self.gpu, texture)?;
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
                    self.ring.push_audio(packet.data, pts);
                }
            }
        }

        for packet in self.encoder.take() {
            self.video_packets += 1;
            let pts = self.to_90k(packet.pts_hns);
            self.ring.push_video(&packet.data, pts, packet.keyframe)?;
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
        None
    }

    fn finish(&mut self) {
        if let Ok(packets) = self.encoder.finish() {
            for packet in packets {
                let pts = self.to_90k(packet.pts_hns);
                let _ = self.ring.push_video(&packet.data, pts, packet.keyframe);
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
                        let pts = self.to_90k(packet.pts_hns);
                        self.ring.push_audio(packet.data, pts);
                    }
                }
            }
        }
        if let Some(enc) = &mut self.aac {
            if let Ok(packets) = enc.finish() {
                for packet in packets {
                    let pts = self.to_90k(packet.pts_hns);
                    self.ring.push_audio(packet.data, pts);
                }
            }
        }
        let _ = self.ring.finish();
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

    let mut key = hotkey::Hotkey::register(&config.hotkey);
    if let Err(e) = &key {
        crate::lifecycle::log(&format!("hotkey unavailable: {}", e.message()));
        emit(serde_json::json!({ "event": "error", "message": format!("hotkey: {}", e.message()) }));
    }

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
                    "heartbeat: {:.0}s recorded {} frames at {:.1} fps, {} dropped, {} missed ticks, {} segments, audio {:+.0} ms",
                    elapsed,
                    p.video_packets,
                    p.measured_fps(),
                    p.encoder.dropped,
                    p.ticker.missed,
                    p.ring.segments_written,
                    p.audio_offset_ms().unwrap_or(0.0)
                ));
            }
        }

        // ── what should be running ────────────────────────────────────────────────
        if elapsed * 1000.0 - last_foreground_check >= FOREGROUND_POLL_MS {
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
                        if let Some(mut p) = pipeline.take() {
                            p.finish();
                            collect(&mut totals, &p);
                        }
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
                    if let Some(mut p) = pipeline.take() {
                        p.finish();
                        collect(&mut totals, &p);
                        emit(serde_json::json!({ "event": "state", "recording": false }));
                    }
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
                            " after {} frames at {:.1} fps, {} dropped, {} missed, {} capture frames in, {} empty polls, {} pool rebuilds, {} segments",
                            p.video_packets,
                            p.measured_fps(),
                            p.encoder.dropped,
                            p.ticker.missed,
                            p.capture.frames_in,
                            p.capture.empty_polls,
                            p.capture.recreates,
                            p.ring.segments_written
                        )
                    })
                    .unwrap_or_default();
                crate::lifecycle::log(&format!("rebuilding: {reason}{detail}"));
                emit(serde_json::json!({ "event": "rebuilding", "reason": reason }));
                if let Some(mut p) = pipeline.take() {
                    p.finish();
                    collect(&mut totals, &p);
                }
            }
        }

        // `worth_recording`, not `is_game`: a clip filed in the wrong folder can be moved, and a
        // clip that was never recorded cannot.
        let should_record = config.enabled
            && display_present
            && (config.record_mode != "game" || front.worth_recording);

        if should_record && pipeline.is_none() {
            match Pipeline::start(&config, &monitor) {
                Ok(p) => {
                    crate::lifecycle::log(&format!(
                        "recording {}x{} from {} ({}, {}-{} Mbps, {}{}, {} candidate(s) rejected first)",
                        p.width,
                        p.height,
                        monitor.friendly,
                        p.encoder_name,
                        p.bitrate / 1_000_000,
                        p.max_bitrate / 1_000_000,
                        p.encoder.applied.join(" "),
                        if p.converter.resampling { " resampled" } else { "" },
                        p.encoder.rejected
                    ));
                    emit(serde_json::json!({ "event": "state", "recording": true }));
                    last_error.clear();
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
        } else if !should_record && pipeline.is_some() {
            if let Some(mut p) = pipeline.take() {
                p.finish();
                collect(&mut totals, &p);
            }
            crate::lifecycle::log("stopped recording (nothing in focus worth keeping)");
            emit(serde_json::json!({ "event": "state", "recording": false }));
        }

        // ── one frame, or idle ────────────────────────────────────────────────────
        // The deliberate stall. Everything else carries on exactly as it would: the loop runs, the
        // daemon answers, nothing errors — only the frames stop.
        let faking_a_stall = options.stall_after > 0.0
            && elapsed >= options.stall_after
            && elapsed < options.stall_after + 10.0;
        if faking_a_stall {
            std::thread::sleep(std::time::Duration::from_millis(16));
        }

        match &mut pipeline {
            Some(_) if faking_a_stall => {}
            Some(p) => match p.tick() {
                Ok(()) => p.errors = 0,
                Err(e) => {
                    p.errors += 1;
                    if p.errors >= TICK_ERROR_LIMIT {
                        crate::lifecycle::log(&format!(
                            "pipeline failed {TICK_ERROR_LIMIT} ticks running ({e}); rebuilding"
                        ));
                        emit(serde_json::json!({ "event": "rebuilding", "reason": e.to_string() }));
                        if let Some(mut p) = pipeline.take() {
                            p.finish();
                            collect(&mut totals, &p);
                        }
                    }
                }
            },
            None => std::thread::sleep(std::time::Duration::from_millis(200)),
        }

        // ── saving ────────────────────────────────────────────────────────────────
        let fired = key.as_ref().map(|k| k.taken()).unwrap_or(0);
        let auto = options.save_after > 0.0 && !save_fired && elapsed >= options.save_after;
        if fired > 0 || auto {
            save_fired = true;
            match &mut pipeline {
                Some(p) => {
                    let out = save_path(&config, &front);
                    crate::lifecycle::log(&format!("saving {}", out.display()));
                    saves.push(p.ring.save(
                        (config.buffer_seconds * 1000.0) as u64,
                        out,
                        options.ffmpeg.clone(),
                    ));
                }
                None => {
                    emit(serde_json::json!({
                        "event": "error",
                        "message": "nothing is being recorded right now",
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

        // ── control ───────────────────────────────────────────────────────────────
        while let Some(command) = control.as_ref().and_then(|c| c.try_recv()) {
            if !matches!(command, Command::Status) {
                crate::lifecycle::log(&format!("command: {command:?}"));
            }
            match command {
                Command::Quit => quit = true,
                Command::Save => {
                    if let Some(p) = &mut pipeline {
                        saves.push(p.ring.save(
                            (config.buffer_seconds * 1000.0) as u64,
                            save_path(&config, &front),
                            options.ffmpeg.clone(),
                        ));
                    } else {
                        emit(serde_json::json!({
                            "event": "error",
                            "message": "nothing is being recorded right now",
                        }));
                    }
                }
                Command::Status => emit(status(
                    &config,
                    shown(&monitor, &selector, display_present),
                    &panel,
                    display_present,
                    pipeline.as_ref(),
                    &front,
                    &watcher,
                    key.is_ok(),
                )),
                Command::Reload => {
                    let Some(path) = &options.config_path else { continue };
                    let next = Config::load(path);
                    let restart = config.needs_restart(&next);

                    if config.hotkey != next.hotkey {
                        drop(key);
                        key = hotkey::Hotkey::register(&next.hotkey);
                        if let Err(e) = &key {
                            emit(serde_json::json!({
                                "event": "error",
                                "message": format!("hotkey: {}", e.message()),
                            }));
                        }
                    }
                    if restart {
                        if let Some(mut p) = pipeline.take() {
                            p.finish();
                            collect(&mut totals, &p);
                        }
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
                        key.is_ok(),
                    ));
                }
                // The settings panel cannot capture Alt+F-key itself — Windows eats those above
                // every layer Electron can reach — so it asks the daemon, which can install a
                // low-level hook. See hotkey.rs.
                Command::Listen => {
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
    }

    if let Some(mut p) = pipeline.take() {
        p.finish();
        collect(&mut totals, &p);
    }

    // A save started near the end is a background copy and has to be allowed to finish rather than
    // be killed by the daemon shutting down.
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

fn collect(totals: &mut (u64, u64, u64, u64), p: &Pipeline) {
    totals.0 += p.video_packets;
    totals.1 += p.audio_packets;
    totals.2 += p.ring.segments_written;
    totals.3 += p.ring.bytes_written;
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
    dir.join(crate::timestamp_name())
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
    hotkey_ok: bool,
) -> serde_json::Value {
    let tier = config.tier();
    serde_json::json!({
        "event": "status",
        "recording": pipeline.is_some(),
        "enabled": config.enabled,
        "recordMode": config.record_mode,
        "bufferedMs": pipeline.map(|p| p.ring.buffered_90k() * 1000 / ring::HZ).unwrap_or(0),
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
        "segmentsWritten": pipeline.map(|p| p.ring.segments_written),
        "sinceProgressMs": pipeline.map(|p| (p.since_progress() * 1000.0).round()),
        "desktopAudio": config.audio.desktop,
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
        "hotkeyOk": hotkey_ok,
    })
}
