//! Recording on demand: everything from one press of the record hotkey to the next, as one file.
//!
//! **A second reader of the same stream, not a second recorder.** The pipeline already encodes the
//! screen once for the replay ring; a recording takes a copy of the very same packets on their way
//! there. So it costs no encode silicon, it is at exactly the replay's quality, resolution, tone
//! map and audio mix by construction rather than by keeping two sets of settings in step, and the
//! replay hotkey goes on working in the middle of a recording because nothing about the ring has
//! changed. The only new cost is the disk the file itself takes.
//!
//! **Written as TS, published as MP4.** While it runs the recording is `rec_<time>.ts` in the
//! `Recordings` folder — a stream that is valid up to its last byte, so a daemon that is killed,
//! or a machine that loses power, leaves a file that can still be finished. Stopping remuxes it
//! (`-c copy`, no generation loss) to `.mp4.part` and renames that into place, so the library never
//! shows a half-written MP4, and `recover` finishes anything a previous run left behind. Clipper's
//! folder scan does not list `.ts`, so the grid never shows the file while it grows.
//!
//! **The pipeline is disposable and the recording is not.** A display switched to HDR or turned
//! off, a driver hiccup — each rebuilds the pipeline, and the ring starts over. A recording instead
//! *detaches* and picks up again on the new pipeline's first keyframe, with the gap closed rather
//! than filled with a frozen frame, so an hour-long session survives an HDR toggle as one file.
//! Only a change the file cannot carry — a new resolution, audio appearing or going away — ends
//! it and starts the next one, because an MP4 whose picture changes size halfway is one plenty of
//! players will refuse.

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;

use crate::ring::HZ;
use crate::ts::TsMuxer;

/// Where the first frame lands on the recording's own timeline. Not zero, for the same reason the
/// pipeline's timeline is not: a PCR of 0 upsets some players.
const START_90K: u64 = HZ;

/// What a recording is made of. Two pipelines that agree on this can feed one file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Format {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub audio: bool,
}

pub struct Outcome {
    pub path: PathBuf,
    pub duration_ms: u64,
    pub bytes: u64,
    pub elapsed_ms: f64,
}

pub struct Recording {
    ts_path: PathBuf,
    out_path: PathBuf,
    file: std::io::BufWriter<std::fs::File>,
    muxer: TsMuxer,
    pub format: Format,
    /// Added to a pipeline timestamp to place it on the recording's timeline. `None` while waiting
    /// for a keyframe to start from — at the beginning, and after every rebuild.
    shift: Option<i64>,
    /// Where the next keyframe goes once the recording is waiting: one frame after the last thing
    /// written, so a rebuild closes up rather than leaving a hole.
    resume_90k: u64,
    end_90k: u64,
    frame_90k: u64,
    /// Audio in pipeline time, held until video reaches it so the two interleave in PTS order —
    /// the same reason the ring holds it.
    pending_audio: VecDeque<(Vec<u8>, u64)>,
    pub bytes: u64,
    /// The first write that failed — a full disk, usually. Kept rather than returned, because a
    /// recording that cannot write is not a reason for the pipeline feeding the replay to fail.
    error: Option<String>,
}

impl Recording {
    pub fn start(dir: &Path, format: Format) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let out_path = unique(dir, &crate::timestamp_name("rec"));
        let ts_path = out_path.with_extension("ts");
        let file = std::io::BufWriter::new(std::fs::File::create(&ts_path)?);
        Ok(Recording {
            ts_path,
            out_path,
            file,
            muxer: TsMuxer::new(format.audio),
            format,
            shift: None,
            resume_90k: START_90K,
            end_90k: START_90K,
            frame_90k: HZ / format.fps.max(1) as u64,
            pending_audio: VecDeque::new(),
            bytes: 0,
            error: None,
        })
    }

    pub fn path(&self) -> &Path {
        &self.out_path
    }

    /// True until the first keyframe has been written — the moment the file actually starts.
    pub fn waiting(&self) -> bool {
        self.shift.is_none()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn duration_ms(&self) -> u64 {
        self.end_90k.saturating_sub(START_90K) * 1000 / HZ
    }

    pub fn push_audio(&mut self, data: &[u8], pts_90k: u64) {
        self.pending_audio.push_back((data.to_vec(), pts_90k));
    }

    pub fn push_video(&mut self, data: &[u8], pts_90k: u64, keyframe: bool) {
        if self.error.is_some() {
            return;
        }
        let shift = match self.shift {
            Some(shift) => shift,
            // Only a keyframe can open a stream, and until one arrives there is nothing to place
            // the audio against either — anything heard before the first picture goes.
            None if !keyframe => return,
            None => {
                let shift = self.resume_90k as i64 - pts_90k as i64;
                crate::lifecycle::log(&format!(
                    "recording picks up at {:.3} s (pipeline {:.3} s)",
                    self.resume_90k.saturating_sub(START_90K) as f64 / HZ as f64,
                    pts_90k as f64 / HZ as f64
                ));
                self.shift = Some(shift);
                self.pending_audio.retain(|(_, pts)| *pts >= pts_90k);
                shift
            }
        };
        self.flush_audio(Some(pts_90k));

        let pts = (pts_90k as i64 + shift).max(0) as u64;
        let mut bytes = Vec::new();
        // Tables on every keyframe rather than once at the head: 376 bytes every two seconds, and
        // the file can then be opened from anywhere in the middle, which is what a truncated
        // recording needs.
        if keyframe {
            self.muxer.write_tables(&mut bytes);
        }
        self.muxer.write_video(&mut bytes, data, pts, keyframe);
        self.end_90k = self.end_90k.max(pts);
        self.write(&bytes);
    }

    /// Writes held audio up to `upto` (pipeline time), or all of it.
    fn flush_audio(&mut self, upto: Option<u64>) {
        let Some(shift) = self.shift else {
            return;
        };
        while let Some((_, pts)) = self.pending_audio.front() {
            if upto.is_some_and(|upto| *pts > upto) {
                break;
            }
            let (data, pts) = self.pending_audio.pop_front().unwrap();
            let pts = (pts as i64 + shift).max(0) as u64;
            let mut bytes = Vec::new();
            self.muxer.write_audio(&mut bytes, &data, pts);
            self.write(&bytes);
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        if self.error.is_some() {
            return;
        }
        match self.file.write_all(bytes) {
            Ok(()) => self.bytes += bytes.len() as u64,
            Err(e) => self.error = Some(format!("writing {}: {e}", self.ts_path.display())),
        }
    }

    /// The pipeline feeding this recording is going away. Everything it produced is written, and
    /// the recording waits for the next pipeline's first keyframe, which it places one frame after
    /// this one's last — so the file skips the gap rather than holding a frozen picture across it.
    pub fn detach(&mut self) {
        self.flush_audio(None);
        self.pending_audio.clear();
        if self.shift.take().is_some() {
            self.resume_90k = self.end_90k + self.frame_90k;
        }
        let _ = self.file.flush();
    }

    /// Closes the file and turns it into an MP4 on a background thread, so stopping a long
    /// recording never stalls the frame loop that is still feeding the replay ring.
    pub fn finish(mut self, ffmpeg: PathBuf) -> Receiver<Result<Outcome, String>> {
        self.detach();
        let (tx, rx) = std::sync::mpsc::channel();
        let Recording { ts_path, out_path, file, .. } = self;
        drop(file);
        std::thread::spawn(move || {
            let _ = tx.send(publish(&ts_path, &out_path, &ffmpeg));
        });
        rx
    }
}

/// Remuxes one finished `.ts` into its `.mp4`, by way of a `.part` so the library never sees a
/// half-written file. The `.ts` goes only once the MP4 is in place.
fn publish(ts_path: &Path, out_path: &Path, ffmpeg: &Path) -> Result<Outcome, String> {
    let started = std::time::Instant::now();
    let bytes = std::fs::metadata(ts_path).map(|m| m.len()).unwrap_or(0);
    if bytes == 0 {
        // Stopped before the first keyframe arrived. There is no file to make, and ffmpeg's
        // complaint about an empty input would read like something had broken.
        let _ = std::fs::remove_file(ts_path);
        return Err("stopped before the first frame was recorded".into());
    }
    let part = out_path.with_extension("mp4.part");
    crate::ring::remux(ts_path, &part, ffmpeg)?;
    std::fs::rename(&part, out_path).map_err(|e| format!("rename {part:?}: {e}"))?;
    // `CLIPPER_KEEP_TS` leaves the stream as written, so a timing question about the MP4 can be
    // told apart from one about the muxer.
    if std::env::var_os("CLIPPER_KEEP_TS").is_none() {
        let _ = std::fs::remove_file(ts_path);
    }
    Ok(Outcome {
        path: out_path.to_path_buf(),
        duration_ms: crate::ring::probe_duration_ms(out_path, ffmpeg).unwrap_or(0),
        bytes,
        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
    })
}

/// Finishes whatever a previous run left in `dir` — a daemon killed mid-recording, or one that
/// was told to quit and did not live long enough to remux. Called once, before anything records.
pub fn recover(dir: &Path, ffmpeg: PathBuf) -> Option<Receiver<Result<Outcome, String>>> {
    let orphans: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|e| e == "ts")
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("rec_"))
        })
        .collect();
    if orphans.is_empty() {
        return None;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for ts in orphans {
            crate::lifecycle::log(&format!("finishing a recording a previous run left: {}", ts.display()));
            let result = publish(&ts, &ts.with_extension("mp4"), &ffmpeg);
            if tx.send(result).is_err() {
                return;
            }
        }
    });
    Some(rx)
}

/// `rec_2026-09-24_18-42-03.mp4`, or `…_2.mp4` when a split lands in the same second as the
/// recording it split from.
fn unique(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() && !first.with_extension("ts").exists() {
        return first;
    }
    let stem = Path::new(name).file_stem().and_then(|s| s.to_str()).unwrap_or("rec");
    (2..)
        .map(|n| dir.join(format!("{stem}_{n}.mp4")))
        .find(|p| !p.exists() && !p.with_extension("ts").exists())
        .unwrap()
}
