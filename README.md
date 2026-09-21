# ✂ Clipper — Video Trim Tool

A fast, minimal desktop app for browsing a folder of video clips, trimming them, and saving or replacing them. Built with Electron + ffmpeg.

---

## Requirements

- **Node.js** v18+ → https://nodejs.org
- **ffmpeg.exe** (see below)
- Windows 10/11 x64

---

## Setup (first time)

### 1. Get ffmpeg

Download a pre-built ffmpeg binary for Windows:

- Go to https://github.com/BtbN/FFmpeg-Builds/releases
- Download `ffmpeg-master-latest-win64-gpl.zip` (or any recent release)
- Extract it
- Copy **`ffmpeg.exe`** from the `bin/` folder inside the zip

Then place it in the `ffmpeg-bin/` folder of this project:

```
clipper/
  ffmpeg-bin/
    ffmpeg.exe    ← put it here
  src/
  package.json
  ...
```

> **Tip:** You can also install ffmpeg system-wide (`winget install ffmpeg` or via Chocolatey: `choco install ffmpeg`) and Clipper will fall back to using it from PATH.

### 2. Install dependencies

Open a terminal in the `clipper/` folder and run:

```bash
npm install
```

### 3. Run the app

```bash
npm start
```

---

## Build a distributable (optional)

To build a Windows installer or portable .exe:

```bash
npm run build
```

Output goes to `dist/`. This creates both an NSIS installer and a portable `.exe`.

For just a portable executable:

```bash
npm run build-portable
```

> Note: `electron-builder` will automatically bundle `ffmpeg-bin/ffmpeg.exe` into the package if present.

---

## How to use

1. Click **Open Folder** — select any folder containing video files
2. Click a clip in the left sidebar to load it
3. Drag the **yellow handles** on the timeline to set trim in/out points
4. Click **Preview trim** to watch just the selected portion
5. When happy:
   - **Save as new** → prompts for a filename/location
   - **Replace original** → overwrites the original file in-place
6. **Show in Explorer** opens the file's location in Windows Explorer

Each card shows a **quality badge** in the corner of its thumbnail — `1080p60`, `720p`, `4K` and so on,
named after the clip's short side so portrait clips read correctly too. Clips encoded with AV1 get an
extra green **AV1** badge, and HDR clips get an orange **HDR** (or **HLG**) one.

---

## Compressing a clip

Open a card's **⋮** menu → **Compress…**. The modal shows the clip's current resolution, codec, bitrate
and size, and gives you:

- **Resolution** — keep the original or drop down the ladder to 1440p / 1080p / 720p / 480p (only rungs
  below the source are offered; upscaling never saves space)
- **Size control** — *Quality (CRF)* to target a look and let the size fall where it may, or
  *Target bitrate* to land under a hard upload limit
- **Framerate**, **audio** (keep / re-encode / strip) and **encoder speed**
- **Encoder** — defaults to your GPU when one is usable, with CPU x264/x265 always selectable

Every control carries a one-line note on what moving it actually does to the picture, and the footer
shows a live **estimated output size** against the original. The estimate is a model, not a promise —
real size depends on how much motion the clip has.

## Converting AV1 → MP4

**Convert to MP4…** only appears in the **⋮** menu for clips that are actually AV1 — for anything else it
would be a lossy round-trip with nothing gained. It decodes the source and re-encodes it to **H.264 in an
.mp4**, which every editor, player and upload target accepts. AV1 clips usually get *larger* — that is the
cost of compatibility, and the modal says so up front.

## Converting HDR → SDR

Game capture on an HDR display records the clip in HDR, graded against a PQ or HLG curve and the wide
bt2020 gamut. Anything that is not an HDR screen ignores that grading and shows the raw values, which is
why the clip looks **grey, flat and washed out** the moment you send it to a friend. Re-encoding alone
does not fix it — the picture has to be converted.

**Convert HDR → SDR…** appears in the **⋮** menu for clips that are actually HDR. It takes the picture
back to linear light, maps the bt2020 gamut to bt709, compresses the brightness into SDR range and
re-encodes to **H.264 in an .mp4**, correctly tagged bt709 so nothing downstream second-guesses it.

Four options, each with a note on what it costs:

| Mode | What it does |
| --- | --- |
| **Keep HDR** | Leaves the dynamic range alone. The output stays HDR — and stays grey on SDR screens. |
| **Balanced** | Holds midtone brightness close to the original and only rolls off the top highlights. The closest match to how the game looked, and the default. |
| **Filmic** | Filmic S-curve that protects detail in skies, explosions and muzzle flashes, at the cost of darkening the whole picture. |
| **Punchy** | Leaves everything below SDR white exactly as graded and hard-clips above it. Most contrast, no highlight detail. |

The same control appears in **Compress** whenever the source is HDR, switched on by default — compressing
an HDR clip to 8-bit without tone mapping is exactly what produces the washed-out result. Tone mapping
needs an ffmpeg with the `zscale` filter (libzimg); builds without it say so instead of offering the
option.

All three actions offer **Save as new** (writes `clip_720p.mp4` / `clip_h264.mp4` / `clip_sdr.mp4` next to the original) and
**Replace original** (keeps the original's name; if the container changes, the old file is removed).
Long encodes show a progress bar with speed and time remaining, and can be cancelled — a cancelled run
leaves nothing behind.

---

## Settings

The **⚙** button in the titlebar opens Settings. Everything saves as you change it.

| Setting | What it does |
| --- | --- |
| **Font size** | Scales every label, button and tip in the app, 80%–140%, with a live preview. Icons and window chrome stay fixed so nothing gets clipped. |
| **Card size** | How wide a clip card gets before the grid wraps — Small / Medium / Large / Huge. Worth turning up on a big monitor. |
| **Play preview on hover** | Turn off if scrolling a large folder feels heavy. |
| **Thumbnail frame** | Which second of each clip to grab its still from. Bump it up if your clips open on a black intro or a loading screen. Changing it re-grabs the visible stills. |
| **Default encoder** | Preselected whenever you open Compress or Convert. *Automatic* prefers your GPU when one is usable. You can still override it per clip. |

---

## Supported formats

Any format ffmpeg supports: `.mp4`, `.mov`, `.avi`, `.mkv`, `.webm`, `.wmv`, `.flv`, `.m4v`, `.mts`, `.m2ts`

---

## Notes

- Trimming uses **stream copy** (`-c copy`) — it's near-instant and lossless. There may be slight inaccuracy at the cut points for non-keyframe boundaries (this is a fundamental video encoding constraint, not a bug).
- No re-encoding means no quality loss and no waiting.
- **Compress** and **Convert**, unlike trimming, do re-encode — they take real time and lose a little
  quality by definition. Hardware encoding (NVENC / QuickSync / AMF) is typically 5–10× faster than CPU
  x264 but produces somewhat larger files at the same quality setting.
- Clipper probes for usable hardware encoders at startup by running a throwaway one-frame encode, so the
  list only offers encoders this machine can actually use.
- HDR detection reads the clip's transfer curve (`smpte2084` for HDR10, `arib-std-b67` for HLG), not just
  its bit depth or gamut — a 10-bit bt2020 clip with an ordinary curve is wide-gamut, not HDR, and is
  left alone.
- Tone mapping is one-way. The SDR copy cannot be turned back into HDR, so keep the original if you still
  want the HDR version.
