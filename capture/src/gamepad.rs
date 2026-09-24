//! Buttons on game controllers — sim racing wheels above all — as alternative hotkeys.
//!
//! A wheel is where the hands are during a race, and reaching for Alt+F10 mid-corner is not a
//! thing anybody does. So the alternative binds can be a button on anything Windows calls a
//! joystick, gamepad or multi-axis controller: wheel rims, button boxes, flight sticks.
//!
//! **Raw Input, because it is the one path that keeps delivering while a game has focus.**
//! `Windows.Gaming.Input` would be less code and stops reporting the moment our process is in the
//! background, which is always. DirectInput can run in the background too, but needs a top-level
//! window and polls. Raw Input with `RIDEV_INPUTSINK`, registered on a message-only window in a
//! thread of its own, is handed every HID report the device sends, focused or not.
//!
//! **Parsed by the device's own descriptor, not by guessing at bytes.** `HidP_GetUsages` on the
//! button page and `HidP_GetUsageValue` on the hat switch read the report through the preparsed
//! data Windows already built from the device, so a Moza base, a Fanatec rim and a twenty-quid
//! button box all come out as "Button N" and "Hat Up" without anything specific to any of them.
//!
//! **It only runs while it has a reason to.** A force-feedback wheel base reports its position
//! continuously — hundreds of reports a second while the wheel is sitting still — and every one of
//! them is a message to wake up for. So the reader exists only while some bind names a controller,
//! or while the settings panel is asking which button to use. `Pads` stops its thread on drop.
//!
//! A button is identified by the device's vendor and product id plus the control's name. The id
//! survives unplugging and moving to another port; a device path or a handle does not. Two
//! identical devices would share one id, which for a wheel on one desk is not a real case.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};

use windows::core::{w, PCWSTR};
use windows::Win32::Devices::HumanInterfaceDevice::{
    HidD_GetProductString, HidP_GetButtonCaps, HidP_GetCaps, HidP_GetUsageValue, HidP_GetUsages,
    HidP_GetValueCaps, HidP_Input, HIDP_BUTTON_CAPS, HIDP_CAPS, HIDP_STATUS_SUCCESS, HIDP_VALUE_CAPS,
    PHIDP_PREPARSED_DATA,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::{
    GetRawInputData, GetRawInputDeviceInfoW, GetRawInputDeviceList, RegisterRawInputDevices,
    HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTDEVICELIST, RAWINPUTHEADER, RIDEV_INPUTSINK,
    RIDI_DEVICEINFO, RIDI_DEVICENAME, RIDI_PREPARSEDDATA, RID_DEVICE_INFO, RID_INPUT, RIM_TYPEHID,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, PeekMessageW,
    PostThreadMessageW, PM_REMOVE, RegisterClassW, HWND_MESSAGE, MSG, WINDOW_EX_STYLE, WINDOW_STYLE, WM_INPUT,
    WM_QUIT, WNDCLASSW,
};

/// HID usage page 1 (generic desktop): joystick, gamepad, multi-axis controller. Wheels report as
/// one of these three, depending on the vendor.
const GAME_USAGES: [u16; 3] = [0x04, 0x05, 0x08];
const PAGE_GENERIC: u16 = 0x01;
const PAGE_BUTTON: u16 = 0x09;
const USAGE_HAT: u16 = 0x39;

/// Every HID report read, from every controller. What a wheel base costs is how often it wakes
/// the reader, and this is how that gets measured rather than assumed.
pub static REPORTS: AtomicU64 = AtomicU64::new(0);

/// One control going down.
#[derive(Clone, Debug)]
pub struct Press {
    /// `346E:0004` — vendor and product id, which is what a bind matches on.
    pub device: String,
    /// What the device calls itself, for the settings panel. Not matched on.
    pub name: String,
    /// `Button 12`, `Hat Up`.
    pub control: String,
}

impl Press {
    /// The string a bind is stored as: readable in `capture.json`, and parseable back by `Bind`.
    pub fn spec(&self) -> String {
        format!("{} / {} [{}]", self.name, self.control, self.device)
    }
}

/// A bind on a controller, parsed from `Name / Control [VID:PID]`.
#[derive(Clone, Debug, PartialEq)]
pub struct Bind {
    pub device: String,
    pub control: String,
}

impl Bind {
    /// `None` for anything that is not a controller spec — a keyboard combination, or nothing.
    pub fn parse(spec: &str) -> Option<Bind> {
        let spec = spec.trim();
        let open = spec.rfind('[')?;
        if !spec.ends_with(']') {
            return None;
        }
        let device = spec[open + 1..spec.len() - 1].trim().to_ascii_uppercase();
        let (_, control) = spec[..open].trim().rsplit_once(" / ")?;
        if device.is_empty() || control.trim().is_empty() {
            return None;
        }
        Some(Bind {
            device,
            control: control.trim().to_string(),
        })
    }

    pub fn matches(&self, press: &Press) -> bool {
        self.device.eq_ignore_ascii_case(&press.device) && self.control.eq_ignore_ascii_case(&press.control)
    }
}

pub fn is_pad_spec(spec: &str) -> bool {
    Bind::parse(spec).is_some()
}

/// The running reader. Dropping it stops the thread.
pub struct Pads {
    rx: Receiver<Press>,
    thread_id: u32,
}

impl Pads {
    pub fn start() -> Option<Pads> {
        let (tx, rx) = std::sync::mpsc::channel();
        let (id_tx, id_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || unsafe { run(tx, id_tx) });
        let thread_id = id_rx.recv().ok()??;
        Some(Pads { rx, thread_id })
    }

    /// Everything pressed since the last call.
    pub fn poll(&self) -> Vec<Press> {
        self.rx.try_iter().collect()
    }
}

impl Drop for Pads {
    fn drop(&mut self) {
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
    }
}

/// What one report says is down: up to 256 buttons as a bit set, and the hat's direction. Compared
/// report to report without allocating, because a wheel base sends hundreds of reports a second
/// and almost none of them change a button — only the ones that do become a `Press`.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Held {
    buttons: [u64; 4],
    hat: Option<&'static str>,
}

impl Held {
    fn set(&mut self, usage: u16) {
        if (usage as usize) < 256 {
            self.buttons[usage as usize / 64] |= 1 << (usage % 64);
        }
    }

    fn has(&self, usage: usize) -> bool {
        self.buttons[usage / 64] & (1 << (usage % 64)) != 0
    }

    /// Controls down in `self` that were not down in `before`.
    fn pressed_since(&self, before: &Held) -> Vec<String> {
        let mut out: Vec<String> = (0..256)
            .filter(|&u| self.has(u) && !before.has(u))
            .map(|u| format!("Button {u}"))
            .collect();
        if let Some(direction) = self.hat {
            if before.hat != Some(direction) {
                out.push(format!("Hat {direction}"));
            }
        }
        out
    }
}

struct Device {
    id: String,
    name: String,
    preparsed: Vec<u8>,
    /// Logical range of the hat switch, if there is one: most report 0–7 clockwise from up and
    /// something outside that range for centred.
    hat: Option<(i32, i32)>,
    held: Held,
    /// False until the first report has been read. Whatever that report says is down was down
    /// before we started listening, and is not a press — the MOZA R5 base reports one of its 128
    /// buttons as permanently held, which otherwise fires the moment the reader starts.
    primed: bool,
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

unsafe fn run(tx: Sender<Press>, id_tx: Sender<Option<u32>>) {
    let class = w!("ClipperPads");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        lpszClassName: class,
        ..Default::default()
    };
    RegisterClassW(&wc);
    let Ok(hwnd) = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        class,
        PCWSTR::null(),
        WINDOW_STYLE(0),
        0,
        0,
        0,
        0,
        Some(HWND_MESSAGE),
        None,
        None,
        None,
    ) else {
        crate::lifecycle::log("controllers: could not create the input window");
        let _ = id_tx.send(None);
        return;
    };

    let devices: Vec<RAWINPUTDEVICE> = GAME_USAGES
        .iter()
        .map(|&usage| RAWINPUTDEVICE {
            usUsagePage: PAGE_GENERIC,
            usUsage: usage,
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: hwnd,
        })
        .collect();
    if let Err(e) = RegisterRawInputDevices(&devices, std::mem::size_of::<RAWINPUTDEVICE>() as u32) {
        crate::lifecycle::log(&format!("controllers: raw input registration failed: {}", e.message()));
        let _ = DestroyWindow(hwnd);
        let _ = id_tx.send(None);
        return;
    }
    let _ = id_tx.send(Some(GetCurrentThreadId()));

    let mut known: HashMap<isize, Option<Device>> = HashMap::new();
    let mut buffer: Vec<u8> = Vec::new();
    let mut report: Vec<u8> = Vec::new();
    let mut msg = MSG::default();
    // Woken on a clock, not per message. A wheel base sends ~900 reports a second whether or not
    // anybody touches it, and waking for each one measured 2.1% of a core; draining the queue
    // sixty times a second does the same work in a fraction of the wake-ups, and 16 ms is not a
    // delay anybody can feel on a hotkey. The queue holds thousands, so nothing is lost between.
    'pump: loop {
        std::thread::sleep(std::time::Duration::from_millis(16));
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            if msg.message == WM_QUIT {
                break 'pump;
            }
            if msg.message == WM_INPUT {
                read(HRAWINPUT(msg.lParam.0 as _), &mut buffer, &mut report, &mut known, &tx);
            }
            // WM_INPUT has to reach DefWindowProc so the system can free the input.
            DispatchMessageW(&msg);
        }
    }
    let _ = DestroyWindow(hwnd);
}

unsafe fn read(
    handle: HRAWINPUT,
    buffer: &mut Vec<u8>,
    report: &mut Vec<u8>,
    known: &mut HashMap<isize, Option<Device>>,
    tx: &Sender<Press>,
) {
    let header = std::mem::size_of::<RAWINPUTHEADER>() as u32;
    let mut size = 0u32;
    GetRawInputData(handle, RID_INPUT, None, &mut size, header);
    if size == 0 {
        return;
    }
    buffer.resize(size as usize, 0);
    if GetRawInputData(handle, RID_INPUT, Some(buffer.as_mut_ptr() as _), &mut size, header) != size {
        return;
    }
    let raw = &*(buffer.as_ptr() as *const RAWINPUT);
    if raw.header.dwType != RIM_TYPEHID.0 {
        return;
    }
    let device = known
        .entry(raw.header.hDevice.0 as isize)
        .or_insert_with(|| describe(raw.header.hDevice));
    let Some(device) = device else {
        return;
    };

    let hid = raw.data.hid;
    let per = hid.dwSizeHid as usize;
    let first = std::ptr::addr_of!(raw.data.hid.bRawData) as *const u8;
    REPORTS.fetch_add(hid.dwCount as u64, Ordering::Relaxed);
    for i in 0..hid.dwCount as usize {
        report.clear();
        report.extend_from_slice(std::slice::from_raw_parts(first.add(i * per), per));
        let now = controls(device, report);
        if !device.primed {
            device.primed = true;
            device.held = now;
            continue;
        }
        if now == device.held {
            continue;
        }
        for control in now.pressed_since(&device.held) {
            let _ = tx.send(Press {
                device: device.id.clone(),
                name: device.name.clone(),
                control,
            });
        }
        device.held = now;
    }
}

/// Which buttons and hat direction one report says are down.
unsafe fn controls(device: &Device, report: &mut [u8]) -> Held {
    let mut held = Held::default();
    let preparsed = PHIDP_PREPARSED_DATA(device.preparsed.as_ptr() as isize);

    let mut usages = [0u16; 256];
    let mut count = usages.len() as u32;
    if HidP_GetUsages(HidP_Input, PAGE_BUTTON, None, usages.as_mut_ptr(), &mut count, preparsed, report)
        == HIDP_STATUS_SUCCESS
    {
        for &usage in &usages[..count as usize] {
            held.set(usage);
        }
    }

    if let Some((min, max)) = device.hat {
        let mut value = 0u32;
        if HidP_GetUsageValue(HidP_Input, PAGE_GENERIC, None, USAGE_HAT, &mut value, preparsed, report)
            == HIDP_STATUS_SUCCESS
        {
            held.hat = hat_direction(value as i32, min, max);
        }
    }
    held
}

/// A hat reports one of eight positions clockwise from up, or a value outside its range for
/// centred. A four-position hat has a range of 0–3 and steps of 90°.
fn hat_direction(value: i32, min: i32, max: i32) -> Option<&'static str> {
    const EIGHT: [&str; 8] = ["Up", "Up-Right", "Right", "Down-Right", "Down", "Down-Left", "Left", "Up-Left"];
    if value < min || value > max {
        return None;
    }
    let positions = max - min + 1;
    let step = match positions {
        8 => 1,
        4 => 2,
        _ => return None,
    };
    EIGHT.get(((value - min) * step) as usize).copied()
}

/// Everything needed to read reports from one device, or `None` if it is not one we can.
unsafe fn describe(handle: HANDLE) -> Option<Device> {
    let mut info = RID_DEVICE_INFO {
        cbSize: std::mem::size_of::<RID_DEVICE_INFO>() as u32,
        ..Default::default()
    };
    let mut size = info.cbSize;
    if GetRawInputDeviceInfoW(Some(handle), RIDI_DEVICEINFO, Some(&mut info as *mut _ as _), &mut size) == u32::MAX
        || info.dwType != RIM_TYPEHID
    {
        return None;
    }
    let hid = info.Anonymous.hid;

    let mut size = 0u32;
    GetRawInputDeviceInfoW(Some(handle), RIDI_PREPARSEDDATA, None, &mut size);
    let mut preparsed = vec![0u8; size as usize];
    if size == 0
        || GetRawInputDeviceInfoW(Some(handle), RIDI_PREPARSEDDATA, Some(preparsed.as_mut_ptr() as _), &mut size)
            == u32::MAX
    {
        return None;
    }
    let pp = PHIDP_PREPARSED_DATA(preparsed.as_ptr() as isize);

    let mut caps = HIDP_CAPS::default();
    if HidP_GetCaps(pp, &mut caps) != HIDP_STATUS_SUCCESS {
        return None;
    }
    let mut hat = None;
    if caps.NumberInputValueCaps > 0 {
        let mut values = vec![HIDP_VALUE_CAPS::default(); caps.NumberInputValueCaps as usize];
        let mut n = caps.NumberInputValueCaps;
        if HidP_GetValueCaps(HidP_Input, values.as_mut_ptr(), &mut n, pp) == HIDP_STATUS_SUCCESS {
            hat = values[..n as usize].iter().find_map(|v| {
                let usage = if v.IsRange { v.Anonymous.Range.UsageMin } else { v.Anonymous.NotRange.Usage };
                (v.UsagePage == PAGE_GENERIC && usage == USAGE_HAT).then_some((v.LogicalMin, v.LogicalMax))
            });
        }
    }

    let id = format!("{:04X}:{:04X}", hid.dwVendorId, hid.dwProductId);
    let name = product_name(handle).unwrap_or_else(|| format!("Controller {id}"));
    Some(Device {
        id,
        name,
        preparsed,
        hat,
        held: Held::default(),
        primed: false,
    })
}

/// The product string, read from the device itself. Opened with no access rights, which is all
/// `HidD_GetProductString` needs and which works on devices another program holds open.
unsafe fn product_name(handle: HANDLE) -> Option<String> {
    let mut size = 0u32;
    GetRawInputDeviceInfoW(Some(handle), RIDI_DEVICENAME, None, &mut size);
    let mut path = vec![0u16; size as usize + 1];
    if GetRawInputDeviceInfoW(Some(handle), RIDI_DEVICENAME, Some(path.as_mut_ptr() as _), &mut size) == u32::MAX {
        return None;
    }
    let file = CreateFileW(
        PCWSTR(path.as_ptr()),
        0,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        None,
        OPEN_EXISTING,
        FILE_FLAGS_AND_ATTRIBUTES(0),
        None,
    )
    .ok()?;
    let mut name = [0u16; 128];
    let ok = HidD_GetProductString(file, name.as_mut_ptr() as _, (name.len() * 2) as u32);
    let _ = CloseHandle(file);
    if !ok {
        return None;
    }
    let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    let name = String::from_utf16_lossy(&name[..len]).trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// Every game controller connected right now, for `clipper-capture pads`.
pub fn list() -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    unsafe {
        let mut count = 0u32;
        let entry = std::mem::size_of::<RAWINPUTDEVICELIST>() as u32;
        GetRawInputDeviceList(None, &mut count, entry);
        let mut list = vec![RAWINPUTDEVICELIST::default(); count as usize];
        if GetRawInputDeviceList(Some(list.as_mut_ptr()), &mut count, entry) == u32::MAX {
            return out;
        }
        for item in &list[..count as usize] {
            if item.dwType != RIM_TYPEHID {
                continue;
            }
            let mut info = RID_DEVICE_INFO {
                cbSize: std::mem::size_of::<RID_DEVICE_INFO>() as u32,
                ..Default::default()
            };
            let mut size = info.cbSize;
            GetRawInputDeviceInfoW(Some(item.hDevice), RIDI_DEVICEINFO, Some(&mut info as *mut _ as _), &mut size);
            let hid = info.Anonymous.hid;
            if hid.usUsagePage != PAGE_GENERIC || !GAME_USAGES.contains(&hid.usUsage) {
                continue;
            }
            let Some(device) = describe(item.hDevice) else { continue };
            let pp = PHIDP_PREPARSED_DATA(device.preparsed.as_ptr() as isize);
            let mut caps = HIDP_CAPS::default();
            let _ = HidP_GetCaps(pp, &mut caps);
            let mut buttons = 0u32;
            if caps.NumberInputButtonCaps > 0 {
                let mut bc = vec![HIDP_BUTTON_CAPS::default(); caps.NumberInputButtonCaps as usize];
                let mut n = caps.NumberInputButtonCaps;
                if HidP_GetButtonCaps(HidP_Input, bc.as_mut_ptr(), &mut n, pp) == HIDP_STATUS_SUCCESS {
                    for b in &bc[..n as usize] {
                        if b.UsagePage == PAGE_BUTTON {
                            buttons += if b.IsRange {
                                (b.Anonymous.Range.UsageMax - b.Anonymous.Range.UsageMin + 1) as u32
                            } else {
                                1
                            };
                        }
                    }
                }
            }
            out.push(serde_json::json!({
                "id": device.id,
                "name": device.name,
                "usage": hid.usUsage,
                "buttons": buttons,
                "hat": device.hat.is_some(),
            }));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_round_trip() {
        let press = Press {
            device: "346E:0004".into(),
            name: "MOZA R5 Base".into(),
            control: "Button 12".into(),
        };
        let bind = Bind::parse(&press.spec()).unwrap();
        assert!(bind.matches(&press));
        assert_eq!(bind.control, "Button 12");
        // A name with the separator in it still parses: the control is the last part.
        let bind = Bind::parse("Wheel / Rim / Hat Up-Left [ABCD:0001]").unwrap();
        assert_eq!((bind.device.as_str(), bind.control.as_str()), ("ABCD:0001", "Hat Up-Left"));
        assert!(Bind::parse("Ctrl+Alt+F12").is_none());
        assert!(Bind::parse("").is_none());
    }

    #[test]
    fn hats_read_clockwise_from_up() {
        assert_eq!(hat_direction(0, 0, 7), Some("Up"));
        assert_eq!(hat_direction(2, 0, 7), Some("Right"));
        assert_eq!(hat_direction(8, 0, 7), None); // centred
        assert_eq!(hat_direction(1, 1, 8), Some("Up"));
        assert_eq!(hat_direction(3, 0, 3), Some("Left"));
    }
}
