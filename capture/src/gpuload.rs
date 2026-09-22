//! Which processes are actually driving the GPU.
//!
//! This exists to answer one question the window manager cannot: *is the thing in front of me a
//! game?* Covering the screen is not an answer — a maximised terminal covers the screen, and so
//! does a chat window someone hit F11 in — and asking the user to maintain a list is asking them
//! to do the job by hand.
//!
//! What a game does that a text editor does not is keep the GPU's 3D engine busy, continuously,
//! for as long as it is in front. Windows already measures exactly that per process: it is the
//! number Task Manager puts in its GPU column, published as the performance counter
//! `\GPU Engine(pid_1234_..._engtype_3D)\Utilization Percentage`. We read the same counter.
//!
//! Notes that matter:
//!
//! * The counter is a rate, so PDH needs two collections spaced apart before it means anything.
//!   The first sample after opening the query is always zero and is thrown away.
//! * One process shows up once per engine and per adapter — a machine with an iGPU and a discrete
//!   card lists both, and a modern driver exposes a dozen 3D engine instances. They are summed.
//! * Counter names are localised; `PdhAddEnglishCounterW` is what makes this work on a Windows
//!   that is not in English.
//! * This is a diagnostic API and it is allowed to be missing or broken. Every failure path here
//!   ends in "no data", never in an error the recorder has to care about — the classifier falls
//!   back to the window heuristic and the user's lists, which is where it started.

use std::collections::{HashMap, HashSet};

use windows::core::HSTRING;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
    PdhOpenQueryW, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY,
    PDH_MORE_DATA,
};

const PATH: &str = r"\GPU Engine(*)\Utilization Percentage";

pub struct GpuLoad {
    query: PDH_HQUERY,
    counter: PDH_HCOUNTER,
    /// The first collection of a rate counter has nothing to rate against.
    primed: bool,
}

impl GpuLoad {
    pub fn open() -> Option<GpuLoad> {
        unsafe {
            let mut query = PDH_HQUERY::default();
            if PdhOpenQueryW(None, 0, &mut query) != 0 {
                return None;
            }
            let mut counter = PDH_HCOUNTER::default();
            if PdhAddEnglishCounterW(query, &HSTRING::from(PATH), 0, &mut counter) != 0 {
                let _ = PdhCloseQuery(query);
                return None;
            }
            // Prime it: this one returns nothing useful, and without it the next one is empty too.
            let _ = PdhCollectQueryData(query);
            Some(GpuLoad {
                query,
                counter,
                primed: false,
            })
        }
    }

    /// 3D engine utilisation since the previous call, summed per process id. Percent, and it can
    /// legitimately exceed 100 on a machine with several engines busy at once.
    pub fn sample(&mut self) -> HashMap<u32, f64> {
        let mut out = HashMap::new();
        unsafe {
            if PdhCollectQueryData(self.query) != 0 {
                return out;
            }
            if !self.primed {
                self.primed = true;
                return out;
            }

            let mut size = 0u32;
            let mut count = 0u32;
            // The documented way to learn the buffer size: ask with none and expect MORE_DATA.
            if PdhGetFormattedCounterArrayW(
                self.counter,
                PDH_FMT_DOUBLE,
                &mut size,
                &mut count,
                None,
            ) != PDH_MORE_DATA
            {
                return out;
            }

            let mut buffer = vec![0u8; size as usize];
            let items = buffer.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
            if PdhGetFormattedCounterArrayW(
                self.counter,
                PDH_FMT_DOUBLE,
                &mut size,
                &mut count,
                Some(items),
            ) != 0
            {
                return out;
            }

            for i in 0..count as usize {
                let item = &*items.add(i);
                let Ok(name) = item.szName.to_string() else {
                    continue;
                };
                let Some((pid, engine)) = parse_instance(&name) else {
                    continue;
                };
                if engine != "3D" {
                    continue;
                }
                *out.entry(pid).or_insert(0.0) += item.FmtValue.Anonymous.doubleValue;
            }
        }
        out
    }
}

/// Every process descended from `root`, including `root` itself.
///
/// The foreground window belongs to one process and the GPU work is not always done by that one.
/// Anything built on Chromium renders in a separate GPU child process, so a window that is pinning
/// the 3D engine reads as zero if you only ask about the process that owns it — measured at exactly
/// that, 0%, on a full-screen WebGL window. Games are usually single-process and would not care,
/// but a wrapper or launcher that keeps its window in one process and its renderer in another is
/// common enough that asking about the tree is the only answer that holds.
pub fn process_tree(root: u32) -> HashSet<u32> {
    let mut tree = HashSet::new();
    tree.insert(root);

    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return tree;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                children
                    .entry(entry.th32ParentProcessID)
                    .or_default()
                    .push(entry.th32ProcessID);
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = windows::Win32::Foundation::CloseHandle(snapshot);
    }

    // Breadth-first, and guarded by the visited set: parent ids are recycled, and a cycle here
    // would hang the foreground poll.
    let mut queue = vec![root];
    while let Some(pid) = queue.pop() {
        if let Some(kids) = children.get(&pid) {
            for &kid in kids {
                if tree.insert(kid) {
                    queue.push(kid);
                }
            }
        }
    }
    tree
}

impl Drop for GpuLoad {
    fn drop(&mut self) {
        unsafe {
            let _ = PdhCloseQuery(self.query);
        }
    }
}

/// `pid_12345_luid_0x00000000_0x0001775C_phys_0_eng_3_engtype_3D` -> (12345, "3D").
///
/// PDH appends `#1`, `#2` to instance names it has seen more than once, so the engine type is
/// taken as everything after `engtype_` up to a `#` rather than to the end of the string.
fn parse_instance(name: &str) -> Option<(u32, String)> {
    let pid = name.strip_prefix("pid_")?;
    let pid = pid[..pid.find('_')?].parse().ok()?;
    let engine = name.rsplit_once("engtype_")?.1;
    let engine = engine.split('#').next().unwrap_or(engine);
    Some((pid, engine.to_string()))
}
