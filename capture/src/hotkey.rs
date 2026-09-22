//! The global hotkey.
//!
//! It lives here rather than in Electron's `globalShortcut` because the daemon is the thing that
//! is always running — the UI may not even be open when you want to keep the last minute.
//!
//! **Known limitation, and there is no way around it from user space.** `RegisterHotKey` never
//! sees a key press while a process running at higher integrity has focus. A game launched as
//! administrator, or one whose anti-cheat runs elevated, will swallow the combination silently. A
//! low-level keyboard hook has exactly the same restriction. The only real fix is running the
//! daemon elevated too.

use windows::core::Result;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
    MOD_SHIFT, MOD_WIN,
};
use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, MSG, PM_REMOVE, WM_HOTKEY};

const HOTKEY_ID: i32 = 1;

pub struct Hotkey {
    registered: bool,
    pub spec: String,
}

impl Hotkey {
    pub fn register(spec: &str) -> Result<Self> {
        let (modifiers, vk) = parse(spec).ok_or_else(|| {
            windows::core::Error::new(
                windows::Win32::Foundation::E_INVALIDARG,
                format!("unrecognised hotkey: {spec}"),
            )
        })?;

        // NOREPEAT: holding the combination should save one clip, not one per key repeat.
        unsafe { RegisterHotKey(None, HOTKEY_ID, modifiers | MOD_NOREPEAT, vk)? };
        Ok(Hotkey {
            registered: true,
            spec: spec.to_string(),
        })
    }

    /// How many times the hotkey fired since the last call.
    ///
    /// `RegisterHotKey` posts to the thread that registered it, so this must be called from that
    /// thread — which is the record loop, and is why it polls rather than blocking on a message
    /// pump of its own.
    pub fn taken(&self) -> u32 {
        let mut count = 0;
        let mut msg = MSG::default();
        unsafe {
            while PeekMessageW(&mut msg, None, WM_HOTKEY, WM_HOTKEY, PM_REMOVE).as_bool() {
                if msg.wParam.0 as i32 == HOTKEY_ID {
                    count += 1;
                }
            }
        }
        count
    }
}

impl Drop for Hotkey {
    fn drop(&mut self) {
        if self.registered {
            unsafe {
                let _ = UnregisterHotKey(None, HOTKEY_ID);
            }
        }
    }
}

/// "Ctrl+Alt+F12" and friends. Case and spacing are not significant.
fn parse(spec: &str) -> Option<(HOT_KEY_MODIFIERS, u32)> {
    let mut modifiers = HOT_KEY_MODIFIERS(0);
    let mut key = None;

    for part in spec.split('+') {
        let part = part.trim().to_ascii_lowercase();
        match part.as_str() {
            "ctrl" | "control" => modifiers |= MOD_CONTROL,
            "alt" => modifiers |= MOD_ALT,
            "shift" => modifiers |= MOD_SHIFT,
            "win" | "super" | "meta" => modifiers |= MOD_WIN,
            "" => {}
            other => key = virtual_key(other),
        }
    }

    key.map(|k| (modifiers, k))
}

fn virtual_key(name: &str) -> Option<u32> {
    // Function keys are contiguous from VK_F1 (0x70).
    if let Some(digits) = name.strip_prefix('f') {
        if let Ok(n) = digits.parse::<u32>() {
            if (1..=24).contains(&n) {
                return Some(0x70 + n - 1);
            }
        }
    }
    if name.len() == 1 {
        let c = name.chars().next().unwrap().to_ascii_uppercase();
        if c.is_ascii_alphanumeric() {
            return Some(c as u32);
        }
    }
    match name {
        "space" => Some(0x20),
        "insert" => Some(0x2D),
        "delete" | "del" => Some(0x2E),
        "home" => Some(0x24),
        "end" => Some(0x23),
        "pageup" => Some(0x21),
        "pagedown" => Some(0x22),
        "printscreen" | "prtsc" => Some(0x2C),
        "scrolllock" => Some(0x91),
        "pause" => Some(0x13),
        _ => None,
    }
}
