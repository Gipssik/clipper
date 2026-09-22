//! QPC time and the fixed-rate tick.
//!
//! QPC is the single clock the whole daemon runs on: it paces the encoder here, and later it is
//! what video and audio timestamps are both derived from. A/V sync comes from stamping both legs
//! against this clock at the moment of capture — never from the order things arrive in.
//!
//! The ticker schedules against an absolute deadline rather than sleeping for a fixed interval,
//! because "sleep 16.67 ms" accumulates error and at 60 fps you notice within a minute. A
//! periodic `SetWaitableTimer` cannot help: its period is whole milliseconds, and 60 fps is not.

use windows::core::Result;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{
    CreateWaitableTimerExW, SetWaitableTimer, WaitForSingleObject,
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, INFINITE, TIMER_ALL_ACCESS,
};

pub fn qpc_frequency() -> i64 {
    let mut f = 0i64;
    unsafe { QueryPerformanceFrequency(&mut f).ok() };
    f
}

pub fn qpc_now() -> i64 {
    let mut t = 0i64;
    unsafe { QueryPerformanceCounter(&mut t).ok() };
    t
}

/// QPC ticks -> 100 ns units, which is what Media Foundation and the WASAPI packet timestamps
/// both speak. Converting here is what puts video and audio on one timeline.
pub fn qpc_to_hns(ticks: i64, freq: i64) -> i64 {
    // Split to keep the multiply from overflowing on a long-running session.
    let whole = ticks / freq;
    let rest = ticks % freq;
    whole * 10_000_000 + rest * 10_000_000 / freq
}

/// QPC ticks -> milliseconds, for logging and stats.
pub fn qpc_to_ms(ticks: i64, freq: i64) -> f64 {
    ticks as f64 * 1000.0 / freq as f64
}

pub struct Ticker {
    timer: HANDLE,
    freq: i64,
    period: i64,
    next: i64,
    /// Ticks we gave up on because the loop fell more than a full period behind.
    pub missed: u64,
}

impl Ticker {
    pub fn new(fps: u32) -> Result<Self> {
        // The high-resolution flag is what makes this a ~0.1 ms timer instead of one quantised to
        // the 15.6 ms scheduler tick. Win10 1803+, which we already require for capture.
        let timer = unsafe {
            CreateWaitableTimerExW(
                None,
                None,
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )?
        };
        let freq = qpc_frequency();
        let period = freq / fps.max(1) as i64;
        Ok(Ticker {
            timer,
            freq,
            period,
            next: qpc_now() + period,
            missed: 0,
        })
    }

    /// Blocks until the next tick is due. Returns the QPC time the tick was *scheduled* for, which
    /// is the timestamp a frame captured on this tick should carry — using the wake time instead
    /// would bake scheduler jitter into the video's timeline.
    pub fn wait(&mut self) -> i64 {
        let scheduled = self.next;

        loop {
            let remaining = self.next - qpc_now();
            if remaining <= 0 {
                break;
            }
            // Negative due time means relative, in 100 ns units.
            let due = -(remaining * 10_000_000 / self.freq);
            unsafe {
                if SetWaitableTimer(self.timer, &due, 0, None, None, false).is_err() {
                    break;
                }
                if WaitForSingleObject(self.timer, INFINITE) != WAIT_OBJECT_0 {
                    break;
                }
            }
        }

        self.next += self.period;

        // If something stalled us past a whole period, skip ahead rather than firing a burst of
        // catch-up ticks: a burst would encode several frames with stale content and make the
        // stall worse.
        let now = qpc_now();
        if self.next < now {
            self.missed += ((now - self.next) / self.period) as u64 + 1;
            self.next = now + self.period;
        }

        scheduled
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.timer);
        }
    }
}
