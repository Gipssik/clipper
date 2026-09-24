//! Control channel: a named pipe carrying newline-delimited JSON.
//!
//! Restarting the daemon whenever a setting changes would be simpler, and wrong: a restart drops
//! the ring, so nudging a slider would cost you the last minute of footage. Almost everything can
//! be applied in place, and this is how the app says so.
//!
//! Commands in, events out, one client at a time — the client is Clipper's main process and there
//! is only ever one of it.
//!
//! **Everything here is overlapped, and that is not optional.** A named pipe opened without
//! `FILE_FLAG_OVERLAPPED` is a synchronous handle, and Windows serialises operations on those: the
//! reader thread parked in `ReadFile` waiting for the next command blocks any `WriteFile` on the
//! same handle until that read completes. Emitting an event from the recording loop would then
//! stall the recorder until the UI happened to send its next command — which is exactly what it
//! did, throttling capture to the rate the settings panel polled. With overlapped handles the two
//! directions are independent.

use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use windows::core::HSTRING;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_IO_PENDING, HANDLE, INVALID_HANDLE_VALUE,
};
use windows::Win32::Storage::FileSystem::{
    ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows::Win32::System::Threading::CreateEventW;
use windows::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};

pub const PIPE_NAME: &str = r"\\.\pipe\clipper-capture";

const BUFFER: u32 = 64 * 1024;

#[derive(Debug)]
pub enum Command {
    Status,
    Save,
    /// Start or stop a recording: `Some(true)` starts, `Some(false)` stops, `None` toggles, which
    /// is what the hotkey and the tray do.
    Record(Option<bool>),
    Reload,
    Quit,
    /// Capture the next combination the user presses and report it back.
    Listen,
    Unknown(String),
}

/// A pipe handle that can cross threads. Windows handles are process-wide; the only reason Rust
/// objects is that the raw pointer type is not `Send`.
struct Pipe(HANDLE);
unsafe impl Send for Pipe {}

/// Runs one overlapped operation to completion on `handle`.
///
/// Each call gets its own event, so a read and a write in flight at the same time never share
/// completion state.
unsafe fn await_overlapped(
    handle: HANDLE,
    start: impl FnOnce(&mut OVERLAPPED) -> bool,
) -> Option<u32> {
    let event = CreateEventW(None, true, false, None).ok()?;
    let mut overlapped = OVERLAPPED {
        hEvent: event,
        ..Default::default()
    };

    let started = start(&mut overlapped);
    if !started && GetLastError() != ERROR_IO_PENDING {
        let _ = CloseHandle(event);
        return None;
    }

    let mut transferred = 0u32;
    let ok = GetOverlappedResult(handle, &overlapped, &mut transferred, true).is_ok();
    let _ = CloseHandle(event);
    if ok {
        Some(transferred)
    } else {
        None
    }
}

#[derive(Clone)]
pub struct Emitter {
    client: Arc<Mutex<Option<Pipe>>>,
}

impl Emitter {
    /// Best effort by design: the UI not being attached is the normal case, not an error.
    pub fn emit(&self, event: &serde_json::Value) {
        let mut line = event.to_string();
        line.push('\n');
        let guard = self.client.lock().unwrap();
        if let Some(pipe) = guard.as_ref() {
            unsafe {
                await_overlapped(pipe.0, |ov| {
                    WriteFile(pipe.0, Some(line.as_bytes()), None, Some(ov)).is_ok()
                });
            }
        }
    }
}

pub struct Control {
    rx: Receiver<Command>,
    emitter: Emitter,
}

impl Control {
    pub fn start() -> Control {
        let (tx, rx) = std::sync::mpsc::channel();
        let client: Arc<Mutex<Option<Pipe>>> = Arc::new(Mutex::new(None));
        let emitter = Emitter {
            client: Arc::clone(&client),
        };

        let thread_client = Arc::clone(&client);
        std::thread::spawn(move || serve(tx, thread_client));

        Control { rx, emitter }
    }

    pub fn emitter(&self) -> Emitter {
        self.emitter.clone()
    }

    pub fn try_recv(&self) -> Option<Command> {
        self.rx.try_recv().ok()
    }
}

fn serve(tx: Sender<Command>, client: Arc<Mutex<Option<Pipe>>>) {
    loop {
        let handle = unsafe {
            CreateNamedPipeW(
                &HSTRING::from(crate::lifecycle::instance_name(PIPE_NAME)),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                BUFFER,
                BUFFER,
                0,
                None,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            // Nothing useful to do but stop trying; the daemon still records without a UI.
            return;
        }

        // Waits until Clipper connects, which may be never — the recorder does not depend on it.
        let connected =
            unsafe { await_overlapped(handle, |ov| ConnectNamedPipe(handle, Some(ov)).is_ok()) }
                .is_some();
        if !connected {
            unsafe {
                let _ = CloseHandle(handle);
            }
            continue;
        }

        *client.lock().unwrap() = Some(Pipe(handle));
        read_commands(handle, &tx);
        *client.lock().unwrap() = None;

        unsafe {
            let _ = DisconnectNamedPipe(handle);
            let _ = CloseHandle(handle);
        }
    }
}

fn read_commands(handle: HANDLE, tx: &Sender<Command>) {
    let mut pending = String::new();
    let mut buffer = [0u8; 4096];

    loop {
        let read = match unsafe {
            await_overlapped(handle, |ov| {
                ReadFile(handle, Some(&mut buffer), None, Some(ov)).is_ok()
            })
        } {
            Some(n) if n > 0 => n,
            _ => return, // client went away
        };
        pending.push_str(&String::from_utf8_lossy(&buffer[..read as usize]));

        while let Some(newline) = pending.find('\n') {
            let line: String = pending.drain(..=newline).collect();
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if tx.send(parse(line)).is_err() {
                return; // recorder has stopped
            }
        }
    }
}

fn parse(line: &str) -> Command {
    let value: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return Command::Unknown(line.to_string()),
    };
    match value.get("cmd").and_then(|c| c.as_str()) {
        Some("status") => Command::Status,
        Some("save") => Command::Save,
        Some("record") => Command::Record(value.get("on").and_then(|v| v.as_bool())),
        Some("reload") => Command::Reload,
        Some("quit") => Command::Quit,
        Some("listen") => Command::Listen,
        other => Command::Unknown(other.unwrap_or("").to_string()),
    }
}
