# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Clipper is a Windows Electron app for browsing a folder of game clips, trimming them, and
re-encoding them for sharing. Four files do everything: `src/main.js`, `src/preload.js`,
`src/renderer.js`, `src/index.html`. No framework, no bundler, no transpile step — `npm start`
runs `src/` directly, and electron-builder is only involved when packaging.

## Commands

```bash
npm start              # run the app
npm run build          # electron-builder --win --x64 -> dist/ (NSIS installer + portable .exe)
npm run build-portable # portable .exe only
node --check src/main.js   # the only static check available; there is no linter or test runner
```

`ffmpeg-bin/ffmpeg.exe` is tracked in **Git LFS** (~130 MB) and bundled via electron-builder
`extraResources`. A clone without LFS gets a pointer file and every ffmpeg call fails with a
confusing error.

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
- A harness that writes settings should back up and restore
  `%APPDATA%/clipper/prefs.json` — it is the live app's config.
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
