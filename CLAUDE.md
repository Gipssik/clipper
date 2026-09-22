# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Clipper is a Windows Electron app for browsing a folder of game clips, trimming them, and
re-encoding them for sharing — plus an always-on instant replay recorder.

**Two halves, and they are not the same kind of code.** The app is four files: `src/main.js`,
`src/preload.js`, `src/renderer.js`, `src/index.html`. No framework, no bundler, no transpile step —
`npm start` runs `src/` directly, and electron-builder is only involved when packaging. The recorder
is a separate Rust crate in `capture/` that builds to `clipper-capture.exe`, talks Direct3D 11 and
Media Foundation, and is driven over a named pipe. **`capture/DESIGN.md` is the document for that
half** — it is long, and it is the reference, not this file.

## Commands

```bash
npm start              # run the app
npm run build          # electron-builder --win --x64 -> dist/ (NSIS installer + portable .exe)
npm run build-portable # portable .exe only
node --check src/main.js   # the only static check available; there is no linter or test runner

cd capture && cargo build --release      # the recorder; copy the exe to capture-bin/ to ship it
python assets/source/mkicons.py          # regenerate every icon from assets/source/icon.png
```

`ffmpeg-bin/ffmpeg.exe` is tracked in **Git LFS** (~130 MB) and bundled via electron-builder
`extraResources`. A clone without LFS gets a pointer file and every ffmpeg call fails with a
confusing error. `capture-bin/clipper-capture.exe` is the same story — `*.exe` is an LFS filter, so
rebuilding the recorder means committing an LFS object, not a 1.2 MB blob.

## Architecture

**ffmpeg is the only binary, and there is no ffprobe.** All clip metadata comes from parsing the
stream banner ffmpeg writes to stderr for any input it opens — `parseProbe()` and
`parseColorInfo()` in `main.js`. Adding a metadata field means extending that parser, not reaching
for another tool. `parseColorInfo` anchors on the frame dimensions (`..., yuv420p10le(tv,
bt2020nc/bt2020/smpte2084), 3840x2160`) because a codec profile or fourcc would otherwise match
first.

**Capabilities are probed by doing, not by asking.** `detectEncoders()` runs a throwaway one-frame
encode per candidate, because an `-encoders` listing only proves the build has the encoder, not
that this machine has the silicon. `detectFilters()` greps `-filters` for `zscale`/`tonemap`, since
tone mapping needs libzimg and plenty of builds ship without it. Both are memoized promises, warmed
on `app.whenReady`, and exposed over IPC.

**One pass for anything that touches the picture.** `runEncode()` in `main.js` composes a single
`-vf` chain (scale → fps → tone map) and one encoder invocation, so an export that needs a
downscale *and* a codec change *and* HDR→SDR never stacks generation loss. `runPassthrough()` is
the lossless sibling: a file copy, or `-c copy` into a new container when only the container is
wrong. Never chain encodes to reach a combined result.

**HDR.** A clip is HDR by its transfer curve (`smpte2084` / `arib-std-b67`), not by bit depth or
gamut — 10-bit bt2020 with an ordinary curve is wide-gamut, not HDR. Tone mapping goes to linear
light, maps bt2020→bt709, applies the operator, and re-tags the output bt709; without that re-tag
ffmpeg copies the source's colour tags and the result still claims to be HDR. The default operator
is `mobius` ("Balanced"), chosen from measurement rather than convention: across real clips `hable`
consistently under-exposes while `mobius` holds brightness with the same saturation recovery.

**Renderer state flow.** `scanAndRender()` refills `allVideos`, then `syncGrid()` reconciles it
against `cardMap` (fullPath → card element). `thumbCache` and `metaCache` are both keyed by
fullPath and both written by exactly one function, `extractThumb()` — a single ffmpeg pass yields
the poster frame and the stream metadata together, which is why the quality badges cost nothing.

`syncGrid()` is the **sole owner of cache invalidation**. It compares each reused card's
`card._video` against the freshly scanned record and calls `refreshCard()` when mtime or size
moved, meaning the file was rewritten in place. Do not delete from `thumbCache`/`metaCache` in
feature code: only `syncGrid` can tell a replace (source changed) from a save-as-new (source
untouched, caches still valid). Card handlers read `card._video`, never the record captured at
build time, for the same reason.

**Export** (`planExport()` in `renderer.js`) compares a clip to the saved preset and picks the
cheapest route: nothing, remux, or one encode. The preset control panel is built by a single
factory, `buildPresetUi()`, and mounted twice — in Settings and in the export modal — so the two
copies cannot drift. The export modal borrows the encode modal's chrome through comma-joined CSS
selectors in `index.html` rather than duplicated rules.

**Icons are generated, not hand-made.** `assets/source/icon.png` is the only artwork; everything
else in `assets/` — `icon.ico`, `icon.png`, `tray.png`, `tray-idle.png` — comes out of
`assets/source/mkicons.py`, which scales with ffmpeg and writes the PNG and ICO containers itself
(no PIL, no ImageMagick on this machine). Edit the master and re-run it; do not hand-edit the
outputs. Two things that script knows and a replacement would have to relearn: electron-builder
**rejects an `.ico` without a 256×256 entry**, and the tray icons get their alpha by
unpremultiplying against the master's black background rather than by colour-keying, which is what
keeps a 16px mark from looking fringed. `assets/source/` is excluded from the package via a
`!assets/source/**` filter — the master is repo history, not payload.

The same `assets/icon.ico` is compiled into `clipper-capture.exe` by `capture/build.rs`, along with
a version resource. That resource is not cosmetic: `FileDescription` is what Task Manager prints in
its Name column, and without it an always-on recorder shows up in somebody's process list as an
anonymous `clipper-capture.exe` with a blank Description — the exact shape of a thing you kill on
sight. It reads **Clipper Instant Replay**, nested under Clipper because `startCaptureDaemon()`
spawns it as an ordinary child (`detached: false`).

**Start with Windows.** `app.setLoginItemSettings` with an explicit `name: 'Clipper'`, because the
default names the registry value after the app user model id and Task Manager's Startup tab then
lists it as `com.clipper.app`. The registration always carries `--hidden`, which `createWindow()`
turns into `show: false`.

Read the state back with **`executableWillLaunchAtLogin`, never `openAtLogin`** — this costs an hour
to rediscover. On Windows `openAtLogin` is computed by comparing the registered command line against
the `args` you pass in, and Electron drops `--hidden` from the args it parses back out of the
registry. The comparison therefore always fails: the entry is written correctly and launches the app
every morning, while the switch in Settings reads off.

**The hotkey is chosen by the daemon, not the settings panel.** Windows consumes Alt+F-key above
every layer Electron can reach, and this was measured rather than guessed: pressing Alt+F10 over
Clipper's window delivers the Alt and then nothing at all — the F10 reaches neither the DOM, nor
`before-input-event`, nor `WM_SYSKEYDOWN` via `hookWindowMessage`. `Alt+G`, `F10` alone and
`Alt+Shift+F8` all arrive normally, so the hole is specifically Alt plus a function key. There is
no fix on the Electron side; do not go looking for one again.

So `replayHotkeyBtn` sends `listen` down the pipe and the daemon installs a `WH_KEYBOARD_LL` hook
(`hotkey.rs`), which runs ahead of all that, and replies with a `hotkey-captured` event. The window
keeps a keydown handler as the fallback for when the daemon is not up; it gets everything except
Alt+F-key. The hook swallows what it captures, so choosing a hotkey cannot also trigger it. Alt+F4
alone is declined on purpose — `RegisterHotKey` would grant it and take the one shortcut everybody
knows away from every window on the machine.

`parse()` and `key_name()` in `hotkey.rs` are inverses over a shared `NAMED` table and there is a
test that says so; a key the daemon can capture must be one it can register.

**A hotkey that cannot be registered has to say so.** Windows gives a global hotkey to one process
at a time, so a combination another program holds is refused outright — Alt+F9 and Alt+F10 belong
to NVIDIA's overlay on a typical gaming machine. The daemon reports this as `hotkeyOk` on every
status, and the panel shows a persistent warning under the field. It is deliberately not a toast:
the setting reads back exactly as the user chose it, so the only clue something is wrong is the one
the field carries.

**Voice.** Every control carries a one-line tip explaining what moving it does to the picture, and
what it costs. Match that when adding UI; a bare label is out of place here.

## Verifying changes

There is no test suite. Verification is done by driving the real app from a harness written to the
scratchpad directory, and by inspecting the media that comes out with `ffmpeg-bin/ffmpeg.exe`.
Always verify with real media before reporting a change as working.

**Write harness files with the Write tool, not a shell heredoc.** JS full of backticks, `${}` and
quotes fights bash quoting and wastes turns.

### Main process only (fast, no window)

Stub `electron` through `Module._load`, require `main.js`, and call its IPC handlers directly.
Runs under plain `node`:

```js
const Module = require('module');
const orig = Module._load;
const handlers = new Map();
Module._load = function (req) {
  if (req === 'electron') return {
    app: { isPackaged: false, commandLine: { appendSwitch() {} },
           getPath: () => require('os').tmpdir(), whenReady: () => Promise.resolve(), on() {} },
    BrowserWindow: class { loadFile() {} },
    ipcMain: { handle: (ch, fn) => handlers.set(ch, fn) },
    dialog: {}, shell: {},
  };
  return orig.apply(this, arguments);
};
require('D:/Work/repos/clipper/src/main.js');

const call = (ch, arg) => handlers.get(ch)({ sender: { isDestroyed: () => true, send() {} } }, arg);
// await call('video:probe', 'C:/clips/x.mp4')
// await call('video:encode', { inputPath, saveMode: 'new', toneMap: 'balanced', ... })
```

Good for the parser, encoder/filter detection, and every encode branch.

### Full app (real window, real renderer)

Run with `./node_modules/.bin/electron <harness.js>`. `renderer.js` is a classic script, so every
top-level function and binding is reachable by name from `executeJavaScript`: `scanAndRender`,
`syncGrid`, `planExport`, `runExport`, `openEncodeModal`, `startExport`, `settings`, `allVideos`,
`cardMap`, `metaCache`.

```js
const { app, shell } = require('electron');
const LIB = 'C:/.../scratchpad/lib';        // a fixture folder, never the user's real clips
shell.showItemInFolder = () => {};          // otherwise every export opens an Explorer window
app.on('browser-window-created', (_, win) => {
  win.webContents.on('console-message', (_e, lvl, msg, line) => {
    if (!/Content Security Policy/.test(msg)) console.log(`[renderer ${lvl}] ${msg} (:${line})`);
  });
  win.webContents.on('did-finish-load', async () => {
    const js = (code) => win.webContents.executeJavaScript(code);
    await js(`(async () => { rootFolder = ${JSON.stringify(LIB)}; await scanAndRender(); })()`);
    await new Promise(r => setTimeout(r, 3000));   // probes are lazy; let them land
    console.log(await js(`JSON.stringify({ /* read the DOM back */ })`));
    app.quit();
  });
});
require('D:/Work/repos/clipper/src/main.js');
setTimeout(() => app.quit(), 120000);       // never leave a window running
```

`win.webContents.capturePage()` gives a PNG to read back when a layout needs eyeballing.

Notes that cost time to rediscover:

- Thumbnail and metadata probes are lazy and driven by `IntersectionObserver`. Wait ~3–4 s after
  `scanAndRender()` before asserting on badges or `metaCache`.
- Fixtures must be **longer than `settings.thumbTime` (default 3 s)** or no still is produced and
  the thumbnail silently stays empty. Make them ~10 s.
- A harness that writes settings touches a **different profile** from the real app. `userData`
  comes from `app.name`, and `electron <harness.js>` finds no package.json, so it writes
  `%APPDATA%/Electron/` while `npm start` writes `%APPDATA%/clipper/`. Good news — the real config
  is never at risk — but it also means a harness reading back `capture.json` or `prefs.json` is
  reading its own copy, and a value that looks wrong there usually is not.
- Build fixtures with `ffmpeg-bin/ffmpeg.exe` into a scratchpad folder; never point a harness at the
  user's real clips folder for anything that writes.
- The `Content Security Policy` warning about the Google Fonts stylesheet is pre-existing noise.
- Verifying output media: check the stream banner for codec, dimensions and colour tags; `cmp` for a
  lossless copy; `-map 0:v -c copy -f md5 -` on both sides to prove a remux kept the bitstream.

## Committing and releasing

Line endings: `core.autocrlf=true`, the index is LF, and most worktree files are already LF. When
patching files with Python, pass `newline=''` to **both** the read and the write —
`io.open(p, encoding='utf-8')` silently converts CRLF→LF on read and will rewrite the whole file.

Release convention, following `v2.1.0`:

```bash
# 1. bump package.json "version" (electron-builder derives artifact names from it)
# 2. commit the bump with the work, or on its own
git commit -am "Release v2.2.0"
git tag v2.2.0
git push origin main --tags

# 3. build both artifacts
npm run build          # -> dist/Clipper 2.2.0.exe  +  dist/Clipper Setup 2.2.0.exe

# 4. publish, attaching both (quote the paths — they contain spaces)
gh release create v2.2.0 \
  --title "v2.2.0 — <short summary>" \
  --notes-file <notes.md> \
  "dist/Clipper 2.2.0.exe" "dist/Clipper Setup 2.2.0.exe"
```

`dist/` is gitignored. Release notes are written for users, not as a changelog: `##` sections per
feature, explaining what it does and what it costs, in the same voice as the README. Confirm with
the user before tagging or pushing — both are outward-facing and hard to undo.
