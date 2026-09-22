//! What to call a game, given the executable that is running it.
//!
//! The executable name is the obvious answer and it is wrong often enough to be the wrong default.
//! Unreal ships a binary named after the internal project rather than the product, and the two are
//! frequently unrelated — on the machine this was written on, Game Bar's own registry holds:
//!
//! | what you play | what the process is called |
//! |---|---|
//! | REMATCH | `RuntimeClient-Win64-Shipping.exe` |
//! | MECCHA CHAMELEON | `PenguinHotel-Win64-Shipping.exe` |
//! | Half-Life: Alyx | `hlvr.exe` |
//! | Assetto Corsa | `acs.exe` |
//! | Helldivers 2 | `helldivers2.exe` |
//!
//! A clips library filed under `RuntimeClient-Win64-Shipping` is not a clips library sorted by
//! game, which was the whole point of the setting.
//!
//! **So the name is taken from wherever somebody already wrote it down**, most authoritative
//! first. Every layer here reads something a human or a store put there on purpose; none of them
//! guesses from the shape of a string.
//!
//! 1. **Steam's own manifest.** `steamapps\appmanifest_*.acf` carries the store name next to the
//!    install directory it belongs to. This is the store's name for the product — "REMATCH",
//!    "ELDEN RING", "Assetto Corsa" — and nothing else on the machine knows better.
//! 2. **The install folder a launcher chose.** Steam without a readable manifest, Epic, GOG,
//!    Ubisoft, EA and Xbox all install into a directory named after the game.
//! 3. **The Unreal layout.** `<install>\<Project>\Binaries\Win64\<Project>-Win64-Shipping.exe` —
//!    two directories above `Binaries` is the install root, which is named after the game even
//!    when the project inside it is not.
//! 4. **The executable's version resource**, when it says something other than its own file name.
//! 5. **The file name**, which is where this started.
//!
//! **The window title is deliberately not in that list.** It is often the best-looking string on
//! offer and it is the only one that changes while you play: a title that reads `Game` in the menu
//! and `Game - Level 3` in play would file one session's clips into two folders, and a folder that
//! splits is worse than a folder that is ugly.

use std::path::{Component, Path};

/// Install-directory markers that mean "the next path segment is the name of a game".
///
/// Each is a launcher's library root. The segment after it is a directory the launcher created and
/// named after the product, which is exactly the string we want.
const LIBRARY_ROOTS: &[&str] = &[
    "steamapps\\common",
    "epic games",
    "gog galaxy\\games",
    "ubisoft game launcher\\games",
    "ea games",
    "origin games",
    "riot games",
    "amazon games\\library",
    "xboxgames",
    "microsoft games",
];

/// Version-resource strings that are true of thousands of programs and therefore name none of them.
const GENERIC: &[&str] = &[
    "unreal engine",
    "unrealgame",
    "unreal",
    "unity",
    "unity player",
    "game",
    "application",
    "shipping",
    "client",
    "launcher",
];

/// The name a clip taken from `path` should be filed under.
///
/// Always returns something usable: the fallbacks end at the executable's own stem, which is what
/// this used to do unconditionally.
pub fn resolve(path: &str) -> String {
    let stem = Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();

    let candidate = steam_name(path)
        .or_else(|| library_name(path))
        .or_else(|| unreal_name(path))
        .or_else(|| version_name(path, &stem));

    match candidate.map(|name| sanitize(&name)) {
        Some(name) if !name.is_empty() => name,
        _ => sanitize(&stem),
    }
}

/// The store's name for whatever is installed at this path, from Steam's manifest for it.
///
/// Steam keeps one `appmanifest_<appid>.acf` per installed app in each library's `steamapps`
/// folder, and it holds both `installdir` — the folder under `common\` — and `name`, the store
/// listing. Matching one to the other is the whole trick, and it is why this beats the folder name:
/// the folder is `Rematch` and `assettocorsa`, the manifest says `REMATCH` and `Assetto Corsa`.
fn steam_name(path: &str) -> Option<String> {
    let lower = path.to_ascii_lowercase().replace('/', "\\");
    let marker = lower.find("\\steamapps\\common\\")?;
    let steamapps = &path[..marker + "\\steamapps".len()];
    let rest = &path[marker + "\\steamapps\\common\\".len()..];
    let install_dir = rest.split('\\').next()?.to_string();
    if install_dir.is_empty() {
        return None;
    }

    let Ok(listing) = std::fs::read_dir(steamapps) else {
        // No manifest to read is not a reason to throw away the folder name, which is already
        // better than the executable.
        return Some(install_dir);
    };
    for entry in listing.flatten() {
        let file = entry.file_name();
        let file = file.to_string_lossy();
        if !file.to_ascii_lowercase().starts_with("appmanifest_") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let (mut name, mut dir) = (None, None);
        for (key, value) in vdf_pairs(&text) {
            match key.as_str() {
                "name" if name.is_none() => name = Some(value),
                "installdir" if dir.is_none() => dir = Some(value),
                _ => {}
            }
        }
        match (name, dir) {
            (Some(name), Some(dir)) if dir.eq_ignore_ascii_case(&install_dir) && !name.is_empty() => {
                return Some(name)
            }
            _ => {}
        }
    }

    // A library with no readable manifest still named the folder after the game.
    Some(install_dir)
}

/// Valve's KeyValues format is nested, but every leaf we want is `"key"<tab>"value"` on one line,
/// and the keys we want are unique enough that depth does not matter. Four quoted fields or more on
/// a line means it is an object header we do not care about.
fn vdf_pairs(text: &str) -> impl Iterator<Item = (String, String)> + '_ {
    text.lines().filter_map(|line| {
        let parts: Vec<&str> = line.split('"').collect();
        if parts.len() < 5 {
            return None;
        }
        Some((parts[1].to_ascii_lowercase(), parts[3].to_string()))
    })
}

/// The directory a launcher created for this game, for the launchers that do it predictably.
fn library_name(path: &str) -> Option<String> {
    let normalised = path.replace('/', "\\");
    let lower = normalised.to_ascii_lowercase();
    for root in LIBRARY_ROOTS {
        let marker = format!("\\{root}\\");
        if let Some(at) = lower.find(&marker) {
            let rest = &normalised[at + marker.len()..];
            let segment = rest.split('\\').next().unwrap_or("");
            // The last segment is the executable itself, which means the marker was the game's own
            // folder rather than a library root.
            if !segment.is_empty() && rest.contains('\\') {
                return Some(segment.to_string());
            }
        }
    }
    None
}

/// Unreal's fixed layout: `<install root>\<Project>\Binaries\<platform>\<Project>-<platform>.exe`.
///
/// Two above `Binaries`, not one. One above is the project, which is the name we are trying to get
/// away from — `Runtime` for REMATCH, `Chameleon` for MECCHA CHAMELEON. Two above is the install
/// root, which whoever packaged the game named after the game.
fn unreal_name(path: &str) -> Option<String> {
    let path = Path::new(path);
    let segments: Vec<String> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().to_string()),
            _ => None,
        })
        .collect();
    let at = segments
        .iter()
        .position(|s| s.eq_ignore_ascii_case("binaries"))?;
    let name = segments.get(at.checked_sub(2)?)?;
    // Two above `Binaries` is the install root for a packaged game and somebody's shelf for a game
    // that was unpacked loose. Naming a folder `Games` would be worse than naming it after the
    // executable, so a container name disqualifies the answer rather than being returned.
    const CONTAINERS: &[&str] = &[
        "games", "game", "program files", "program files (x86)", "common", "steamapps", "bin",
        "binaries", "desktop", "downloads", "documents",
    ];
    if CONTAINERS.contains(&name.to_ascii_lowercase().as_str()) {
        return None;
    }
    Some(name.clone())
}

/// `ProductName`, then `FileDescription`, from the executable's version resource.
///
/// Rejected when it is blank, generic, or just the file name wearing a suit — a version resource
/// that says `RuntimeClient` has told us nothing we did not already have.
fn version_name(path: &str, stem: &str) -> Option<String> {
    let usable = |value: String| -> Option<String> {
        let trimmed = value.trim().to_string();
        let compact = trimmed.to_ascii_lowercase().replace([' ', '-', '_'], "");
        let stem_compact = stem.to_ascii_lowercase().replace([' ', '-', '_'], "");
        if trimmed.is_empty()
            || compact == stem_compact
            || stem_compact.starts_with(&compact)
            || GENERIC.contains(&trimmed.to_ascii_lowercase().as_str())
        {
            None
        } else {
            Some(trimmed)
        }
    };
    version_string(path, "ProductName")
        .and_then(&usable)
        .or_else(|| version_string(path, "FileDescription").and_then(&usable))
}

fn version_string(path: &str, field: &str) -> Option<String> {
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
    };

    unsafe {
        let wide = HSTRING::from(path);
        let size = GetFileVersionInfoSizeW(&wide, None);
        if size == 0 {
            return None;
        }
        let mut block = vec![0u8; size as usize];
        GetFileVersionInfoW(&wide, None, size, block.as_mut_ptr() as *mut _).ok()?;

        // The strings live under a language/codepage the file chooses, and it is not always the
        // one you would guess, so the translation table is the only reliable way in.
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        let translation = HSTRING::from(r"\VarFileInfo\Translation");
        if !VerQueryValueW(
            block.as_ptr() as *const _,
            PCWSTR(translation.as_ptr()),
            &mut ptr,
            &mut len,
        )
        .as_bool()
            || len < 4
        {
            return None;
        }
        let langs = std::slice::from_raw_parts(ptr as *const u16, (len / 2) as usize);

        for pair in langs.chunks_exact(2) {
            let key = HSTRING::from(format!(
                "\\StringFileInfo\\{:04x}{:04x}\\{}",
                pair[0], pair[1], field
            ));
            let mut value = std::ptr::null_mut();
            let mut chars = 0u32;
            if VerQueryValueW(
                block.as_ptr() as *const _,
                PCWSTR(key.as_ptr()),
                &mut value,
                &mut chars,
            )
            .as_bool()
                && chars > 1
            {
                let text = std::slice::from_raw_parts(value as *const u16, chars as usize - 1);
                let text = String::from_utf16_lossy(text);
                if !text.trim().is_empty() {
                    return Some(text);
                }
            }
        }
    }
    None
}

/// Turn a product name into something Windows will accept as a folder.
///
/// Store names carry things that paths will not: `™`, `®`, colons in subtitles. Dropping them is
/// better than letting the save fail, and better than inventing a substitution nobody asked for —
/// a colon becomes a dash because that is what a person writing the folder by hand would do.
fn sanitize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        match ch {
            ':' => out.push_str(" -"),
            '<' | '>' | '"' | '/' | '\\' | '|' | '?' | '*' => {}
            '\u{2122}' | '\u{00ae}' | '\u{00a9}' => {}
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
    // Collapse the runs of spaces that dropping characters just created, and lose the trailing
    // dots and spaces Windows silently strips from directory names anyway.
    let collapsed = out.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim_end_matches(['.', ' ']).to_string();

    // A device name is a legal string and an illegal directory.
    const DEVICES: &[&str] = &[
        "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "lpt1", "lpt2", "lpt3",
    ];
    if DEVICES.contains(&trimmed.to_ascii_lowercase().as_str()) {
        return format!("{trimmed}_");
    }
    trimmed.chars().take(64).collect::<String>().trim_end().to_string()
}
