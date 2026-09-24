//! The replay buffer: rolling TS segments on disk, and the save that turns them into a clip.
//!
//! Disk rather than RAM. A minute at 30 Mbps is 225 MB and ten minutes is 2.2 GB — a real cost in
//! RAM on a gaming machine, and 3.75 MB/s of sequential writes on an SSD, which is nothing. It
//! also survives a crash, and it makes long buffers cheap.
//!
//! Segments start only on a keyframe, so every one is independently decodable and a save is a byte
//! concatenation. Eviction drops whole segments from the front; there is no such thing as trimming
//! part of one.
//!
//! **Pinning.** A save reads segment files while the recorder keeps writing new ones and evicting
//! old ones. Pinned segments are exempt from eviction until the copy finishes, so a save taken at
//! the exact moment the ring rolls cannot have the floor pulled out from under it.

use std::collections::{HashSet, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use crate::ts::TsMuxer;

pub const HZ: u64 = 90_000;

/// Slack kept beyond the requested window so a save taken right on a segment boundary still has
/// the full duration behind it.
const SLACK_SEGMENTS: u64 = 2;

/// Tolerance on the rotation deadline, in 90 kHz ticks (20 ms).
///
/// Timestamps arrive as integers after two divisions, so a keyframe meant to land exactly on the
/// two-second mark routinely computes as 179_999 against a threshold of 180_000. Without slop the
/// test fails and rotation waits a whole further keyframe, which silently turned every segment
/// into three seconds instead of two. The margin is tiny next to the one-second keyframe interval,
/// so it can never rotate a keyframe early.
const ROTATE_SLOP_90K: u64 = HZ / 50;

pub struct Segment {
    pub path: PathBuf,
    pub start_90k: u64,
    pub end_90k: u64,
    pub bytes: u64,
}

struct Open {
    path: PathBuf,
    file: std::io::BufWriter<std::fs::File>,
    start_90k: u64,
    end_90k: u64,
    bytes: u64,
}

pub struct SaveOutcome {
    pub path: PathBuf,
    pub segments: usize,
    pub source_bytes: u64,
    pub requested_ms: u64,
    /// What the saved clip actually spans, which can exceed the request by up to one segment —
    /// the head is rounded back to a keyframe.
    pub actual_ms: u64,
    pub elapsed_ms: f64,
}

pub struct Ring {
    dir: PathBuf,
    target_90k: u64,
    window_90k: u64,
    muxer: TsMuxer,
    current: Option<Open>,
    segments: VecDeque<Segment>,
    /// Audio waits here until video reaches its timestamp, so the two interleave in PTS order
    /// rather than in the order our polling happened to produce them.
    pending_audio: VecDeque<(Vec<u8>, u64)>,
    pinned: Arc<Mutex<HashSet<PathBuf>>>,
    seq: u64,
    pub segments_written: u64,
    pub bytes_written: u64,
    pub evicted: u64,
}

impl Ring {
    pub fn new(dir: &Path, target_seconds: f64, window_seconds: f64, has_audio: bool) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        // A previous run's segments are dead weight: their timeline has nothing to do with this
        // session's. Sweep them rather than let the directory grow without limit.
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                if entry.path().extension().is_some_and(|e| e == "ts") {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }

        Ok(Ring {
            dir: dir.to_path_buf(),
            target_90k: (target_seconds * HZ as f64) as u64,
            window_90k: (window_seconds * HZ as f64) as u64,
            muxer: TsMuxer::new(has_audio),
            current: None,
            segments: VecDeque::new(),
            pending_audio: VecDeque::new(),
            pinned: Arc::new(Mutex::new(HashSet::new())),
            seq: 0,
            segments_written: 0,
            bytes_written: 0,
            evicted: 0,
        })
    }

    pub fn push_audio(&mut self, data: Vec<u8>, pts_90k: u64) {
        self.pending_audio.push_back((data, pts_90k));
    }

    pub fn push_video(&mut self, data: &[u8], pts_90k: u64, keyframe: bool) -> std::io::Result<()> {
        // Everything audible before this picture belongs ahead of it in the stream.
        self.flush_audio(pts_90k)?;

        let rotate = match &self.current {
            None => true,
            Some(open) => {
                keyframe
                    && pts_90k.saturating_sub(open.start_90k) + ROTATE_SLOP_90K >= self.target_90k
            }
        };
        if rotate {
            // Only ever on a keyframe: a segment that opens on a P-frame cannot be decoded alone,
            // which would defeat the whole point of concatenating them.
            if keyframe || self.current.is_none() {
                self.rotate(pts_90k)?;
            }
        }

        let mut bytes = Vec::new();
        self.muxer.write_video(&mut bytes, data, pts_90k, keyframe);
        self.write(&bytes, pts_90k)
    }

    fn flush_audio(&mut self, upto_90k: u64) -> std::io::Result<()> {
        while let Some((_, pts)) = self.pending_audio.front() {
            if *pts > upto_90k {
                break;
            }
            let (data, pts) = self.pending_audio.pop_front().unwrap();
            if self.current.is_none() {
                // Audio before the first keyframe has nowhere to go; the stream has not started.
                continue;
            }
            let mut bytes = Vec::new();
            self.muxer.write_audio(&mut bytes, &data, pts);
            self.write(&bytes, pts)?;
        }
        Ok(())
    }

    fn write(&mut self, bytes: &[u8], pts_90k: u64) -> std::io::Result<()> {
        if let Some(open) = &mut self.current {
            open.file.write_all(bytes)?;
            open.bytes += bytes.len() as u64;
            open.end_90k = open.end_90k.max(pts_90k);
            self.bytes_written += bytes.len() as u64;
        }
        Ok(())
    }

    fn rotate(&mut self, pts_90k: u64) -> std::io::Result<()> {
        self.close_current();

        self.seq += 1;
        let path = self.dir.join(format!("seg_{:08}.ts", self.seq));
        let mut file = std::io::BufWriter::new(std::fs::File::create(&path)?);

        // Every segment opens with its own tables, which is what makes byte concatenation work.
        let mut head = Vec::new();
        self.muxer.write_tables(&mut head);
        file.write_all(&head)?;

        self.current = Some(Open {
            path,
            file,
            start_90k: pts_90k,
            end_90k: pts_90k,
            bytes: head.len() as u64,
        });
        self.segments_written += 1;
        self.bytes_written += head.len() as u64;
        self.evict();
        Ok(())
    }

    fn close_current(&mut self) {
        if let Some(mut open) = self.current.take() {
            let _ = open.file.flush();
            self.segments.push_back(Segment {
                path: open.path,
                start_90k: open.start_90k,
                end_90k: open.end_90k,
                bytes: open.bytes,
            });
        }
    }

    fn evict(&mut self) {
        let keep = self.window_90k + SLACK_SEGMENTS * self.target_90k;
        let pinned = self.pinned.lock().unwrap();
        while self.segments.len() > 1 {
            // Measure from the segment being written, not the last closed one, or the ring holds
            // an extra segment's worth of disk beyond the configured window.
            let newest = self
                .current
                .as_ref()
                .map(|o| o.end_90k)
                .or_else(|| self.segments.back().map(|s| s.end_90k))
                .unwrap_or(0);
            let front = self.segments.front().unwrap();
            if newest.saturating_sub(front.start_90k) <= keep {
                break;
            }
            if pinned.contains(&front.path) {
                break;
            }
            let front = self.segments.pop_front().unwrap();
            let _ = std::fs::remove_file(&front.path);
            self.evicted += 1;
        }
    }

    /// Deletes every segment a save is not reading. For replay being switched off while the
    /// pipeline stays up for a recording: the footage is no longer wanted, and nothing else would
    /// clear it until the next ring swept the directory.
    pub fn discard(&mut self) {
        self.close_current();
        let pinned = self.pinned.lock().unwrap();
        for segment in self.segments.drain(..) {
            if !pinned.contains(&segment.path) {
                let _ = std::fs::remove_file(&segment.path);
            }
        }
    }

    /// Flushes anything still buffered and closes the open segment.
    pub fn finish(&mut self) -> std::io::Result<()> {
        let last = self.pending_audio.back().map(|(_, pts)| *pts).unwrap_or(0);
        self.flush_audio(last)?;
        self.close_current();
        Ok(())
    }

    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    /// Span currently on disk.
    pub fn buffered_90k(&self) -> u64 {
        let start = self.segments.front().map(|s| s.start_90k);
        let end = self
            .current
            .as_ref()
            .map(|o| o.end_90k)
            .or_else(|| self.segments.back().map(|s| s.end_90k));
        match (start, end) {
            (Some(s), Some(e)) => e.saturating_sub(s),
            _ => 0,
        }
    }

    /// Copies the last `window_ms` out to an MP4, on a background thread.
    ///
    /// Returns immediately: capture and encode must not stall behind a file copy, or pressing the
    /// hotkey would stutter the game it is trying to record.
    pub fn save(
        &mut self,
        window_ms: u64,
        out: PathBuf,
        ffmpeg: PathBuf,
    ) -> Receiver<Result<SaveOutcome, String>> {
        let (tx, rx) = std::sync::mpsc::channel();

        // Close the open segment so its bytes are on disk and it can be part of this clip.
        self.close_current();

        let window_90k = window_ms * HZ / 1000;
        let newest = self.segments.back().map(|s| s.end_90k).unwrap_or(0);
        let cutoff = newest.saturating_sub(window_90k);

        // The segment containing the cutoff is included whole — its head is the nearest keyframe
        // at or before what was asked for.
        let chosen: Vec<(PathBuf, u64, u64, u64)> = self
            .segments
            .iter()
            .filter(|s| s.end_90k > cutoff)
            .map(|s| (s.path.clone(), s.start_90k, s.end_90k, s.bytes))
            .collect();

        {
            let mut pinned = self.pinned.lock().unwrap();
            for (path, ..) in &chosen {
                pinned.insert(path.clone());
            }
        }

        let pinned = Arc::clone(&self.pinned);
        let dir = self.dir.clone();
        let started = std::time::Instant::now();

        std::thread::spawn(move || {
            let result = concat_and_remux(&dir, &chosen, &out, &ffmpeg, window_ms, started);
            {
                let mut set = pinned.lock().unwrap();
                for (path, ..) in &chosen {
                    set.remove(path);
                }
            }
            let _ = tx.send(result);
        });

        rx
    }
}

fn concat_and_remux(
    dir: &Path,
    chosen: &[(PathBuf, u64, u64, u64)],
    out: &Path,
    ffmpeg: &Path,
    window_ms: u64,
    started: std::time::Instant,
) -> Result<SaveOutcome, String> {
    if chosen.is_empty() {
        return Err("nothing buffered yet".into());
    }

    let joined = dir.join(format!(
        "save_{}.ts",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));

    let mut source_bytes = 0u64;
    {
        let mut file = std::io::BufWriter::new(
            std::fs::File::create(&joined).map_err(|e| format!("create {joined:?}: {e}"))?,
        );
        for (path, ..) in chosen {
            let bytes = std::fs::read(path).map_err(|e| format!("read {path:?}: {e}"))?;
            source_bytes += bytes.len() as u64;
            file.write_all(&bytes)
                .map_err(|e| format!("write {joined:?}: {e}"))?;
        }
        file.flush().map_err(|e| e.to_string())?;
    }

    let result = remux(&joined, out, ffmpeg);
    let _ = std::fs::remove_file(&joined);
    result?;

    let first = chosen.first().unwrap();
    let last = chosen.last().unwrap();
    Ok(SaveOutcome {
        path: out.to_path_buf(),
        segments: chosen.len(),
        source_bytes,
        requested_ms: window_ms,
        actual_ms: (last.2.saturating_sub(first.1)) * 1000 / HZ,
        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
    })
}

/// A TS stream into an MP4, bitstream untouched. Shared by a saved replay and a finished recording.
///
/// -c copy: the bitstream is already what we want, and re-encoding on the way out would cost
/// generation loss for nothing. aac_adtstoasc converts the ADTS framing TS carries into the ASC
/// form MP4 expects; without it the audio track is rejected. `-f mp4` because a recording is
/// written to a `.part` name first, and ffmpeg would otherwise guess the container from that.
pub fn remux(input: &Path, out: &Path, ffmpeg: &Path) -> Result<(), String> {
    if let Some(parent) = out.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let output = std::process::Command::new(ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-fflags",
            "+genpts",
            "-i",
        ])
        .arg(input)
        .args(["-c", "copy", "-bsf:a", "aac_adtstoasc", "-movflags", "+faststart", "-f", "mp4", "-y"])
        .arg(out)
        .output()
        .map_err(|e| format!("spawn ffmpeg: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "ffmpeg failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

/// The container's own idea of how long a file is, from the banner ffmpeg prints for any input.
pub fn probe_duration_ms(path: &Path, ffmpeg: &Path) -> Option<u64> {
    let output = std::process::Command::new(ffmpeg)
        .args(["-hide_banner", "-i"])
        .arg(path)
        .output()
        .ok()?;
    let banner = String::from_utf8_lossy(&output.stderr);
    let at = banner.find("Duration: ")? + "Duration: ".len();
    let mut parts = banner[at..].split(',').next()?.trim().split(':');
    let h: f64 = parts.next()?.parse().ok()?;
    let m: f64 = parts.next()?.parse().ok()?;
    let s: f64 = parts.next()?.parse().ok()?;
    Some(((h * 3600.0 + m * 60.0 + s) * 1000.0).round() as u64)
}
