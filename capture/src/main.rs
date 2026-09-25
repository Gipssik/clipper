//! clipper-capture — instant replay ring buffer recorder.
//!
//! See DESIGN.md next to this crate for the whole pipeline. Today this binary only does the first
//! stage: enumerate monitors, and capture one at a fixed rate so the path can be verified against
//! real pixels before anything is built on top of it.

// `status` in daemon.rs is one `json!` literal with forty-odd keys, and the macro recurses once per
// key. The default limit of 128 is not a statement about anything; raising it is cheaper than
// splitting a flat object into nested ones nobody wants to read.
#![recursion_limit = "256"]

mod aac;
mod audio;
mod capture;
mod clock;
mod config;
mod daemon;
mod foreground;
mod gamepad;
mod gamename;
mod gpuload;
mod ipc;
mod lifecycle;
mod convert;
mod d3d;
mod denoise;
mod desktop;
mod display;
mod encoder;
mod hotkey;
mod record;
mod ring;
mod ts;
mod monitors;
mod procloop;

#[cfg(feature = "dump")]
mod dump;

use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

pub type Fallible<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn main() {
    if let Err(e) = run() {
        eprintln!("clipper-capture: {e}");
        std::process::exit(1);
    }
}

fn run() -> Fallible<()> {
    // Free-threaded, because the capture frame pool and the encoder both run off our thread.
    unsafe { RoInitialize(RO_INIT_MULTITHREADED)? };

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("monitors") => cmd_monitors(),
        Some("names") => cmd_names(),
        Some("inputs") => cmd_inputs(),
        Some("pads") => cmd_pads(&args[1..]),
        #[cfg(feature = "dump")]
        Some("dump") => cmd_dump(&args[1..]),
        Some("encode") => cmd_encode(&args[1..]),
        Some("audio") => cmd_audio(&args[1..]),
        Some("mictest") => cmd_mictest(&args[1..]),
        Some("record") => cmd_record(&args[1..]),
        Some("daemon") => cmd_daemon(&args[1..]),
        _ => {
            usage();
            Ok(())
        }
    }
}

fn usage() {
    eprintln!("clipper-capture <command>\n");
    eprintln!("  monitors                 list displays as JSON");
    eprintln!("  names                    the folder name every game Windows knows would get");
    eprintln!("  inputs                   list microphones as JSON");
    eprintln!("  pads [--watch <s>]       list game controllers; --watch prints presses for s seconds");
    #[cfg(feature = "dump")]
    {
        eprintln!("  dump [options]           capture at a fixed rate and report what happened\n");
        eprintln!("    --monitor <name>       friendly name, \\\\.\\DISPLAY1, or index (default: primary)");
        eprintln!("    --fps <n>              tick rate (default 60)");
        eprintln!("    --frames <n>           ticks to run (default 180)");
        eprintln!("    --save <n>             PNGs to write, spread evenly (default 3)");
        eprintln!("    --out <dir>            where the PNGs go (default %TEMP%\\clipper-capture)");
        eprintln!("    --hdr                  capture FP16 scRGB instead of 8-bit BGRA");
        eprintln!("    --height <n>           scale + tone map + NV12 pack to this height");
        eprintln!("    --cursor               include the mouse cursor");
    }
    eprintln!("
  encode [options]         capture, convert and encode to a raw .h264
");
    eprintln!("    --monitor <name>       as above (default: primary)");
    eprintln!("    --seconds <n>          how long to record (default 10)");
    eprintln!("    --height <n>           encode height (default 1080)");
    eprintln!("    --fps <n>              frame rate (default 60)");
    eprintln!("    --bitrate <bps>        target bitrate (default 30000000)");
    eprintln!("    --gop <seconds>        IDR interval (default 1.0)");
    eprintln!(r"    --out <file>           output path (default %TEMP%\clipper-capture\out.h264)");
    eprintln!("    --sdr                  force 8-bit capture even on an HDR display");
    eprintln!("
  audio [options]          capture desktop audio to a raw .aac
");
    eprintln!("    --seconds <n>          how long to record (default 10)");
    eprintln!("    --bitrate <bps>        AAC bitrate (default 192000)");
    eprintln!("    --tone <hz>            emit a sine through the keep-alive stream and record it");
    eprintln!("    --no-keepalive         drop the silence stream, to show what it prevents");
    eprintln!("    --mic                  mix the default microphone in");
    eprintln!("    --mic-device <id>      mix that microphone in; see the `inputs` command");
    eprintln!("    --no-desktop           microphone only");
    eprintln!("    --mic-gain <dB>        boost the microphone before the mix (default 0)");
    eprintln!("    --noise <0-100>        suppress noise on the microphone at that strength");
    eprintln!("    --wav <file>           also dump the mixed PCM, before AAC touches it");
    eprintln!("    --out <file>           output path");
    eprintln!("\n  mictest [options]        record the microphone as a clip would hear it, to a .wav\n");
    eprintln!("    --mic-device <id>      that microphone (default: Windows' default)");
    eprintln!("    --mic-gain <dB>        boost, as in the recorder");
    eprintln!("    --noise <0-100>        noise suppression at that strength");
    eprintln!("    --seconds <n>          stop after n seconds (default 30); a line on stdin stops sooner");
    eprintln!(r"    --out <file>           default %TEMP%\clipper-capture\mictest.wav");
    eprintln!("\n  record [options]         run the replay buffer; hotkey saves a clip\n");
    eprintln!("    --monitor <name>       as above (default: primary)");
    eprintln!("    --buffer <seconds>     how much to keep (default 60)");
    eprintln!("    --segment <seconds>    segment length (default 2)");
    eprintln!("    --hotkey <combo>       default Ctrl+Alt+F12");
    eprintln!("    --out <dir>            where clips land");
    eprintln!("    --seconds <n>          stop after n seconds (default: run until Ctrl+C)");
    eprintln!("    --save-after <n>       fire a save n seconds in, for testing without a keyboard");
    eprintln!("    --record-after <n>     start a recording n seconds in");
    eprintln!("    --record-for <n>       and stop it n seconds later (default: at exit)");
    eprintln!("    --record-hotkey <c>    default Alt+F9");
    eprintln!("    --no-replay            no ring buffer; the pipeline runs only while recording");
    eprintln!("    --quality <tier>       low | medium | high | ultra | native (default high)");
    eprintln!("    --bitrate <bps>        override the tier's mean bitrate");
    eprintln!("    --max-bitrate <bps>    override the tier's peak bitrate");
    eprintln!("    --mode <m>             always | game (default always)");
    eprintln!("    --no-audio             video only");
    eprintln!("    --no-mic               desktop audio only");
    eprintln!("    --noise <0-100>        suppress noise on the microphone at that strength");
    eprintln!("    --mic-device <id>      record that microphone; see the `inputs` command");
    eprintln!("    --ipc                  also serve the control pipe");
    eprintln!("\n  daemon [options]        what Clipper launches\n");
    eprintln!("    --config <file>        default %APPDATA%\\clipper\\capture.json");
    eprintln!("    --parent-pid <pid>     exit when that process does");
}

fn cmd_inputs() -> Fallible<()> {
    println!("{}", serde_json::to_string_pretty(&audio::input_devices())?);
    Ok(())
}

fn cmd_monitors() -> Fallible<()> {
    let list = monitors::list()?;
    let infos: Vec<monitors::MonitorInfo> = list.iter().map(Into::into).collect();
    println!("{}", serde_json::to_string_pretty(&infos)?);
    Ok(())
}

/// Every executable Game Bar has on file as a game, and the folder its clips would land in.
///
/// This is how the naming layers are checked against a real library rather than against a guess:
/// the registry holds the paths, and the answer is either recognisably the game or it is not.
fn cmd_names() -> Fallible<()> {
    let mut paths = foreground::registered_paths();
    paths.sort();
    paths.dedup();
    for path in &paths {
        let stem = std::path::Path::new(path)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        println!("{:<34} <- {}", gamename::resolve(path), stem);
    }
    eprintln!("\n{} registered", paths.len());
    Ok(())
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn present(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn number<T: std::str::FromStr>(args: &[String], name: &str, default: T) -> T {
    flag(args, name).and_then(|v| v.parse().ok()).unwrap_or(default)
}

#[cfg(feature = "dump")]
fn cmd_dump(args: &[String]) -> Fallible<()> {
    use std::path::PathBuf;

    let fps: u32 = number(args, "--fps", 60);
    let frames: u32 = number(args, "--frames", 180);
    let saves: u32 = number(args, "--save", 3);
    let out = flag(args, "--out")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("clipper-capture"));
    let hdr = present(args, "--hdr");
    let cursor = present(args, "--cursor");
    let out_height: u32 = number(args, "--height", 0);

    let all = monitors::list()?;
    let monitor = monitors::resolve(&all, flag(args, "--monitor").unwrap_or(""))
        .ok_or("no monitor matched --monitor")?;

    let format = if hdr {
        DirectXPixelFormat::R16G16B16A16Float
    } else {
        DirectXPixelFormat::B8G8R8A8UIntNormalized
    };

    let gpu = d3d::create()?;
    let mut cap = capture::Capture::start(&gpu, monitor.handle, format)?;

    // The tone map is driven by what the display is actually doing, not by a flag: with HDR off,
    // scRGB is simply a linear version of ordinary sRGB content and the curve must stay out of it.
    let panel = display::probe(monitor.handle, &monitor.device);
    let tone_map = hdr && panel.enabled;
    let mut converter = if out_height > 0 {
        let src = cap.size();
        let width = (src.Width as u32 * out_height / src.Height.max(1) as u32 + 1) & !1;
        Some(convert::Converter::new(
            &gpu,
            src.Width as u32,
            src.Height as u32,
            width,
            out_height,
            hdr,
            if tone_map { panel.white_scale() } else { 1.0 },
            if tone_map { panel.peak() } else { 1.0 },
        )?)
    } else {
        None
    };
    if cursor {
        // The session hides the cursor by default; --cursor is only here so the dump can prove
        // the toggle actually does something.
        cap.set_cursor(true);
    }

    // Which ticks get read back. Readback stalls the GPU, so these ticks are excluded from the
    // pacing statistics — otherwise we would be measuring the diagnostics, not the capture.
    let save_at: Vec<u32> = if saves == 0 || frames == 0 {
        Vec::new()
    } else if saves == 1 {
        vec![frames / 2]
    } else {
        (0..saves)
            .map(|i| (i * (frames.saturating_sub(1))) / (saves - 1))
            .collect()
    };

    let freq = clock::qpc_frequency();
    let mut ticker = clock::Ticker::new(fps)?;
    let mut jitter_ms: Vec<f64> = Vec::with_capacity(frames as usize);
    let mut pump_ms: Vec<f64> = Vec::with_capacity(frames as usize);
    let mut convert_ms: Vec<f64> = Vec::with_capacity(frames as usize);
    let mut images: Vec<(u32, dump::Image)> = Vec::new();
    let mut new_frames = 0u32;

    let started = clock::qpc_now();
    for tick in 0..frames {
        let scheduled = ticker.wait();
        let woke = clock::qpc_now();

        let before = clock::qpc_now();
        let fresh = cap.pump(&gpu)?;
        let after = clock::qpc_now();
        if fresh {
            new_frames += 1;
        }

        let converted = match (&mut converter, cap.latest()) {
            (Some(conv), Some(texture)) => {
                let t0 = clock::qpc_now();
                conv.convert(&gpu, texture)?;
                convert_ms.push(clock::qpc_to_ms(clock::qpc_now() - t0, freq));
                true
            }
            _ => false,
        };

        if save_at.contains(&tick) {
            if converted {
                let nv12 = converter.as_ref().unwrap().nv12().clone();
                images.push((tick, dump::read_back_nv12(&gpu, &nv12)?));
            } else if let Some(texture) = cap.latest() {
                images.push((tick, dump::read_back(&gpu, texture)?));
            }
            // A readback stalls the GPU, so this tick tells us nothing about pacing.
            convert_ms.pop();
        } else {
            jitter_ms.push(clock::qpc_to_ms(woke - scheduled, freq));
            pump_ms.push(clock::qpc_to_ms(after - before, freq));
        }
    }
    let elapsed_ms = clock::qpc_to_ms(clock::qpc_now() - started, freq);

    std::fs::create_dir_all(&out)?;
    let mut written = Vec::new();
    for (tick, image) in &images {
        let path = out.join(format!("frame_{tick:04}.png"));
        dump::write_png(&path, image)?;
        written.push(path.to_string_lossy().to_string());
    }

    let size = cap.size();
    let report = serde_json::json!({
        "monitor": monitors::MonitorInfo::from(&monitor),
        "captureSize": { "width": size.Width, "height": size.Height },
        "format": if hdr { "R16G16B16A16Float" } else { "B8G8R8A8UIntNormalized" },
        "requestedFps": fps,
        "ticks": frames,
        "elapsedMs": round2(elapsed_ms),
        "effectiveFps": round2(frames as f64 * 1000.0 / elapsed_ms.max(0.001)),
        "newFrames": new_frames,
        "repeatedFrames": frames - new_frames,
        "missedTicks": ticker.missed,
        "outputSize": converter.as_ref().map(|c| { let (w, h) = c.size(); serde_json::json!({ "width": w, "height": h }) }),
        "toneMap": if tone_map { "mobius" } else { "none" },
        "nv12RenderTarget": convert::supports_nv12_render_target(&gpu),
        "convertMs": stats(&mut convert_ms),
        "tickJitterMs": stats(&mut jitter_ms),
        "pumpMs": stats(&mut pump_ms),
        "saved": written,
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn cmd_encode(args: &[String]) -> Fallible<()> {
    use std::io::Write;
    use std::path::PathBuf;

    let fps: u32 = number(args, "--fps", 60);
    let seconds: f64 = number(args, "--seconds", 10.0);
    let out_height: u32 = number(args, "--height", 1080);
    let bitrate: u32 = number(args, "--bitrate", 30_000_000);
    let max_bitrate: u32 = number(args, "--max-bitrate", 0);
    let gop_seconds: f32 = number(args, "--gop", config::GOP_SECONDS);
    let quality_vs_speed: u32 = number(args, "--quality", config::QUALITY_VS_SPEED);
    let out = flag(args, "--out").map(PathBuf::from).unwrap_or_else(|| {
        std::env::temp_dir()
            .join("clipper-capture")
            .join("out.h264")
    });

    let all = monitors::list()?;
    let monitor = monitors::resolve(&all, flag(args, "--monitor").unwrap_or(""))
        .ok_or("no monitor matched --monitor")?;
    let panel = display::probe(monitor.handle, &monitor.device);

    // This is `toneMap: "auto"` in miniature: the display's current colour space decides both the
    // capture format and whether the tone map runs. Nothing is asked of the user.
    let tone_map = panel.enabled && !present(args, "--sdr");
    let format = if tone_map {
        DirectXPixelFormat::R16G16B16A16Float
    } else {
        DirectXPixelFormat::B8G8R8A8UIntNormalized
    };

    let gpu = d3d::create()?;
    let mut cap = capture::Capture::start(&gpu, monitor.handle, format)?;

    let src = cap.size();
    let width = (src.Width as u32 * out_height / src.Height.max(1) as u32 + 1) & !1;
    let mut converter = convert::Converter::new(
        &gpu,
        src.Width as u32,
        src.Height as u32,
        width,
        out_height,
        tone_map,
        panel.white_scale(),
        if tone_map { panel.peak() } else { 1.0 },
    )?;
    let (width, height) = converter.size();

    let mut enc = encoder::Encoder::new(
        &gpu,
        &encoder::EncoderConfig {
            width,
            height,
            fps,
            bitrate,
            max_bitrate: max_bitrate.max(bitrate),
            gop_seconds,
            quality_vs_speed,
        },
    )?;

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::io::BufWriter::new(std::fs::File::create(&out)?);

    let freq = clock::qpc_frequency();
    let frames = (seconds * fps as f64).round() as u32;
    let mut ticker = clock::Ticker::new(fps)?;
    let mut submit_ms: Vec<f64> = Vec::with_capacity(frames as usize);
    let mut keyframe_ticks: Vec<i64> = Vec::new();
    let mut bytes = 0usize;
    let mut packets = 0usize;
    let mut new_frames = 0u32;

    let started = clock::qpc_now();
    for tick in 0..frames {
        ticker.wait();
        let fresh = cap.pump(&gpu)?;
        if fresh {
            new_frames += 1;
        }
        let Some(texture) = cap.latest() else { continue };

        let t0 = clock::qpc_now();
        if fresh {
            converter.convert(&gpu, texture)?;
        } else {
            converter.repeat(&gpu);
        }
        // Constant frame rate, so the timestamp is the tick's position on the timeline and not
        // whenever this loop happened to get here.
        let pts = tick as i64 * 10_000_000 / fps as i64;
        enc.submit(converter.nv12(), pts)?;
        submit_ms.push(clock::qpc_to_ms(clock::qpc_now() - t0, freq));

        for packet in enc.take() {
            if packet.keyframe {
                keyframe_ticks.push(packet.pts_hns * fps as i64 / 10_000_000);
            }
            bytes += packet.data.len();
            packets += 1;
            file.write_all(&packet.data)?;
        }
    }
    let elapsed_ms = clock::qpc_to_ms(clock::qpc_now() - started, freq);

    for packet in enc.finish()? {
        if packet.keyframe {
            keyframe_ticks.push(packet.pts_hns * fps as i64 / 10_000_000);
        }
        bytes += packet.data.len();
        packets += 1;
        file.write_all(&packet.data)?;
    }
    file.flush()?;

    let gaps: Vec<i64> = keyframe_ticks.windows(2).map(|w| w[1] - w[0]).collect();
    let report = serde_json::json!({
        "encoder": enc.name,
        "hardware": enc.hardware,
        "codecSettingsApplied": enc.applied,
        "captureSize": { "width": src.Width, "height": src.Height },
        "encodeSize": { "width": width, "height": height },
        "toneMap": if tone_map { "mobius" } else { "none" },
        "fps": fps,
        "targetBitrate": bitrate,
        "ticks": frames,
        "newFrames": new_frames,
        "missedTicks": ticker.missed,
        "elapsedMs": round2(elapsed_ms),
        "packets": packets,
        "bytes": bytes,
        "measuredBitrate": round2(bytes as f64 * 8.0 / (elapsed_ms / 1000.0)),
        "keyframes": keyframe_ticks.len(),
        "keyframeGapFrames": gaps,
        "submitMs": stats(&mut submit_ms),
        "out": out.to_string_lossy(),
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

/// 16-bit PCM in a WAV container, written by hand — the crate has no audio-file dependency and
/// this header is 44 bytes.
fn write_wav(path: &std::path::Path, pcm: &[i16], rate: u32, channels: u16) -> Fallible<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = (pcm.len() * 2) as u32;
    let block_align = channels * 2;
    let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);
    file.write_all(b"RIFF")?;
    file.write_all(&(36 + bytes).to_le_bytes())?;
    file.write_all(b"WAVEfmt ")?;
    file.write_all(&16u32.to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?; // PCM
    file.write_all(&channels.to_le_bytes())?;
    file.write_all(&rate.to_le_bytes())?;
    file.write_all(&(rate * block_align as u32).to_le_bytes())?;
    file.write_all(&block_align.to_le_bytes())?;
    file.write_all(&16u16.to_le_bytes())?;
    file.write_all(b"data")?;
    file.write_all(&bytes.to_le_bytes())?;
    for sample in pcm {
        file.write_all(&sample.to_le_bytes())?;
    }
    file.flush()?;
    Ok(())
}

/// The microphone as a clip will hear it, recorded to a file somebody can listen to.
///
/// This is the recorder's own `Mixer` with the desktop leg switched off, not a second audio path
/// written to look like it: the boost, the suppressor and the limiter are the same code, in the
/// same order, so what the settings panel plays back is what a saved clip will contain. It runs
/// as its own process rather than as a daemon command so the test works with instant replay off,
/// and it shares the microphone with a daemon that is running — WASAPI shared mode is built for
/// exactly that.
///
/// Stops at the first line on stdin, or at end of input, which is also what happens when the app
/// that started it goes away. A level line goes to stdout ten times a second while it runs, and
/// one line saying where the file is when it is done.
fn cmd_mictest(args: &[String]) -> Fallible<()> {
    use std::io::{BufRead, Write};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let seconds: f64 = number(args, "--seconds", 30.0);
    let out = flag(args, "--out").map(PathBuf::from).unwrap_or_else(|| {
        std::env::temp_dir().join("clipper-capture").join("mictest.wav")
    });
    let say = |value: serde_json::Value| {
        let mut stdout = std::io::stdout().lock();
        let _ = writeln!(stdout, "{value}");
        let _ = stdout.flush();
    };

    let mut mixer = audio::Mixer::start(audio::MixerConfig {
        desktop: false,
        mic: true,
        mic_device: flag(args, "--mic-device").unwrap_or(""),
        mic_gain_db: number(args, "--mic-gain", 0.0),
        noise_suppression: flag(args, "--noise").is_some(),
        noise_strength: number(args, "--noise", 70.0),
        tone_hz: None,
        keep_alive: false,
    })?;
    if let Some(error) = &mixer.mic_error {
        say(serde_json::json!({ "event": "error", "message": error }));
        return Ok(());
    }

    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut line = String::new();
            let _ = std::io::stdin().lock().read_line(&mut line);
            stop.store(true, Ordering::Relaxed);
        });
    }

    let freq = clock::qpc_frequency();
    let mut ticker = clock::Ticker::new(100)?;
    let started = clock::qpc_now();
    let mut pcm: Vec<i16> = Vec::new();
    let mut peak = 0i32;
    let mut window_peak = 0i32;
    let mut last_level = 0.0;
    let db = |p: i32| 20.0 * (p.max(1) as f64 / 32768.0).log10();

    loop {
        let elapsed = clock::qpc_to_ms(clock::qpc_now() - started, freq) / 1000.0;
        if stop.load(Ordering::Relaxed) || elapsed >= seconds {
            break;
        }
        ticker.wait();
        let before = pcm.len();
        if let Err(e) = mixer.poll(&mut pcm) {
            say(serde_json::json!({ "event": "error", "message": e.message().to_string() }));
            return Ok(());
        }
        let loudest = pcm[before..].iter().map(|&s| (s as i32).abs()).max().unwrap_or(0);
        window_peak = window_peak.max(loudest);
        if elapsed - last_level >= 0.1 {
            last_level = elapsed;
            say(serde_json::json!({
                "event": "level",
                "peakDb": (db(window_peak) * 10.0).round() / 10.0,
                "seconds": (elapsed * 10.0).round() / 10.0,
            }));
            peak = peak.max(window_peak);
            window_peak = 0;
        }
    }
    mixer.limiter.flush(&mut pcm);
    peak = peak.max(window_peak);

    write_wav(&out, &pcm, mixer.rate(), audio::OUT_CHANNELS as u16)?;
    say(serde_json::json!({
        "event": "done",
        "path": out.to_string_lossy(),
        "seconds": (pcm.len() / audio::OUT_CHANNELS) as f64 / mixer.rate() as f64,
        "peakDb": (db(peak) * 10.0).round() / 10.0,
        "device": mixer.mic_name,
        "noiseActive": mixer.noise_active(),
        "noiseError": mixer.noise_error,
    }));
    Ok(())
}

fn cmd_audio(args: &[String]) -> Fallible<()> {
    use std::io::Write;
    use std::path::PathBuf;

    let seconds: f64 = number(args, "--seconds", 10.0);
    let bitrate: u32 = number(args, "--bitrate", 192_000);
    let tone: Option<f32> = flag(args, "--tone").and_then(|v| v.parse().ok());
    let out = flag(args, "--out").map(PathBuf::from).unwrap_or_else(|| {
        std::env::temp_dir().join("clipper-capture").join("out.aac")
    });

    // --no-keepalive exists to prove the keep-alive stream is load-bearing rather than
    // decorative: without it, a quiet desktop stops delivering packets entirely.
    let mut loopback = audio::Mixer::start(audio::MixerConfig {
        desktop: !present(args, "--no-desktop"),
        mic: present(args, "--mic") || flag(args, "--mic-device").is_some(),
        mic_device: flag(args, "--mic-device").unwrap_or(""),
        mic_gain_db: number(args, "--mic-gain", 0.0),
        noise_suppression: flag(args, "--noise").is_some(),
        noise_strength: number(args, "--noise", 70.0),
        tone_hz: tone,
        keep_alive: !present(args, "--no-keepalive"),
    })?;
    loopback.keep_legs = flag(args, "--wav").is_some();
    let format = loopback.desktop_format().unwrap_or(audio::MixFormat {
        sample_rate: loopback.rate(),
        channels: audio::OUT_CHANNELS as u16,
        bits: 16,
        float: false,
    });
    let mut enc = aac::AacEncoder::new(loopback.rate(), audio::OUT_CHANNELS as u32, bitrate / 8)?;

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::io::BufWriter::new(std::fs::File::create(&out)?);

    let freq = clock::qpc_frequency();
    // 100 Hz is far more often than the endpoint needs, which is the point: the keep-alive render
    // buffer must never run dry, or the engine stops and loopback goes quiet.
    let mut ticker = clock::Ticker::new(100)?;
    let started = clock::qpc_now();
    let mut totals = audio::PollStats::default();
    let mut pcm_all: Vec<i16> = Vec::new();
    let mut packets = 0usize;
    let mut bytes = 0usize;
    let mut base_hns = None;

    while clock::qpc_to_ms(clock::qpc_now() - started, freq) < seconds * 1000.0 {
        ticker.wait();

        let mut pcm = Vec::new();
        let stats = loopback.poll(&mut pcm)?;
        totals.add(&stats);

        if pcm.is_empty() {
            continue;
        }
        if base_hns.is_none() {
            base_hns = loopback.first_hns;
        }
        pcm_all.extend_from_slice(&pcm);
        for packet in enc.submit(&pcm, base_hns.unwrap_or(0))? {
            bytes += packet.data.len();
            packets += 1;
            file.write_all(&packet.data)?;
        }
    }
    let elapsed_ms = clock::qpc_to_ms(clock::qpc_now() - started, freq);

    // Whatever the limiter is still holding as lookahead, at the gain it last settled on.
    let mut tail = Vec::new();
    loopback.limiter.flush(&mut tail);
    if !tail.is_empty() {
        pcm_all.extend_from_slice(&tail);
        for packet in enc.submit(&tail, base_hns.unwrap_or(0))? {
            bytes += packet.data.len();
            packets += 1;
            file.write_all(&packet.data)?;
        }
    }
    for packet in enc.finish()? {
        bytes += packet.data.len();
        packets += 1;
        file.write_all(&packet.data)?;
    }
    file.flush()?;

    // The mixed stream as it went into the encoder. A click that has been through AAC is a click
    // plus whatever AAC made of it, which is not what you want to be looking at when the question
    // is whether the mixer dropped a sample.
    if let Some(path) = flag(args, "--wav") {
        let path = std::path::Path::new(path);
        let rate = loopback.rate();
        write_wav(path, &pcm_all, rate, audio::OUT_CHANNELS as u16)?;
        // And each leg beside it. The sum is the only thing the encoder sees, so when the sum is
        // wrong the question is always "which of the two", and that is unanswerable from the sum.
        let beside = |suffix: &str| {
            let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            path.with_file_name(format!("{stem}-{suffix}.wav"))
        };
        write_wav(&beside("desktop"), &loopback.leg_desktop, rate, audio::OUT_CHANNELS as u16)?;
        write_wav(&beside("mic"), &loopback.leg_mic, rate, audio::OUT_CHANNELS as u16)?;
    }

    // The drift test, and the reason it is written this way. Comparing audio length against the
    // wall clock would mostly measure when this loop happened to start and stop — the endpoint is
    // already buffering before the timer begins. What matters is whether the device's sample clock
    // and QPC agree: if they do not, a long recording slides out of sync no matter how carefully
    // the first frame was aligned. So both sides come from the device's own packet timestamps.
    let emitted = (pcm_all.len() / audio::OUT_CHANNELS) as f64;
    let audio_ms = emitted * 1000.0 / loopback.rate() as f64;
    let device_span_ms = loopback
        .first_hns
        .map(|first| (loopback.last_packet_hns - first) as f64 / 10_000.0)
        .unwrap_or(0.0);

    let report = serde_json::json!({
        "mixFormat": format,
        "mixRate": loopback.rate(),
        "micActive": loopback.mic_active(),
        "micDevice": loopback.mic_name,
        "micError": loopback.mic_error,
        "micFrames": loopback.mic_frames,
        "micDropouts": loopback.mic_dropouts,
        "micOpen": loopback.mic_open,
        "micRatePpm": loopback.mic_rate_ppm(),
        "micClockPpm": loopback.mic_clock_ppm(),
        "micPeakDb": loopback.mic_peak_db().map(|v| round2(v as f64)),
        "deskClockPpm": loopback.desk_clock_ppm(),
        "deskRatePpm": loopback.desk_rate_ppm(),
        "deskFilled": loopback.desk_stats.filled_frames,
        "micStats": loopback.mic_stats,
        "micPadded": loopback.mic_padded(),
        "micDropped": loopback.mic_dropped(),
        "deskPadded": loopback.desk_padded(),
        "deskLead": loopback.desk_lead(),
        "deskDropped": loopback.desk_dropped(),
        "deskSplices": loopback.desk_splices(),
        "micLead": loopback.mic_lead(),
        "micPadEvents": loopback.mic_splices().0,
        "micDropEvents": loopback.mic_splices().1,
        "limitedFrames": loopback.limiter.limited_frames,
        "wouldClipFrames": loopback.limiter.would_clip_frames,
        "minGain": round4(loopback.limiter.min_gain as f64),
        "resampled": enc.resampled,
        "encodeRate": enc.out_rate(),
        "elapsedMs": round2(elapsed_ms),
        "audioMs": round2(audio_ms),
        "deviceSpanMs": round2(device_span_ms),
        "driftMs": round2(audio_ms - device_span_ms),
        "capturedFrames": totals.captured_frames,
        "filledFrames": totals.filled_frames,
        "silentPackets": totals.silent_packets,
        "discontinuities": totals.discontinuities,
        "rms": round4(rms(&pcm_all)),
        "zeroCrossHz": round2(zero_cross_hz(&pcm_all, loopback.rate())),
        "aacPackets": packets,
        "aacBytes": bytes,
        "out": out.to_string_lossy(),
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

/// Locates the bundled ffmpeg the way `getFfmpegPath()` does on the Electron side: near the
/// executable first, then whatever is on PATH.
fn ffmpeg_path(args: &[String]) -> std::path::PathBuf {
    use std::path::PathBuf;
    if let Some(explicit) = flag(args, "--ffmpeg") {
        return PathBuf::from(explicit);
    }
    if let Ok(exe) = std::env::current_exe() {
        for up in [1usize, 2, 3, 4] {
            let mut base = exe.clone();
            for _ in 0..up {
                base.pop();
            }
            let candidate = base.join("ffmpeg-bin").join("ffmpeg.exe");
            if candidate.exists() {
                return candidate;
            }
        }
    }
    PathBuf::from("ffmpeg")
}

/// `clip_2026-09-21_18-42-03.mp4` for a saved replay, `rec_…` for a recording.
pub fn timestamp_name(prefix: &str) -> String {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let t = unsafe { GetLocalTime() };
    format!(
        "{prefix}_{:04}-{:02}-{:02}_{:02}-{:02}-{:02}.mp4",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

fn config_from_args(args: &[String]) -> config::Config {
    let mut config = config::Config::default();
    config.buffer_seconds = number::<f64>(args, "--buffer", 60.0)
        .clamp(config::MIN_BUFFER_SECONDS, config::MAX_BUFFER_SECONDS);
    config.quality = flag(args, "--quality").unwrap_or("high").to_string();
    config.hotkey = flag(args, "--hotkey").unwrap_or("Ctrl+Alt+F12").to_string();
    config.record.enabled = true;
    config.record.hotkey = flag(args, "--record-hotkey").unwrap_or("Alt+F9").to_string();
    // Replay off leaves only the recording, which is how record-only is tested from here.
    config.enabled = !present(args, "--no-replay");
    // `--no-audio` means no audio, not "no desktop audio": the config default now turns the
    // microphone on, and a flag that left one of the two sources running would be a trap.
    config.audio.desktop = !present(args, "--no-audio");
    config.audio.mic = !present(args, "--no-audio") && !present(args, "--no-mic");
    config.audio.mic_device = flag(args, "--mic-device").unwrap_or("").to_string();
    config.audio.mic_gain_db = number(args, "--mic-gain", 0.0);
    config.audio.noise_suppression = flag(args, "--noise").is_some();
    config.audio.noise_strength = number(args, "--noise", 70.0);
    config.record_mode = flag(args, "--mode").unwrap_or("always").to_string();
    config.per_game_subfolder = present(args, "--per-game");
    config.tone_map = if present(args, "--sdr") { "off".into() } else { "auto".into() };
    config.monitor.friendly = flag(args, "--monitor").unwrap_or("").to_string();
    config.output_path = flag(args, "--out")
        .map(str::to_string)
        .unwrap_or_else(|| {
            std::env::temp_dir()
                .join("clipper-capture")
                .join("clips")
                .to_string_lossy()
                .to_string()
        });
    config.segment_dir = flag(args, "--segment-dir").map(str::to_string);
    config.bitrate_override = number(args, "--bitrate", 0);
    config.max_bitrate_override = number(args, "--max-bitrate", 0);
    config
}

/// Game controllers, and with `--watch` every button that goes down — the same reader the alt binds
/// use, so what this prints is exactly what a bind can be set to. Also reports how many HID reports
/// a second the devices send, which is what the reader costs while it runs.
fn cmd_pads(args: &[String]) -> Fallible<()> {
    println!("{}", serde_json::to_string_pretty(&gamepad::list())?);
    let seconds: f64 = number(args, "--watch", 0.0);
    if seconds <= 0.0 {
        return Ok(());
    }
    let pads = gamepad::Pads::start().ok_or("could not start the controller reader")?;
    let started = std::time::Instant::now();
    let mut last = std::time::Instant::now();
    let mut last_reports = 0u64;
    while started.elapsed().as_secs_f64() < seconds {
        for press in pads.poll() {
            println!("{}", serde_json::json!({ "press": press.spec() }));
        }
        if last.elapsed().as_secs_f64() >= 5.0 {
            let reports = gamepad::REPORTS.load(std::sync::atomic::Ordering::Relaxed);
            eprintln!("{:.0} reports/s", (reports - last_reports) as f64 / last.elapsed().as_secs_f64());
            last_reports = reports;
            last = std::time::Instant::now();
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    Ok(())
}

/// A front end for the same code the daemon runs, so what is tested here is what ships.
fn cmd_record(args: &[String]) -> Fallible<()> {
    let config = config_from_args(args);
    let report = daemon::run(
        config,
        daemon::Options {
            run_seconds: number(args, "--seconds", 0.0),
            save_after: number(args, "--save-after", 0.0),
            record_after: number(args, "--record-after", 0.0),
            record_for: number(args, "--record-for", 0.0),
            stall_after: number(args, "--stall-after", 0.0),
            ffmpeg: ffmpeg_path(args),
            config_path: None,
            ipc: present(args, "--ipc"),
        },
    )?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn cmd_daemon(args: &[String]) -> Fallible<()> {
    use std::path::PathBuf;

    // Two recorders would fight over one segment directory and double the cost for nothing.
    let Some(_singleton) = lifecycle::single_instance() else {
        lifecycle::log("another clipper-capture is already running; exiting");
        return Ok(());
    };

    // Exit when Clipper does. An orphaned recorder writing to disk forever is the worst bug
    // available here, so this is wired up before anything else starts.
    if let Some(pid) = flag(args, "--parent-pid").and_then(|v| v.parse::<u32>().ok()) {
        lifecycle::exit_with_parent(pid);
        // And leave Clipper out of the recording: the parent is its main process, and everything
        // that makes a sound — the renderer's replay-saved chime, a clip playing in the grid — is
        // under it.
        procloop::exclude_tree(pid);
    }

    let config_path = flag(args, "--config")
        .map(PathBuf::from)
        .unwrap_or_else(config::Config::path);
    let config = config::Config::load(&config_path);

    lifecycle::log(&format!(
        "daemon starting: config {}, buffer {:.0}s, mode {}, quality {}",
        config_path.display(),
        config.buffer_seconds,
        config.record_mode,
        config.quality
    ));

    match daemon::run(
        config,
        daemon::Options {
            run_seconds: number(args, "--seconds", 0.0),
            save_after: 0.0,
            record_after: 0.0,
            record_for: 0.0,
            stall_after: 0.0,
            ffmpeg: ffmpeg_path(args),
            config_path: Some(config_path),
            ipc: true,
        },
    ) {
        Ok(_) => lifecycle::log("daemon stopped"),
        Err(e) => {
            lifecycle::log(&format!("daemon failed: {e}"));
            return Err(e);
        }
    }
    Ok(())
}


/// Level of the captured signal, so "did we record anything at all" is answerable without ears.
fn rms(pcm: &[i16]) -> f64 {
    if pcm.is_empty() {
        return 0.0;
    }
    let sum: f64 = pcm.iter().map(|&s| { let v = s as f64 / 32768.0; v * v }).sum();
    (sum / pcm.len() as f64).sqrt()
}

/// Crude pitch estimate from sign changes on the left channel. Enough to prove a known test tone
/// came back at the frequency it went out at, which in turn proves the sample format conversion
/// and the sample rate are both right.
fn zero_cross_hz(pcm: &[i16], sample_rate: u32) -> f64 {
    let left: Vec<i16> = pcm.iter().step_by(audio::OUT_CHANNELS).copied().collect();
    if left.len() < 2 {
        return 0.0;
    }
    let crossings = left
        .windows(2)
        .filter(|w| (w[0] >= 0) != (w[1] >= 0))
        .count();
    crossings as f64 * sample_rate as f64 / (2.0 * left.len() as f64)
}

fn round4(v: f64) -> f64 {
    (v * 10000.0).round() / 10000.0
}

fn stats(values: &mut Vec<f64>) -> serde_json::Value {
    if values.is_empty() {
        return serde_json::Value::Null;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let at = |q: f64| values[((values.len() - 1) as f64 * q).round() as usize];
    serde_json::json!({
        "mean": round2(values.iter().sum::<f64>() / values.len() as f64),
        "p50": round2(at(0.50)),
        "p99": round2(at(0.99)),
        "max": round2(*values.last().unwrap()),
    })
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}
