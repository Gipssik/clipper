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

---

## Supported formats

Any format ffmpeg supports: `.mp4`, `.mov`, `.avi`, `.mkv`, `.webm`, `.wmv`, `.flv`, `.m4v`, `.mts`, `.m2ts`

---

## Notes

- Trimming uses **stream copy** (`-c copy`) — it's near-instant and lossless. There may be slight inaccuracy at the cut points for non-keyframe boundaries (this is a fundamental video encoding constraint, not a bug).
- No re-encoding means no quality loss and no waiting.
