//! What is in front, and whether it counts as a game.
//!
//! Two jobs. In `game` record mode this decides whether the encoder should be running at all, so
//! that reading email does not burn encode silicon. In both modes it names the clip's subfolder,
//! which is what gives Clipper per-game categories for free — `folder:scan` already treats a
//! subfolder as a category.
//!
//! **Covering the screen is not enough, and that was the bug.** The first version called anything
//! that filled the monitor a game, which is true of a maximised terminal, a chat window someone
//! pressed F11 in, and a full-screen browser. Every one of those got its own folder in the clips
//! library, which is not what "a folder per game" means to anyone.
//!
//! So the question is asked in layers, most certain first:
//!
//! 1. **The user's own lists.** An explicit exclude beats an explicit include beats everything
//!    below. Nothing here is clever enough to be worth overruling a person.
//! 2. **A built-in list of things that are never games** — the shell, browsers, chat apps,
//!    terminals, editors, launchers, media players. These are exactly the programs people run
//!    full-screen, so the fullscreen rule alone gets all of them wrong.
//! 3. **Windows already knows.** `HKCU\System\GameConfigStore\Children` is where Game Bar records
//!    every executable it has identified as a game, by full path. On the machine this was written
//!    on it held two entries, both of them games, and nothing else. When the answer is in there it
//!    beats anything we could infer.
//! 4. **What the process is doing to the GPU.** A game keeps the 3D engine busy continuously; a
//!    text editor does not, whatever size its window is. `gpuload.rs` reads the same per-process
//!    counter that Task Manager's GPU column shows.
//!
//! **Two questions, two standards of proof.** `is_game` decides the folder a clip is filed under and
//! wants to be right; `worth_recording` decides whether the recorder runs at all in `game` mode and
//! wants to be generous. A clip in the wrong folder can be moved. A clip that was never recorded
//! cannot. So anything full-screen that is not on the never-a-game list keeps the recorder going,
//! while only what the layers above can justify gets a folder of its own.
//!
//! The GPU answer is *latched per foreground instance*: a second and a half of continuous load
//! promotes the window to a game, and that verdict then holds until something else takes focus.
//! Without the latch, a game sitting on a pause menu would fall under the threshold and stop the
//! recorder — precisely the moment somebody is about to want the last minute back. Without the
//! "continuous" part, one frame of a window animation would be enough to name a folder after a
//! chat client.

use std::collections::{HashMap, HashSet};

use windows::Win32::Foundation::{CloseHandle, HWND, MAX_PATH, RECT};
use windows::Win32::Graphics::Gdi::{MonitorFromWindow, HMONITOR, MONITOR_DEFAULTTONULL};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowRect, GetWindowThreadProcessId,
};

use crate::gpuload::GpuLoad;

/// Percent of the 3D engine that separates "drawing a game" from "drawing a window". Idle desktop
/// apps sit near zero; a game at 60 fps does not drop below this even on a menu.
const GPU_GAME_PERCENT: f64 = 10.0;

/// Consecutive polls above that line before the answer is believed. Half a second apart, so this
/// is a second and a half of continuous work.
///
/// The threshold alone is not enough in either direction. Set high enough that one scroll of a
/// heavy page cannot reach it, it also excludes games that are vsync-capped on a fast card. Set low
/// enough to catch those, a single animation frame in a desktop app trips it — and because the
/// answer latches, one spike would mean a folder named after a chat client. Requiring the load to
/// *stay* up separates the two cleanly: no window animation lasts a second and a half, and no game
/// fails to.
const GPU_GAME_SAMPLES: u32 = 3;

/// Never a game, whatever size its window happens to be.
///
/// Everything here is something people genuinely run full-screen, which is why the fullscreen rule
/// alone got all of them wrong. The list is about *categories of program* rather than any one
/// person's habits — anything missing is one line in the exclude list, which is what that is for.
const NEVER: &[&str] = &[
    // The shell itself.
    "explorer", "searchhost", "shellexperiencehost", "startmenuexperiencehost",
    "applicationframehost", "textinputhost", "lockapp", "taskmgr", "systemsettings",
    "clipper", "electron",
    // Browsers.
    "chrome", "msedge", "firefox", "opera", "opera_gx", "brave", "vivaldi", "zen", "arc",
    // Chat and calls.
    "telegram", "discord", "slack", "teams", "ms-teams", "whatsapp", "signal", "zoom", "skype",
    // Terminals and shells.
    "windowsterminal", "wt", "cmd", "powershell", "pwsh", "conhost", "alacritty", "wezterm-gui",
    "mintty", "git-bash",
    // Editors, IDEs, and the heavyweight creative tools that would otherwise pass the GPU test.
    "code", "cursor", "devenv", "rider64", "idea64", "pycharm64", "webstorm64", "clion64",
    "goland64", "sublime_text", "notepad++", "notepad", "obsidian", "blender", "unityhub",
    "unity", "unrealeditor", "photoshop", "illustrator", "afterfx", "resolve", "obs64", "obs32",
    // Launchers. The game one of these starts is a different executable.
    "steam", "steamwebhelper", "epicgameslauncher", "battle.net", "riotclientservices",
    "galaxyclient", "ubisoftconnect", "upc", "eadesktop", "playnite.desktopapp", "nvidia app",
    // Media players and music.
    "vlc", "mpc-hc64", "mpv", "spotify", "potplayermini64",
];

#[derive(Clone, Default, PartialEq)]
pub struct Foreground {
    /// Executable name, e.g. `Cyberpunk2077.exe`. Empty when nothing could be determined.
    pub process: String,
    /// Full path to that executable. Empty when it could not be read — a game running at higher
    /// integrity than us will still give up its name but not always its path.
    pub path: String,
    /// What a person calls this game, which is very often not what the executable is called. See
    /// `gamename.rs`. Falls back to the executable's stem.
    pub title: String,
    pub pid: u32,
    pub fullscreen: bool,
    /// Confident enough to name a folder after it.
    pub is_game: bool,
    /// Generous enough to keep the recorder running in `game` mode.
    ///
    /// These are deliberately two different questions with two different standards of proof. Being
    /// wrong about the folder costs a clip filed under `Desktop`; being wrong about whether to
    /// record costs the clip. So anything filling the screen that is not on a list of things that
    /// are definitely not games is recorded, while only what we can actually justify gets its own
    /// folder.
    pub worth_recording: bool,
    /// Why, in a few words. Shown in the settings panel so a wrong answer is visible rather than
    /// mysterious, and so the fix — the include or exclude list — is the obvious next move.
    pub reason: String,
    /// Peak 3D engine use seen since this window took focus, in percent. Negative when the counter
    /// is not available on this machine.
    pub gpu_percent: f64,
}

impl Foreground {
    /// The subfolder a clip taken now should land in.
    ///
    /// Only a game gets a folder of its own. Everything else shares one name: the point of the
    /// setting is a library sorted by game, and a folder called `WindowsTerminal` holding one clip
    /// is not that.
    ///
    /// The name is `title`, not the executable — see `gamename.rs`. A folder called
    /// `RuntimeClient-Win64-Shipping` is no more a game's name than `WindowsTerminal` is.
    pub fn category(&self) -> String {
        if self.is_game && !self.title.is_empty() {
            self.title.clone()
        } else {
            "Desktop".to_string()
        }
    }
}

/// The state the classifier carries between polls: the GPU counter, the peak reading for whatever
/// is in front now, and everything already proven to be a game this session.
pub struct Watcher {
    gpu: Option<GpuLoad>,
    registered: HashSet<String>,
    /// The foreground instance the two counters below belong to, keyed by pid and full path.
    current: (u32, String),
    /// The display name for that instance, resolved once when it took focus. Resolving reads the
    /// disk, and the answer cannot change while the same process holds the foreground.
    title: String,
    /// Names already worked out this session, so alt-tabbing between two games does not re-read
    /// Steam's manifests every time focus moves.
    names: HashMap<String, String>,
    /// Consecutive samples at or above the threshold, and the highest seen. The count decides;
    /// the peak is only there so the settings panel can say why.
    hot: u32,
    peak_gpu: f64,
    /// Set once `hot` has reached the bar. Latched for the life of this foreground instance: a
    /// game sitting on a pause menu must not fall below the line and stop the recorder, which is
    /// precisely the moment somebody is about to want the last minute back.
    busy: bool,
    /// Processes confirmed as games earlier in this session, so alt-tabbing back is instant rather
    /// than waiting a second for the GPU counter to speak again.
    proven: HashSet<String>,
}

impl Watcher {
    pub fn new() -> Watcher {
        Watcher {
            gpu: GpuLoad::open(),
            registered: registered_games(),
            current: (0, String::new()),
            title: String::new(),
            names: HashMap::new(),
            hot: 0,
            peak_gpu: -1.0,
            busy: false,
            proven: HashSet::new(),
        }
    }

    /// True when the per-process GPU counter is available. When it is not, the classifier falls
    /// back to the window heuristic and says so in its reason.
    pub fn measures_gpu(&self) -> bool {
        self.gpu.is_some()
    }

    /// How many executables Windows itself has on file as games.
    pub fn registered_count(&self) -> usize {
        self.registered.len()
    }

    pub fn poll(
        &mut self,
        monitor: HMONITOR,
        include: &[String],
        exclude: &[String],
        strict: bool,
    ) -> Foreground {
        let window = unsafe { GetForegroundWindow() };
        if window.is_invalid() {
            return Foreground::default();
        }

        let (pid, path) = process_of(window);
        let process = path.rsplit('\\').next().unwrap_or("").to_string();
        let fullscreen = covers_monitor(window, monitor);

        // A new window in front starts the measurement over. What the last one did to the GPU says
        // nothing about whether this one is a game.
        if (pid, path.clone()) != self.current {
            self.current = (pid, path.clone());
            self.title = if path.is_empty() {
                String::new()
            } else {
                let key = path.to_ascii_lowercase();
                match self.names.get(&key) {
                    Some(known) => known.clone(),
                    None => {
                        let resolved = crate::gamename::resolve(&path);
                        self.names.insert(key, resolved.clone());
                        resolved
                    }
                }
            };
            self.hot = 0;
            self.busy = false;
            self.peak_gpu = if self.gpu.is_some() { 0.0 } else { -1.0 };
        }
        let loads: HashMap<u32, f64> = match &mut self.gpu {
            Some(g) => g.sample(),
            None => HashMap::new(),
        };
        if self.gpu.is_some() {
            let tree = crate::gpuload::process_tree(pid);
            let now: f64 = loads
                .iter()
                .filter(|(p, _)| tree.contains(p))
                .map(|(_, v)| *v)
                .sum();
            self.peak_gpu = self.peak_gpu.max(now);
            self.hot = if now >= GPU_GAME_PERCENT { self.hot + 1 } else { 0 };
            self.busy |= self.hot >= GPU_GAME_SAMPLES;
        }

        let stem = process.trim_end_matches(".exe").to_ascii_lowercase();
        let listed = |list: &[String]| {
            list.iter()
                .any(|p| p.trim_end_matches(".exe").eq_ignore_ascii_case(&stem))
        };

        let (is_game, reason) = if process.is_empty() {
            (false, "nothing in front".to_string())
        } else if listed(exclude) {
            (false, "on your never-a-game list".to_string())
        } else if listed(include) {
            (true, "on your always-a-game list".to_string())
        } else if !strict {
            // The old rule, kept as an escape hatch for anything the classifier gets wrong in a
            // way the two lists cannot express.
            (fullscreen, "fills the screen".to_string())
        } else if NEVER.contains(&stem.as_str()) {
            (false, "not a game".to_string())
        } else if self.registered.contains(&stem) {
            (true, "Windows has this on file as a game".to_string())
        } else if self.proven.contains(&stem) {
            (true, "seen driving the GPU full-screen earlier".to_string())
        } else if !fullscreen {
            (false, "in a window".to_string())
        } else if self.gpu.is_none() {
            (true, "fills the screen".to_string())
        } else if self.busy {
            (
                true,
                format!("full-screen and driving the GPU ({:.0}%)", self.peak_gpu),
            )
        } else {
            (
                false,
                "full-screen, but not working the GPU like a game".to_string(),
            )
        };

        if is_game && strict && !stem.is_empty() {
            self.proven.insert(stem.clone());
        }

        let worth_recording = if listed(exclude) {
            false
        } else {
            is_game || (fullscreen && !NEVER.contains(&stem.as_str()) && !process.is_empty())
        };

        Foreground {
            process,
            path,
            title: self.title.clone(),
            pid,
            fullscreen,
            is_game,
            worth_recording,
            reason,
            gpu_percent: self.peak_gpu,
        }
    }
}

/// Executables Game Bar has already identified as games, lower-cased and without the extension.
///
/// Read once at startup. It only changes when Windows first meets a new game, which is rare enough
/// that re-reading it on a timer would be pure noise; a game not in here is caught by the GPU test
/// on its first run anyway.
fn registered_games() -> HashSet<String> {
    registered_paths()
        .iter()
        .filter_map(|path| path.rsplit('\\').next())
        .map(|exe| exe.trim_end_matches(".exe").to_ascii_lowercase())
        .filter(|stem| !stem.is_empty())
        .collect()
}

/// The full paths behind those entries. Kept separate because `clipper-capture names` reports them,
/// and because a path is what `gamename::resolve` needs.
pub fn registered_paths() -> Vec<String> {
    use windows::core::{HSTRING, PCWSTR, PWSTR};
    use windows::Win32::System::Registry::{
        RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER,
        KEY_READ, REG_VALUE_TYPE,
    };

    let mut games = Vec::new();
    unsafe {
        let mut root = HKEY::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            &HSTRING::from(r"System\GameConfigStore\Children"),
            None,
            KEY_READ,
            &mut root,
        )
        .is_err()
        {
            return games;
        }

        for index in 0.. {
            let mut name = [0u16; 128];
            let mut len = name.len() as u32;
            if RegEnumKeyExW(
                root,
                index,
                Some(PWSTR(name.as_mut_ptr())),
                &mut len,
                None,
                None,
                None,
                None,
            )
            .is_err()
            {
                break;
            }

            let mut child = HKEY::default();
            if RegOpenKeyExW(root, PCWSTR(name.as_ptr()), None, KEY_READ, &mut child).is_err() {
                continue;
            }

            let mut buffer = [0u16; MAX_PATH as usize];
            let mut bytes = (buffer.len() * 2) as u32;
            let mut kind = REG_VALUE_TYPE::default();
            let ok = RegQueryValueExW(
                child,
                &HSTRING::from("MatchedExeFullPath"),
                None,
                Some(&mut kind),
                Some(buffer.as_mut_ptr() as *mut u8),
                Some(&mut bytes),
            )
            .is_ok();
            let _ = RegCloseKey(child);
            if !ok {
                continue;
            }

            let chars = (bytes as usize / 2).min(buffer.len());
            let path = String::from_utf16_lossy(&buffer[..chars])
                .trim_end_matches('\0')
                .to_string();
            if !path.is_empty() {
                games.push(path);
            }
        }
        let _ = RegCloseKey(root);
    }
    games
}

/// The pid in front and the full path to its executable.
///
/// The full path, not the file name: `gamename::resolve` needs the whole thing, because almost
/// everything that knows what a game is called wrote it into a directory somewhere along it.
fn process_of(window: HWND) -> (u32, String) {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
    if pid == 0 {
        return (0, String::new());
    }

    unsafe {
        // LIMITED_INFORMATION is enough for the image name and, unlike the full query right, is
        // granted for processes running at higher integrity — which most games launched through a
        // launcher are.
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return (pid, String::new());
        };
        let mut buffer = [0u16; MAX_PATH as usize];
        let mut len = buffer.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            windows::core::PWSTR(buffer.as_mut_ptr()),
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(handle);

        if !ok {
            return (pid, String::new());
        }
        (pid, String::from_utf16_lossy(&buffer[..len as usize]))
    }
}

/// Whether the window fills the monitor we are recording — the cheap proxy for "fullscreen game"
/// that costs one rect comparison rather than a driver query.
fn covers_monitor(window: HWND, monitor: HMONITOR) -> bool {
    unsafe {
        if MonitorFromWindow(window, MONITOR_DEFAULTTONULL) != monitor {
            return false;
        }
        let mut rect = RECT::default();
        if GetWindowRect(window, &mut rect).is_err() {
            return false;
        }

        let mut info = windows::Win32::Graphics::Gdi::MONITORINFO {
            cbSize: std::mem::size_of::<windows::Win32::Graphics::Gdi::MONITORINFO>() as u32,
            ..Default::default()
        };
        if !windows::Win32::Graphics::Gdi::GetMonitorInfoW(monitor, &mut info).as_bool() {
            return false;
        }
        let m = info.rcMonitor;

        // A pixel of slop each way: borderless windows sometimes sit a hair outside.
        rect.left <= m.left + 1
            && rect.top <= m.top + 1
            && rect.right >= m.right - 1
            && rect.bottom >= m.bottom - 1
    }
}
