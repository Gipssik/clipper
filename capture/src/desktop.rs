//! The desktop leg, captured on a thread of its own.
//!
//! **Why a thread.** Everything slow about a device is slow on whichever thread touches it, and the
//! tick that polls audio is also the one that captures and encodes the video. Opening the loopback
//! on this machine's Realtek speakers takes 210 ms and on a virtual output a full second; with the
//! desktop leg following the default output, that open happens mid-recording. Done on the tick,
//! switching from a headset back to the speakers left a 40 ms hole in the video — two frames — and
//! a slow device would have cost a second of them. So a worker here owns the device — opening it,
//! feeding its keep-alive, watching the default, reading it — and hands over packets. Nothing it
//! does can hold the video up: the same switch through the worker leaves every frame 16.7 ms apart.
//!
//! **What stays on the tick** is the `Pacer`, which is to say the timeline. It lives across devices,
//! so a new device resumes where the last one's samples ended and the gap between them is filled
//! rather than squeezed out; and when the worker has nothing to give — between devices, or blocked
//! inside a driver — the tick pads the track with silence on the clock rather than stalling it and
//! then pushing a second of audio into the encoder at once.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};

use windows::core::Result;
use windows::Win32::Media::Audio::{eConsole, eRender, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};

use crate::audio::{
    log_desktop, now_hns, read_format, unsafe_string, Endpoint, MixFormat, Pacer, PollStats,
    DEFAULT_CHECK_HNS, DESK_RETRY_HNS, HNS_PER_SECOND, LOOPBACK_DEADBAND_HNS, MAX_LAG_HNS,
};

/// What to open. `tone_hz` and `keep_alive` are the dev hooks `Endpoint::loopback` has always had.
#[derive(Clone, Copy)]
pub struct DesktopConfig {
    pub tone_hz: Option<f32>,
    pub keep_alive: bool,
}

/// What the worker says about itself, for `status` and the settings panel.
#[derive(Default)]
struct Info {
    name: String,
    error: Option<String>,
    rate_changed: bool,
}

enum Message {
    /// A new stream starts; the packets after this are its.
    Stream,
    Packet { hns: i64, pairs: Vec<[f32; 2]>, discontinuity: bool },
    Stats(PollStats),
}

/// The tick's end of the desktop leg. Dropping it stops the worker.
pub struct Desktop {
    rx: Receiver<Message>,
    info: Arc<Mutex<Info>>,
    stop: Arc<AtomicBool>,
    format: MixFormat,
    rate: i64,
    pacer: Pacer,
    pub first_hns: Option<i64>,
    /// The end of the timeline so far: where the next sample belongs.
    pub last_packet_hns: i64,
    /// The current stream's own clock, for `clock_ppm`: its first stamp, the frames it has handed
    /// over, and where its last packet ended.
    stream_first: Option<i64>,
    stream_frames: u64,
    stream_end: i64,
}

impl Desktop {
    /// Starts the worker and waits for its first device, whose rate becomes the mix rate. A desktop
    /// leg that cannot open anything at all is a pipeline that cannot record audio, so that is
    /// still an error here; everything after the first open is the worker's to recover from.
    pub fn start(config: DesktopConfig) -> Result<Desktop> {
        let (tx, rx) = channel();
        let (ready_tx, ready_rx) = sync_channel(1);
        let info = Arc::new(Mutex::new(Info::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (worker_info, worker_stop) = (info.clone(), stop.clone());
        // Built on the worker, because the device and enumerator it will hold are COM objects that
        // do not cross threads.
        std::thread::Builder::new()
            .name("desktop audio".into())
            .spawn(move || {
                Worker {
                    tx,
                    info: worker_info,
                    stop: worker_stop,
                    config,
                    rate: None,
                    source: None,
                    id: String::new(),
                    enumerator: None,
                    check_at: 0,
                    retry_at: 0,
                }
                .run(ready_tx)
            })
            .map_err(|e| windows::core::Error::new(windows::Win32::Foundation::E_FAIL, e.to_string()))?;

        let format = match ready_rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(Ok(format)) => format,
            Ok(Err(message)) => {
                return Err(windows::core::Error::new(windows::Win32::Foundation::E_FAIL, message))
            }
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                return Err(windows::core::Error::new(
                    windows::Win32::Foundation::E_FAIL,
                    "the desktop audio device did not open within ten seconds",
                ));
            }
        };
        Ok(Desktop {
            rx,
            info,
            stop,
            format,
            rate: format.sample_rate as i64,
            pacer: Pacer::new(format.sample_rate, LOOPBACK_DEADBAND_HNS),
            first_hns: None,
            last_packet_hns: 0,
            stream_first: None,
            stream_frames: 0,
            stream_end: 0,
        })
    }

    pub fn format(&self) -> MixFormat {
        self.format
    }

    /// QPC of the next sample `poll` will append. `None` before the first packet.
    pub fn position_hns(&self) -> Option<i64> {
        self.pacer.position_hns()
    }

    pub fn rate_ratio(&self) -> f64 {
        self.pacer.ratio
    }

    /// The current device's clock against the capture clock, as `Endpoint::clock_ppm` measures it.
    pub fn clock_ppm(&self) -> i64 {
        let Some(first) = self.stream_first else { return 0 };
        let span = self.stream_end - first;
        if span <= 0 {
            return 0;
        }
        let nominal = self.stream_frames as i64 * HNS_PER_SECOND / self.rate;
        (((nominal - span) as f64 / span as f64) * 1_000_000.0).round() as i64
    }

    pub fn name(&self) -> String {
        self.info.lock().map(|i| i.name.clone()).unwrap_or_default()
    }

    /// Why there is no device right now, when there is none.
    pub fn error(&self) -> Option<String> {
        self.info.lock().ok().and_then(|i| i.error.clone())
    }

    /// The default moved to a device that cannot be read at the mix rate. See `Mixer`.
    pub fn rate_changed(&self) -> bool {
        self.info.lock().map(|i| i.rate_changed).unwrap_or(false)
    }

    /// Appends everything the worker has handed over, paced onto the capture clock, and silence for
    /// any stretch it has fallen too far behind on.
    pub fn poll(&mut self, out: &mut Vec<i16>) -> Result<PollStats> {
        let mut stats = PollStats::default();
        loop {
            match self.rx.try_recv() {
                Ok(Message::Stream) => {
                    if let Some(at) = self.pacer.position_hns() {
                        self.pacer.resume(at);
                    }
                    self.stream_first = None;
                    self.stream_frames = 0;
                }
                Ok(Message::Packet { hns, pairs, discontinuity }) => {
                    self.first_hns.get_or_insert(hns);
                    stats.filled_frames += self.pacer.packet(hns, &pairs, discontinuity, out);
                    self.stream_first.get_or_insert(hns);
                    self.stream_frames += pairs.len() as u64;
                    self.stream_end = hns + pairs.len() as i64 * HNS_PER_SECOND / self.rate;
                }
                Ok(Message::Stats(read)) => {
                    stats.captured_frames += read.captured_frames;
                    stats.silent_packets += read.silent_packets;
                    stats.discontinuities += read.discontinuities;
                }
                // Disconnected means the worker has gone, which only happens on the way out; the
                // padding below keeps the track honest until the pipeline is torn down.
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        // Held back from the present by the margin the mixer allows any late source: a healthy
        // worker is never more than a couple of packets behind, so this only ever covers a device
        // that has stopped delivering.
        let behind = now_hns() - MAX_LAG_HNS;
        if self.pacer.position_hns().is_some_and(|at| at < behind) {
            stats.filled_frames += self.pacer.pad_to(behind, out);
        }
        if let Some(at) = self.pacer.position_hns() {
            self.last_packet_hns = at;
        }
        Ok(stats)
    }
}

impl Drop for Desktop {
    fn drop(&mut self) {
        // Not joined: the worker may be inside a driver call for up to a second, and a pipeline
        // being torn down should not wait for that. It sees the flag, or its sends start failing,
        // and exits on its own.
        self.stop.store(true, Ordering::Relaxed);
    }
}

// ── The worker ────────────────────────────────────────────────────────────────

struct Worker {
    tx: Sender<Message>,
    info: Arc<Mutex<Info>>,
    stop: Arc<AtomicBool>,
    config: DesktopConfig,
    /// The mix rate, once the first device has set it. Every device after is read at it.
    rate: Option<u32>,
    source: Option<Endpoint>,
    /// The open device's endpoint id, compared against the default to notice it moving.
    id: String,
    enumerator: Option<IMMDeviceEnumerator>,
    check_at: i64,
    retry_at: i64,
}

impl Worker {
    fn run(mut self, ready: std::sync::mpsc::SyncSender<std::result::Result<MixFormat, String>>) {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        match self.open() {
            Ok(format) => {
                self.rate = Some(format.sample_rate);
                let _ = ready.send(Ok(format));
            }
            Err(e) => {
                let _ = ready.send(Err(e.message().to_string()));
                return;
            }
        }
        let Ok(mut ticker) = crate::clock::Ticker::new(100) else { return };

        while !self.stop.load(Ordering::Relaxed) {
            ticker.wait();
            let now = now_hns();
            self.follow_default(now);
            if self.source.is_none() && now >= self.retry_at {
                self.retry_at = now + DESK_RETRY_HNS;
                if let Err(e) = self.open() {
                    self.failed_open(&e);
                }
            }
            let Some(source) = &mut self.source else { continue };
            let tx = &self.tx;
            let mut gone = false;
            // The keep-alive first: a device that has gone away fails here before it fails a read.
            let read = source.pump_silence().and_then(|()| {
                source.read(|hns, pairs, discontinuity| {
                    gone |= tx.send(Message::Packet { hns, pairs: pairs.to_vec(), discontinuity }).is_err();
                })
            });
            match read {
                Ok(stats) => gone |= self.tx.send(Message::Stats(stats)).is_err(),
                Err(e) => self.lose(&e, now),
            }
            if gone {
                return;
            }
        }
    }

    fn open(&mut self) -> Result<MixFormat> {
        let (endpoint, name, id) =
            Endpoint::loopback(self.config.tone_hz, self.config.keep_alive, self.rate)?;
        let format = endpoint.format();
        log_desktop(&name, format);
        self.source = Some(endpoint);
        self.id = id;
        if let Ok(mut info) = self.info.lock() {
            info.name = name;
            info.error = None;
        }
        let _ = self.tx.send(Message::Stream);
        Ok(format)
    }

    fn failed_open(&mut self, e: &windows::core::Error) {
        let message = e.message().to_string();
        let rate_changed = self.default_output_rate().is_some_and(|r| Some(r) != self.rate);
        if let Ok(mut info) = self.info.lock() {
            if info.error.as_deref() != Some(message.as_str()) {
                crate::lifecycle::log(&format!("desktop audio unavailable: {message}"));
            }
            info.error = Some(message);
            info.rate_changed |= rate_changed;
        }
    }

    /// The device errored — unplugged, disabled, or taken by an exclusive-mode application. The
    /// next pass tries whatever is the default now.
    fn lose(&mut self, e: &windows::core::Error, now: i64) {
        crate::lifecycle::log(&format!("desktop audio stopped: {}", e.message()));
        if let Ok(mut info) = self.info.lock() {
            info.error = Some(e.message().to_string());
        }
        self.source = None;
        self.retry_at = now;
    }

    /// Switching from speakers to a headset leaves the speakers present, so a stream on them never
    /// fails — it records silence. Nothing but asking notices, so this asks, four times a second.
    fn follow_default(&mut self, now: i64) {
        if self.source.is_none() || self.id.is_empty() || now < self.check_at {
            return;
        }
        self.check_at = now + DEFAULT_CHECK_HNS;
        let Some(default) = self.default_id() else { return };
        if default != self.id {
            let name = self.info.lock().map(|i| i.name.clone()).unwrap_or_default();
            crate::lifecycle::log(&format!("desktop audio: system default moved away from {name}"));
            self.source = None;
            self.retry_at = now;
        }
    }

    fn enumerator(&mut self) -> Option<&IMMDeviceEnumerator> {
        if self.enumerator.is_none() {
            self.enumerator =
                unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok() };
        }
        self.enumerator.as_ref()
    }

    fn default_id(&mut self) -> Option<String> {
        unsafe {
            let device = self.enumerator()?.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
            device.GetId().ok().map(|id| unsafe_string(id))
        }
    }

    fn default_output_rate(&mut self) -> Option<u32> {
        unsafe {
            let device = self.enumerator()?.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None).ok()?;
            let wave = client.GetMixFormat().ok()?;
            let rate = read_format(wave).sample_rate;
            windows::Win32::System::Com::CoTaskMemFree(Some(wave as *const _));
            Some(rate)
        }
    }
}
