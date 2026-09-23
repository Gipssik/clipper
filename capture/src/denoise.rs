//! Noise suppression for the microphone: RNNoise, through its pure-Rust port, and a gate keyed on
//! its voice detector.
//!
//! RNNoise is a small recurrent network that estimates, twenty-two bands at a time, how much of
//! each 10 ms frame is voice, and attenuates the rest. It is the usual answer for a voice leg
//! because it is cheap enough to run on every sample, and it runs on the CPU without any runtime to ship.
//!
//! **One slider, two stages.** The strength runs 0–100 and means two different things either side
//! of `GATE_FROM`:
//!
//! * **Below it, a dry/wet blend, which is a noise floor.** Mixing the untouched signal back in at
//!   `1 - wet` puts a ceiling on how far anything can be pulled down — where the network has
//!   silenced a frame, what is left is the dry signal at `1 - wet`. The floor in dB is the slider's
//!   number, so 30% leaves noise at -30 dB. This is the gentle end: the dry signal covers the
//!   network's mistakes on breaths and the tails of words rather than letting them be heard as
//!   warble.
//! * **Above it, the network's full output, plus a gate.** A blend can only ever be *weaker* than
//!   the network, and the network on its own leaves a lot behind. Measured per frame, it cuts pink
//!   noise — fans, a room — by ~50 dB, but flat hiss by 1.5 dB and keyboard clicks by barely
//!   anything, because it hears both as consonants. Its voice detector does better, but not by as
//!   much as a single probe suggests: over a long run it scores hiss up to 0.83 and clicks up to
//!   0.58, against 0.97 and up through a word. So the top of the slider closes a gate on frames the
//!   detector is not sure are speech, and moving up the slider does two things at once — the gate
//!   closes deeper, `strength - GATE_FROM` dB, down to -60 at 100%, and it demands more certainty
//!   before it opens, from a score of 0.5 at `GATE_FROM` to 0.9 at 100%. The low end of that range
//!   only shuts on what is obviously not a voice; the top passes nothing the network doubts. The
//!   certainty rises on a square-root curve rather than a line, because the noises that fool the
//!   detector most all score in the 0.7s and 0.8s: on a line, hiss came down 11 dB at 80% and
//!   69 dB at 100%, which is a switch rather than a slider.
//!
//! The gate is shaped so it does not chop words. It looks `LOOKAHEAD` frames ahead, because the
//! detector needs a frame or two of a word before it is sure, and the first consonant of a word is
//! the part a gate most often eats. It holds for `HOLD` frames after the last voiced one, so the
//! tail of a word — quieter, and less clearly voice — is not cut off mid-decay. It opens within a
//! frame and closes at `RELEASE_DB` per frame, so a pause fades rather than snaps.
//!
//! **Latency is real, and it is taken back out rather than ignored.** The network works on
//! 480-sample frames with overlap-add, so its output runs `LATENCY` samples behind its input, the
//! first frame of it being nothing but warm-up. That frame is thrown away here, so output sample
//! `n` is input sample `n` and carries the same timestamp. The mixer aligns the mic to the video by
//! timestamp, and a voice that landed 10 ms late would be a bug nobody could find by ear. The
//! alternative — keeping the warm-up and stamping the stream 10 ms early — works on paper, but a
//! suppressor switched on mid-session then hands the mix 10 ms of audio for a stretch it has
//! already emitted, and every toggle shows up in the dropped-sample counter as a splice. The dry
//! leg of the blend is delayed by the same amount, or it would comb-filter the voice against itself.
//!
//! What the latency and the lookahead still cost is availability: a sample is ready up to four
//! frames after it was captured, so the mix waits that much longer for the microphone. The clip
//! does not notice. The lookahead runs at every strength, gate or no gate, so moving the slider
//! across `GATE_FROM` never changes how far behind the output runs.
//!
//! It runs at 48 kHz only, which is what the network was trained at. The mix runs at whatever the
//! desktop endpoint runs at, and a mix at 44.1 kHz reports that suppression is unavailable rather
//! than feeding the network audio at the wrong pitch.

use std::collections::VecDeque;

use nnnoiseless::DenoiseState;

use crate::audio::OUT_CHANNELS;

pub const RATE: u32 = 48_000;
pub const FRAME: usize = DenoiseState::FRAME_SIZE;

/// How far the network's output runs behind its input, in samples, and so how much warm-up is
/// discarded. Measured, and held there by `output_lines_up_with_input` below.
pub const LATENCY: usize = FRAME;

/// Where the slider stops blending and starts gating. Below it the number is the noise floor in dB;
/// above it, the gate's depth is the distance past it in dB, reaching -60 at 100.
pub const GATE_FROM: f32 = 40.0;

/// Frames the gate looks ahead of the one it is deciding, and frames it stays open after the last
/// voiced one. 20 ms and 200 ms.
const LOOKAHEAD: usize = 2;
const HOLD: usize = 20;

/// The detector score the gate opens at, at the bottom and the top of its range. See the module
/// comment for where the numbers come from.
const OPEN_AT_LO: f32 = 0.5;
const OPEN_AT_HI: f32 = 0.9;

/// The gate ramps from shut to open across this much score below where it opens, rather than
/// switching at one value, so a score hovering at the boundary moves the gain a little instead of
/// flapping it open and shut.
const RAMP: f32 = 0.08;

/// How fast a closing gate may fall, per frame. 100 ms from open to -60 dB.
const RELEASE_DB: f32 = 6.0;

/// What a strength means: how much of the network's output is used, and how deep the gate closes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Setting {
    pub wet: f32,
    pub gate_db: f32,
    /// The detector score at and above which a frame is treated as voice.
    pub open_at: f32,
}

pub fn setting(strength: f32) -> Setting {
    let s = strength.clamp(0.0, 100.0);
    if s < GATE_FROM {
        Setting { wet: 1.0 - db_to_gain(-s), gate_db: 0.0, open_at: OPEN_AT_LO }
    } else {
        let t = ((s - GATE_FROM) / (100.0 - GATE_FROM)).sqrt();
        Setting { wet: 1.0, gate_db: -(s - GATE_FROM), open_at: OPEN_AT_LO + (OPEN_AT_HI - OPEN_AT_LO) * t }
    }
}

fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// One processed frame waiting for the gate to decide about it.
struct Pending {
    samples: Vec<f32>,
    vad: f32,
}

pub struct Suppressor {
    state: Box<DenoiseState<'static>>,
    setting: Setting,
    /// Mono input waiting for a full frame.
    pending: Vec<f32>,
    /// The dry leg, delayed to line up with the network's output.
    dry: VecDeque<f32>,
    frame_out: Vec<f32>,
    /// Blended frames the gate has not decided yet, oldest first. Holds `LOOKAHEAD` frames of
    /// future beyond the one being decided.
    ahead: VecDeque<Pending>,
    /// Detector scores of the last `HOLD` frames emitted, for the hold.
    behind: VecDeque<f32>,
    /// The gain the last emitted frame ended on, so the next one ramps from it.
    gain: f32,
    /// Warm-up output still to discard.
    skip: usize,
}

impl Suppressor {
    pub fn new(strength: f32) -> Suppressor {
        Suppressor {
            state: DenoiseState::new(),
            setting: setting(strength),
            pending: Vec::with_capacity(FRAME),
            dry: VecDeque::from(vec![0.0; LATENCY]),
            frame_out: vec![0.0; FRAME],
            ahead: VecDeque::with_capacity(LOOKAHEAD + 1),
            behind: VecDeque::with_capacity(HOLD),
            gain: 1.0,
            skip: LATENCY,
        }
    }

    /// Takes effect on the next frame. Nothing is rebuilt: a blend weight and a gate depth.
    pub fn set_strength(&mut self, strength: f32) {
        self.setting = setting(strength);
    }

    /// Takes interleaved stereo and appends interleaved stereo, a whole frame at a time. Output is
    /// contiguous and sample-for-sample aligned with the input — the first output sample *is* the
    /// first input sample, processed — so it carries the input's timestamps unchanged. It arrives
    /// late, though: what is short of a frame, the frame of warm-up, and the gate's lookahead all
    /// wait for later calls.
    ///
    /// The voice is folded to mono for the network and written back to both channels. A microphone
    /// is a mono source in all but name — the endpoint duplicates it — and running the network
    /// twice to preserve a stereo image nobody has would double its cost for nothing.
    pub fn process(&mut self, input: &[i16], out: &mut Vec<i16>) {
        for pair in input.chunks_exact(OUT_CHANNELS) {
            let mono = pair.iter().map(|&s| s as f32).sum::<f32>() / OUT_CHANNELS as f32;
            self.pending.push(mono);
            if self.pending.len() < FRAME {
                continue;
            }
            let vad = self.state.process_frame(&mut self.frame_out, &self.pending);
            let wet = self.setting.wet;
            let mut samples = Vec::with_capacity(FRAME);
            for (i, &x) in self.pending.iter().enumerate() {
                self.dry.push_back(x);
                let dry = self.dry.pop_front().unwrap_or(0.0);
                samples.push(self.frame_out[i] * wet + dry * (1.0 - wet));
            }
            self.pending.clear();
            self.ahead.push_back(Pending { samples, vad });
            if self.ahead.len() > LOOKAHEAD {
                self.emit(out);
            }
        }
    }

    /// Decides the gain for the oldest frame waiting, and writes it out.
    fn emit(&mut self, out: &mut Vec<i16>) {
        let Some(frame) = self.ahead.pop_front() else { return };
        // How sure the detector is about this frame and the ones coming (the lookahead), and
        // whether any of the frames just emitted was certainly voice (the hold). The hold is keyed
        // on a clear decision, not on the loudest score: over a 200 ms window of hiss the highest
        // score is routinely high enough to hold a gate open for ever.
        let open_at = self.setting.open_at;
        let soon = self.ahead.iter().fold(frame.vad, |a, p| a.max(p.vad));
        let held = self.behind.iter().any(|&v| v >= open_at);
        self.behind.push_back(frame.vad);
        if self.behind.len() > HOLD {
            self.behind.pop_front();
        }

        let open = if held { 1.0 } else { ((soon - (open_at - RAMP)) / RAMP).clamp(0.0, 1.0) };
        let target = db_to_gain(self.setting.gate_db * (1.0 - open));
        // Opens at once, closes at a bounded rate: a word must not wait for the gate, and a pause
        // should fade rather than snap.
        let end = target.max(self.gain * db_to_gain(-RELEASE_DB));
        let start = self.gain;
        self.gain = end;

        for (i, &v) in frame.samples.iter().enumerate() {
            if self.skip > 0 {
                self.skip -= 1;
                continue;
            }
            let g = start + (end - start) * (i + 1) as f32 / FRAME as f32;
            let s = (v * g).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
            for _ in 0..OUT_CHANNELS {
                out.push(s);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A voice-like test signal: a pitched harmonic stack whose loudness swells and falls, which the
    /// network treats as speech and passes, rather than a steady sine it may decide is a hum.
    fn voiceish(frames: usize) -> Vec<i16> {
        (0..frames)
            .flat_map(|n| {
                let t = n as f32 / RATE as f32;
                let env = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * 3.0 * t).cos();
                let f0 = 140.0 + 20.0 * (2.0 * std::f32::consts::PI * 0.7 * t).sin();
                let v: f32 = (1..12)
                    .map(|k| (2.0 * std::f32::consts::PI * f0 * k as f32 * t).sin() / k as f32)
                    .sum();
                let s = (v * env * 6000.0) as i16;
                [s, s]
            })
            .collect()
    }

    fn xorshift(seed: &mut u32) -> f32 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 17;
        *seed ^= *seed << 5;
        (*seed as f32 / u32::MAX as f32) - 0.5
    }

    /// Pink-ish noise: three leaky integrators of white noise, which is roughly the spectrum of a
    /// fan, a PC case or a room — what RNNoise was trained to remove.
    fn pink(frames: usize, amp: f32, seed: u32) -> Vec<i16> {
        let mut seed = seed;
        let mut b = [0f32; 3];
        (0..frames)
            .flat_map(|_| {
                let w = xorshift(&mut seed);
                b[0] = 0.997 * b[0] + 0.029 * w;
                b[1] = 0.985 * b[1] + 0.032 * w;
                b[2] = 0.95 * b[2] + 0.048 * w;
                let s = ((b[0] + b[1] + b[2]) * 4.0 * amp) as i16;
                [s, s]
            })
            .collect()
    }

    /// Flat hiss: a cheap preamp, or a gain set too high. The network barely touches it.
    fn white(frames: usize, amp: f32, seed: u32) -> Vec<i16> {
        let mut seed = seed;
        (0..frames)
            .flat_map(|_| {
                let s = (xorshift(&mut seed) * amp) as i16;
                [s, s]
            })
            .collect()
    }

    /// A keyboard, roughly: a 3 ms burst of decaying noise every 120 ms.
    fn clicks(frames: usize, amp: f32, seed: u32) -> Vec<i16> {
        let mut seed = seed;
        (0..frames)
            .flat_map(|i| {
                let p = i % 5760;
                let v = if p < 150 {
                    xorshift(&mut seed) * amp * (-(p as f32) / 40.0).exp()
                } else {
                    0.0
                };
                let s = v as i16;
                [s, s]
            })
            .collect()
    }

    fn mono(stereo: &[i16]) -> Vec<f32> {
        stereo.chunks_exact(2).map(|p| p[0] as f32).collect()
    }

    fn rms(s: &[i16]) -> f64 {
        (s.iter().map(|&v| (v as f64).powi(2)).sum::<f64>() / s.len().max(1) as f64).sqrt()
    }

    fn db(a: f64, b: f64) -> f64 {
        20.0 * (a.max(1e-3) / b).log10()
    }

    /// Output held back inside the suppressor at the end of a run: the warm-up and the lookahead.
    const HELD: usize = LATENCY + LOOKAHEAD * FRAME;

    fn run(strength: f32, input: &[i16]) -> Vec<i16> {
        let mut s = Suppressor::new(strength);
        let mut out = Vec::new();
        // In chunks the size WASAPI hands over, so frame boundaries fall mid-chunk.
        for chunk in input.chunks(441 * 2) {
            s.process(chunk, &mut out);
        }
        out
    }

    fn best_lag(a: &[f32], b: &[f32], max: usize) -> usize {
        (0..max)
            .max_by(|&x, &y| {
                let c = |lag: usize| -> f32 { a.iter().zip(&b[lag..]).map(|(p, q)| p * q).sum() };
                c(x).partial_cmp(&c(y)).unwrap()
            })
            .unwrap()
    }

    #[test]
    fn output_lines_up_with_input() {
        // Broadband and aperiodic, so there is exactly one lag at which the output lines up — a
        // pitched signal correlates almost as well one pitch period either side. Full network and
        // no gate, so the dry leg, which is aligned by construction, cannot be what is found, and a
        // closed gate cannot leave nothing to correlate against. Loud, so the network passes enough.
        let input = pink(RATE as usize * 2, 12000.0, 0x9e37_79b9);
        let out = run(GATE_FROM, &input);
        let (x, y) = (mono(&input), mono(&out));
        let lag = best_lag(&x[RATE as usize / 2..RATE as usize], &y[RATE as usize / 2..], FRAME * 3);
        assert_eq!(lag, 0, "output lags its input by {lag} samples; LATENCY is wrong");
    }

    #[test]
    fn zero_strength_is_the_input_exactly() {
        let input = voiceish(FRAME * 20);
        let out = run(0.0, &input);
        assert_eq!(out.len(), input.len() - HELD * 2);
        assert_eq!(&out[..], &input[..out.len()]);
    }

    /// Two seconds of noise, then two of voice over the same noise; how far each came down.
    fn measure(strength: f32, noise: &[i16]) -> (f64, f64) {
        let second = RATE as usize * 2;
        let voice = voiceish(RATE as usize * 2);
        let mut input = noise[..second * 2].to_vec();
        input.extend(voice.iter().zip(&noise[second * 2..]).map(|(v, n)| v.saturating_add(*n)));
        let out = run(strength, &input);
        // Skip the first half second while the network settles, and the half second after the
        // voice starts.
        let noise_cut = db(rms(&out[second / 2..second * 3 / 2]), rms(&input[second / 2..second * 3 / 2]));
        let voice_cut = db(rms(&out[second * 5 / 2..]), rms(&input[second * 5 / 2..out.len()]));
        (noise_cut, voice_cut)
    }

    #[test]
    fn the_slider_reaches_every_kind_of_noise() {
        let n = RATE as usize * 4;
        let kinds = [
            ("pink", pink(n, 1600.0, 0x1234_5678)),
            ("white", white(n, 3000.0, 0x2468_ace0)),
            ("clicks", clicks(n, 20000.0, 0x1357_9bdf)),
        ];
        for (name, noise) in &kinds {
            let row: Vec<String> = [0.0, 20.0, 40.0, 60.0, 80.0, 100.0]
                .iter()
                .map(|&s| {
                    let (n, v) = measure(s, noise);
                    format!("{s:>3}%: {n:6.1}/{v:5.1}")
                })
                .collect();
            eprintln!("{name:6} noise/voice dB  {}", row.join("  "));

            let (noise_cut, voice_cut) = measure(100.0, noise);
            assert!(noise_cut < -45.0, "{name}: 100% only brought noise down {noise_cut:.1} dB");
            assert!(voice_cut > -3.0, "{name}: 100% took the voice down {voice_cut:.1} dB");
            // Monotone: every step of the slider does at least as much as the one before.
            let cuts: Vec<f64> = [20.0, 40.0, 60.0, 80.0, 100.0].iter().map(|&s| measure(s, noise).0).collect();
            assert!(cuts.windows(2).all(|w| w[1] <= w[0] + 1.0), "{name}: not monotone: {cuts:?}");
        }
    }

    #[test]
    fn the_gate_does_not_eat_the_start_of_a_word() {
        // Hiss broken by a word: whatever the gate does to the pause, the start of the word has to
        // come through.
        let second = RATE as usize * 2;
        let mut input = white(RATE as usize, 3000.0, 0xdead_beef);
        let word = voiceish(RATE as usize / 2);
        let under = white(RATE as usize / 2, 3000.0, 0xfeed_f00d);
        input.extend(word.iter().zip(&under).map(|(v, n)| v.saturating_add(*n)));
        input.extend(white(RATE as usize, 3000.0, 0xabad_cafe));
        let out = run(100.0, &input);
        // The fixture's envelope starts from zero, so the first 50 ms where it is loud enough to
        // matter is 20–70 ms in.
        let at = second + second / 50;
        let span = at..at + second / 20;
        let cut = db(rms(&out[span.clone()]), rms(&input[span]));
        eprintln!("onset {cut:.1} dB");
        assert!(cut > -6.0, "the first 50 ms of the word came down {cut:.1} dB");
    }

    #[test]
    fn strength_means_what_the_label_says() {
        assert_eq!(setting(0.0).wet, 0.0);
        assert_eq!(setting(0.0).gate_db, 0.0);
        assert!((setting(30.0).wet - (1.0 - db_to_gain(-30.0))).abs() < 1e-6);
        assert_eq!(setting(GATE_FROM), Setting { wet: 1.0, gate_db: 0.0, open_at: OPEN_AT_LO });
        assert_eq!(setting(100.0), Setting { wet: 1.0, gate_db: -60.0, open_at: OPEN_AT_HI });
    }
}
