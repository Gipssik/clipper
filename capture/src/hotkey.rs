//! The global hotkey: registering it, and capturing the one the user wants.
//!
//! It lives here rather than in Electron's `globalShortcut` because the daemon is the thing that
//! is always running — the UI may not even be open when you want to keep the last minute.
//!
//! **Known limitation, and there is no way around it from user space.** `RegisterHotKey` never
//! sees a key press while a process running at higher integrity has focus. A game launched as
//! administrator, or one whose anti-cheat runs elevated, will swallow the combination silently. A
//! low-level keyboard hook has exactly the same restriction. The only real fix is running the
//! daemon elevated too.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows::core::Result;
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL,
    MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, VK_CONTROL, VK_ESCAPE, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, PeekMessageW, SetWindowsHookExW, UnhookWindowsHookEx, HC_ACTION,
    KBDLLHOOKSTRUCT, MSG, PM_REMOVE, WH_KEYBOARD_LL, WM_HOTKEY, WM_KEYDOWN, WM_SYSKEYDOWN,
};

/// One id per thing a hotkey can do. Both are registered on the record loop's thread and arrive
/// on its queue, which is why `fired` drains them together rather than each hotkey peeking for its
/// own: a `PM_REMOVE` for one would throw the other's message away.
pub const SAVE: i32 = 1;
pub const RECORD: i32 = 2;

pub struct Hotkey {
    id: i32,
    pub spec: String,
}

impl Hotkey {
    pub fn register(spec: &str, id: i32) -> Result<Self> {
        let (modifiers, vk) = parse(spec).ok_or_else(|| {
            windows::core::Error::new(
                windows::Win32::Foundation::E_INVALIDARG,
                format!("unrecognised hotkey: {spec}"),
            )
        })?;

        // NOREPEAT: holding the combination should save one clip, not one per key repeat — and
        // for the record hotkey, should not start a recording and stop it again.
        unsafe { RegisterHotKey(None, id, modifiers | MOD_NOREPEAT, vk)? };
        Ok(Hotkey {
            id,
            spec: spec.to_string(),
        })
    }
}

/// The ids of every hotkey that fired since the last call, in order.
///
/// `RegisterHotKey` posts to the thread that registered it, so this must be called from that
/// thread — which is the record loop, and is why it polls rather than blocking on a message
/// pump of its own.
pub fn fired() -> Vec<i32> {
    let mut ids = Vec::new();
    let mut msg = MSG::default();
    unsafe {
        while PeekMessageW(&mut msg, None, WM_HOTKEY, WM_HOTKEY, PM_REMOVE).as_bool() {
            ids.push(msg.wParam.0 as i32);
        }
    }
    ids
}

impl Drop for Hotkey {
    fn drop(&mut self) {
        unsafe {
            let _ = UnregisterHotKey(None, self.id);
        }
    }
}

// ── Capturing a combination ───────────────────────────────────────────────────
// A low-level keyboard hook, rather than a keydown handler in the settings window, because
// **Windows eats Alt+F-key before any window procedure sees it**. Measured, on the combinations
// this matters for: pressing Alt+F10 over Clipper's window delivers the Alt to the page and then
// nothing at all — the F10 reaches neither the DOM, nor Electron's before-input-event, nor
// WM_SYSKEYDOWN through hookWindowMessage. It is consumed as a system menu command above every
// layer Electron can reach, so the settings panel physically cannot capture it.
//
// WH_KEYBOARD_LL runs ahead of that, which is the whole reason this lives in the daemon: the
// daemon is a plain Win32 process and can install one. It also means the captured key is
// swallowed, so pressing Alt+F4 while choosing a hotkey does not close the window you are
// choosing it in.

/// Where the hook leaves its answer. `None` while listening; `Some("")` means cancelled.
static CAPTURED: Mutex<Option<String>> = Mutex::new(None);

fn is_modifier(vk: u32) -> bool {
    // Both the generic and the side-specific codes; a hook reports the latter.
    matches!(vk, 0x10..=0x12 | 0xA0..=0xA5 | 0x5B | 0x5C)
}

fn held(vk: u32) -> bool {
    unsafe { (GetAsyncKeyState(vk as i32) as u16 & 0x8000) != 0 }
}

/// Alt+F4, and only Alt+F4 — not Alt+Shift+F4, which is nobody's shortcut and a fine hotkey.
///
/// Not ours to take. `RegisterHotKey` would hand it over and then every window on the machine
/// would lose the one shortcut everybody knows, with no way back but finding this panel again.
fn is_system_close(vk: u32) -> bool {
    vk == 0x73
        && held(VK_MENU.0 as u32)
        && !held(VK_CONTROL.0 as u32)
        && !held(VK_SHIFT.0 as u32)
        && !held(VK_LWIN.0 as u32)
        && !held(VK_RWIN.0 as u32)
}

/// The combination currently held, as a spec string, or `None` for a key we cannot name.
fn spec_for(vk: u32) -> Option<String> {
    let name = key_name(vk)?;
    let mut parts = Vec::new();
    // Same order the settings panel writes, so a hotkey set either way reads identically.
    if held(VK_CONTROL.0 as u32) {
        parts.push("Ctrl");
    }
    if held(VK_MENU.0 as u32) {
        parts.push("Alt");
    }
    if held(VK_SHIFT.0 as u32) {
        parts.push("Shift");
    }
    if held(VK_LWIN.0 as u32) || held(VK_RWIN.0 as u32) {
        parts.push("Win");
    }
    let mut spec = parts.join("+");
    if !spec.is_empty() {
        spec.push('+');
    }
    spec.push_str(&name);
    Some(spec)
}

/// Records the answer, first writer wins. An empty string means "stop listening, no change".
fn finish(answer: String) {
    if let Ok(mut slot) = CAPTURED.lock() {
        if slot.is_none() {
            *slot = Some(answer);
        }
    }
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let message = wparam.0 as u32;
        if message == WM_KEYDOWN || message == WM_SYSKEYDOWN {
            let info = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            let vk = info.vkCode;
            // Modifiers pass through: they are context for the real key, not the answer, and
            // swallowing them would strand Windows thinking Alt is still down.
            if !is_modifier(vk) {
                if vk == VK_ESCAPE.0 as u32 {
                    finish(String::new());
                    return LRESULT(1);
                }
                if is_system_close(vk) {
                    // End the capture, but let the key through to do its normal job — leaving the
                    // panel stuck on "Press keys…" for fifteen seconds is not an answer.
                    finish(String::new());
                    return CallNextHookEx(None, code, wparam, lparam);
                }
                if let Some(spec) = spec_for(vk) {
                    finish(spec);
                    return LRESULT(1); // eaten, so the key does not also do its normal job
                }
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

/// Listens for one combination and hands it to `done` — `None` if cancelled or nothing was
/// pressed before the timeout.
///
/// Runs on a thread of its own because a low-level hook needs a message pump on the thread that
/// installed it, and the record loop is busy being a record loop.
pub fn listen<F>(timeout: Duration, done: F)
where
    F: FnOnce(Option<String>) + Send + 'static,
{
    std::thread::spawn(move || unsafe {
        if let Ok(mut slot) = CAPTURED.lock() {
            *slot = None;
        }

        let hook = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0) {
            Ok(h) => h,
            Err(e) => {
                crate::lifecycle::log(&format!("hotkey capture unavailable: {}", e.message()));
                done(None);
                return;
            }
        };

        // Windows silently drops a low-level hook whose thread stops pumping, so this polls
        // rather than sleeping on the answer.
        let deadline = Instant::now() + timeout;
        let answer = loop {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {}

            if let Ok(mut slot) = CAPTURED.lock() {
                if let Some(answer) = slot.take() {
                    break Some(answer);
                }
            }
            if Instant::now() >= deadline {
                break None;
            }
            std::thread::sleep(Duration::from_millis(10));
        };

        let _ = UnhookWindowsHookEx(hook);
        // An empty string is the cancel signal; both it and a timeout mean "no change".
        done(answer.filter(|s| !s.is_empty()));
    });
}

// ── Names ─────────────────────────────────────────────────────────────────────
// `parse` and `key_name` are inverses and share this table, so a key the daemon can capture is
// always a key it can register.

const NAMED: &[(&str, u32)] = &[
    ("Space", 0x20),
    ("Insert", 0x2D),
    ("Delete", 0x2E),
    ("Home", 0x24),
    ("End", 0x23),
    ("PageUp", 0x21),
    ("PageDown", 0x22),
    ("PrintScreen", 0x2C),
    ("ScrollLock", 0x91),
    ("Pause", 0x13),
    ("Backspace", 0x08),
    ("Tab", 0x09),
    ("Enter", 0x0D),
    ("Left", 0x25),
    ("Up", 0x26),
    ("Right", 0x27),
    ("Down", 0x28),
];

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
    // "del" and "prtsc" are not produced by key_name, but a hand-edited config may hold them.
    match name {
        "del" => return Some(0x2E),
        "prtsc" => return Some(0x2C),
        _ => {}
    }
    NAMED
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, vk)| *vk)
}

/// The inverse: what to call a virtual key in a spec string.
fn key_name(vk: u32) -> Option<String> {
    if (0x70..=0x87).contains(&vk) {
        return Some(format!("F{}", vk - 0x70 + 1));
    }
    if (0x30..=0x39).contains(&vk) || (0x41..=0x5A).contains(&vk) {
        return Some((vk as u8 as char).to_string());
    }
    // Numpad digits register as their own keys; name them so they round-trip.
    if (0x60..=0x69).contains(&vk) {
        return Some(format!("{}", vk - 0x60));
    }
    NAMED
        .iter()
        .find(|(_, code)| *code == vk)
        .map(|(n, _)| n.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_codes_are_inverses() {
        for (name, vk) in NAMED {
            assert_eq!(key_name(*vk).as_deref(), Some(*name), "key_name({vk:#x})");
            assert_eq!(virtual_key(&name.to_ascii_lowercase()), Some(*vk), "{name}");
        }
        for vk in [0x70, 0x79, 0x87, 0x41, 0x5A, 0x30, 0x39] {
            let name = key_name(vk).unwrap();
            assert_eq!(virtual_key(&name.to_ascii_lowercase()), Some(vk), "{name}");
        }
    }

    #[test]
    fn parses_the_combinations_the_panel_writes() {
        assert_eq!(parse("Ctrl+Alt+F12"), Some((MOD_CONTROL | MOD_ALT, 0x7B)));
        assert_eq!(parse("Alt+F10"), Some((MOD_ALT, 0x79)));
        assert_eq!(parse("Ctrl+S"), Some((MOD_CONTROL, 0x53)));
        assert_eq!(parse("F9"), Some((HOT_KEY_MODIFIERS(0), 0x78)));
        assert_eq!(parse("Ctrl+Shift+Space"), Some((MOD_CONTROL | MOD_SHIFT, 0x20)));
        assert_eq!(parse("Nonsense"), None);
    }
}
