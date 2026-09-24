//! The config file, and the quality tiers it expands into.
//!
//! Lives at `%APPDATA%\clipper\capture.json` — a sibling of `prefs.json`, not a section inside it.
//! `savePrefs()` on the Electron side does read-modify-write of the whole prefs file from the
//! renderer's settings object; a daemon reading that same file would race the write, and a daemon
//! that ever wrote to it would clobber UI state. Separate file, atomic rewrite, schema owned here.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    pub version: u32,
    pub enabled: bool,
    /// "game" records only while a game has focus; "always" records whenever the daemon runs.
    pub record_mode: String,
    pub buffer_seconds: f64,
    pub monitor: MonitorSelection,
    pub quality: String,
    pub output_path: String,
    pub per_game_subfolder: bool,
    /// How "is this a game" is decided. "auto" uses the classifier in `foreground.rs`;
    /// "fullscreen" is the old, simpler rule that anything filling the screen counts.
    pub game_detection: String,
    /// Whether a saved clip raises a Windows notification. Read by Electron, not by the daemon —
    /// it lives here because it is a property of the recorder, not of the library window.
    pub notify_on_save: bool,
    pub hotkey: String,
    /// Recording on demand. Independent of `enabled`: either one keeps the daemon running.
    pub record: RecordConfig,
    pub audio: AudioConfig,
    /// "auto" follows the display, "always" and "off" override it.
    pub tone_map: String,
    pub segment_dir: Option<String>,
    /// Overrides for the tier's two bitrates, in bits per second. Zero means "use the tier".
    /// These exist for measuring rate-control behaviour against real content without a rebuild;
    /// the tier is what anybody actually sets.
    #[serde(default)]
    pub bitrate_override: u32,
    #[serde(default)]
    pub max_bitrate_override: u32,
    pub include_processes: Vec<String>,
    pub exclude_processes: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MonitorSelection {
    pub device: String,
    pub friendly: String,
}

/// A second hotkey that records from one press to the next.
///
/// It has no quality, screen or audio settings of its own, on purpose: a recording is a copy of
/// the packets the replay pipeline already encodes, so it is the replay's settings by
/// construction. What it does not share is the replay's idea of *when* — a recording ignores the
/// game detection and records the screen for as long as it is asked to — and *where*: it goes to a
/// `Recordings` folder beside the clips, not into a folder per game.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RecordConfig {
    pub enabled: bool,
    pub hotkey: String,
}

impl Default for RecordConfig {
    fn default() -> Self {
        RecordConfig {
            enabled: false,
            hotkey: "Alt+F9".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AudioConfig {
    pub desktop: bool,
    /// Whether to mix a microphone in. On by default: a clip of a match you were in a call for,
    /// with everybody's voice missing, is a clip of the wrong thing, and nobody thinks to turn the
    /// mic on *before* the moment worth keeping.
    pub mic: bool,
    /// The endpoint id to record from. Empty means "whatever Windows currently calls the default",
    /// which is a different promise from naming a device: it follows a headset being plugged in.
    pub mic_device: String,
    /// Gain applied to the microphone before it is summed, in decibels.
    ///
    /// Windows' own input level runs out at 100% and plenty of microphones are still quiet there —
    /// a condenser at conversational distance can sit 30 dB below the game. This is applied in the
    /// mix rather than at the endpoint, where it would clip against full scale before the limiter
    /// ever saw it.
    pub mic_gain_db: f32,
    /// RNNoise on the microphone leg. Off by default: it is a network running on every 10 ms of
    /// voice, it adds a frame of latency, and on a quiet desk it has nothing to do.
    pub noise_suppression: bool,
    /// 0–100: a noise floor up to 40, a voice gate above it; see `denoise::setting`.
    pub noise_strength: f32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        AudioConfig {
            desktop: true,
            mic: true,
            mic_device: String::new(),
            mic_gain_db: 0.0,
            noise_suppression: false,
            noise_strength: 70.0,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            version: 1,
            enabled: true,
            record_mode: "game".into(),
            buffer_seconds: 60.0,
            monitor: MonitorSelection::default(),
            quality: "high".into(),
            output_path: String::new(),
            per_game_subfolder: true,
            game_detection: "auto".into(),
            notify_on_save: true,
            hotkey: "Ctrl+Alt+F12".into(),
            record: RecordConfig::default(),
            audio: AudioConfig::default(),
            tone_map: "auto".into(),
            segment_dir: None,
            bitrate_override: 0,
            max_bitrate_override: 0,
            include_processes: Vec::new(),
            exclude_processes: Vec::new(),
        }
    }
}

/// Buffer bounds. Below 30 s the head rounding to a keyframe is a visible fraction of the clip;
/// above 10 minutes the disk cost stops being negligible.
pub const MIN_BUFFER_SECONDS: f64 = 30.0;
pub const MAX_BUFFER_SECONDS: f64 = 600.0;

impl Config {
    pub fn path() -> PathBuf {
        appdata().join("clipper").join("capture.json")
    }

    pub fn load(path: &std::path::Path) -> Self {
        let mut config: Config = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        config.buffer_seconds = config
            .buffer_seconds
            .clamp(MIN_BUFFER_SECONDS, MAX_BUFFER_SECONDS);
        config
    }

    pub fn tier(&self) -> Tier {
        let mut tier = Tier::named(&self.quality);
        if self.bitrate_override > 0 {
            tier.bitrate = self.bitrate_override;
        }
        if self.max_bitrate_override > 0 {
            tier.max_bitrate = self.max_bitrate_override;
        }
        tier
    }

    /// Where clips go. An empty setting must never mean "the current directory": the daemon is
    /// started by Clipper and inherits whatever working directory it had, so clips would land
    /// somewhere arbitrary and the user would simply never find them.
    pub fn output_dir(&self) -> PathBuf {
        if !self.output_path.is_empty() {
            return PathBuf::from(&self.output_path);
        }
        std::env::var("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir())
            .join("Videos")
            .join("Clipper")
    }

    /// Where recordings go: beside the clips, so the library shows them as one more category.
    pub fn recordings_dir(&self) -> PathBuf {
        self.output_dir().join("Recordings")
    }

    pub fn segment_dir(&self) -> PathBuf {
        match &self.segment_dir {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir),
            _ => local_appdata().join("clipper").join("buffer"),
        }
    }

    /// True when the two differ in a way the recorder cannot absorb without being rebuilt.
    /// Everything else — buffer length, output path, hotkey, record mode — is applied in place, so
    /// nudging a slider does not cost you the footage already buffered.
    pub fn needs_restart(&self, other: &Config) -> bool {
        self.monitor.friendly != other.monitor.friendly
            || self.monitor.device != other.monitor.device
            || self.quality != other.quality
            || self.tone_map != other.tone_map
            // The microphone's boost is deliberately not in here. It is a slider, and a slider
            // that costs you the buffered minute every time you nudge it is a slider nobody can
            // set by ear. The mixer takes the new value in place. Noise suppression and its
            // strength are the same story, for the same reason.
            || self.audio.desktop != other.audio.desktop
            || self.audio.mic != other.audio.mic
            || self.audio.mic_device != other.audio.mic_device
            || self.segment_dir() != other.segment_dir()
    }
}

/// How long between IDRs, in seconds.
///
/// This is two things at once: the trim accuracy at the head of a saved clip, and a real share of
/// the bitrate. An I-frame costs roughly ten times a P-frame, so at 60 fps a one-second GOP spends
/// about a sixth of the stream on keyframes — bits that would otherwise be buying detail in the
/// other 59 frames. Two seconds halves that, and costs a clip that may begin up to two seconds
/// before you asked rather than one. Against a buffer measured in minutes, that is not a cost.
pub const GOP_SECONDS: f32 = 2.0;

/// Where the encoder sits between "fast" and "good". On the NVIDIA MFT this selects the NVENC
/// preset, and it is by far the most expensive knob in the pipeline.
///
/// This was 80, on the theory that the encode chip is nowhere near saturated by one stream and so
/// the preset is close to free. That theory is wrong, and measuring it is what corrected it. The
/// MFT maps this onto three or four preset bands, and the cost of each band roughly doubles:
///
/// | quality-vs-speed | 720p | 1080p | 1440p |
/// |------------------|------|-------|-------|
/// | 0-16             | 3.2% | 6.6%  | -     |
/// | 33-50            | 5.0% | 11.1% | 19.6% |
/// | 66-80            | 10.1%| 22.5% | 46.5% |
///
/// (Video-encode engine, sampled over 16 s of a hard synthetic source at 60 fps.)
///
/// What the top band buys is almost nothing. Encoding twenty seconds of real gameplay at 12 Mbps
/// through the same silicon, NVENC's own presets end up 0.71 dB PSNR apart across their whole
/// range, and the last 0.26 dB of that is what the top band costs double for. Meanwhile 4 Mbps
/// more bitrate at the middle band is worth 1.25 dB, and costs disk rather than the GPU the game
/// is using. So: the middle band, and spend the savings on bits.
pub const QUALITY_VS_SPEED: u32 = 50;

/// A quality tier expands into the settings that actually drive the encoder. One control instead
/// of four keeps the UI honest: the combinations that make sense are the ones offered.
///
/// Two bitrates, not one. `bitrate` is what the stream averages; `max_bitrate` is what a hard
/// scene may spike to. A menu screen costs a fraction of either, and the headroom is what keeps a
/// firefight from turning to mush.
///
/// **The peak is a size guarantee, and the first version gave it away.** It was set at 1.6× the
/// mean, on the reasoning that a spike is brief and the average is what you pay. That is true of
/// content which is sometimes hard. It is not true of a football game, which is hard continuously:
/// a real 60-second REMATCH clip at the 16/26 tier came out at **25.9 Mbps**, meaning the peak was
/// not a spike allowance at all, it was the bitrate. Against a mean of 16 the file was 60% bigger
/// than the number anybody had chosen. The ratio is now about 1.35, so the worst case is close
/// enough to the promise to be one.
///
/// **And the means themselves were too high**, because they were set before the encoder was fixed.
/// Measured on 20 s of real gameplay, against the same source, the milestone 7 and 8 work — the
/// quality preset instead of the low-latency one, a two-second GOP, a real downscale filter — is
/// worth **+1.2 to +1.6 dB at every bitrate**:
///
/// | asked | before (low latency, 1 s GOP) | now (quality preset, 2 s GOP) |
/// |-------|------------------------------|-------------------------------|
/// |  6 Mbps | 45.21 dB                   | 46.39 dB at 5.3 Mbps actual   |
/// |  8 Mbps | 46.63 dB                   | 48.13 dB at 7.2 Mbps actual   |
/// | 10 Mbps | 47.71 dB                   | 49.30 dB at 8.9 Mbps actual   |
/// | 12 Mbps | 48.63 dB                   | 49.97 dB at 10.1 Mbps actual  |
///
/// So 8 Mbps now looks like 11 Mbps did, and the tier that was raised to 16 to fix a picture
/// problem can come back down to 8 without giving that fix back. At 720p that is about 54 MB a
/// minute, which is where hardware recorders sit on the same game.
///
/// Disk is bounded by duration rather than bytes, so none of this can make the ring outgrow its
/// window; what it changes is how much of the window costs.
#[derive(Clone, Copy)]
pub struct Tier {
    pub max_height: u32,
    pub fps: u32,
    pub bitrate: u32,
    pub max_bitrate: u32,
}

impl Tier {
    pub fn named(name: &str) -> Tier {
        match name {
            "low" => Tier { max_height: 720, fps: 60, bitrate: 8_000_000, max_bitrate: 11_000_000 },
            "medium" => Tier { max_height: 1080, fps: 60, bitrate: 14_000_000, max_bitrate: 19_000_000 },
            "ultra" => Tier { max_height: 1440, fps: 60, bitrate: 32_000_000, max_bitrate: 43_000_000 },
            "native" => Tier { max_height: 0, fps: 60, bitrate: 42_000_000, max_bitrate: 56_000_000 },
            // "high" and anything unrecognised.
            _ => Tier { max_height: 1080, fps: 60, bitrate: 20_000_000, max_bitrate: 27_000_000 },
        }
    }
}

pub fn appdata() -> PathBuf {
    std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
}

pub fn local_appdata() -> PathBuf {
    std::env::var("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
}
