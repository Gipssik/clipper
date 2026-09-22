//! Staying alive exactly as long as we should, and leaving a trail when something goes wrong.
//!
//! The daemon is started by Clipper and has no window, no console and nobody watching it. Both
//! halves of that matter: it must not outlive the app that spawned it — an orphaned recorder
//! writing to disk forever is the worst possible bug here — and when it misbehaves, the log is the
//! only evidence that will exist.

use std::io::Write;
use std::path::PathBuf;

use windows::core::HSTRING;
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    CreateMutexW, OpenProcess, WaitForSingleObject, INFINITE, PROCESS_SYNCHRONIZE,
};

const MUTEX_NAME: &str = "Local\\clipper-capture-singleton";

/// Keeps the handle alive for the life of the process; dropping it would release the mutex.
pub struct Singleton(HANDLE);

impl Drop for Singleton {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// `None` when another daemon already holds the name. Two recorders would fight over the same
/// segment directory and double the cost for nothing.
pub fn single_instance() -> Option<Singleton> {
    unsafe {
        let handle = CreateMutexW(None, true, &HSTRING::from(MUTEX_NAME)).ok()?;
        if windows::Win32::Foundation::GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(handle);
            return None;
        }
        Some(Singleton(handle))
    }
}

/// Exits this process when the given one does.
///
/// A Win32 job object would be the other way to guarantee this, but that needs a native module on
/// the Electron side. Watching the parent handle is pure Rust and survives the parent being killed
/// outright, which is the case that matters — a crash, or Task Manager.
pub fn exit_with_parent(pid: u32) {
    std::thread::spawn(move || unsafe {
        let Ok(handle) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) else {
            // Already gone, or not ours to watch. Either way there is nothing to attach to.
            return;
        };
        if WaitForSingleObject(handle, INFINITE) == WAIT_OBJECT_0 {
            log(&format!("parent process {pid} exited; shutting down"));
            std::process::exit(0);
        }
        let _ = CloseHandle(handle);
    });
}

pub fn log_path() -> PathBuf {
    crate::config::local_appdata()
        .join("clipper")
        .join("capture.log")
}

/// Appends a timestamped line, rotating at a megabyte so an unattended daemon cannot fill a disk.
pub fn log(message: &str) {
    let path = log_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > 1_000_000 {
        let _ = std::fs::rename(&path, path.with_extension("log.old"));
    }

    let now = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    let line = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} {}\n",
        now.wYear, now.wMonth, now.wDay, now.wHour, now.wMinute, now.wSecond, message
    );

    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = file.write_all(line.as_bytes());
    }
    eprintln!("{}", line.trim_end());
}
