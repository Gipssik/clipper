const { app, BrowserWindow, ipcMain, dialog, shell } = require('electron');
const path = require('path');
const fs = require('fs');
const { spawn } = require('child_process');
const os = require('os');

let mainWindow;

app.commandLine.appendSwitch('--disable-renderer-backgrounding');
app.commandLine.appendSwitch('--disable-background-timer-throttling');

function getFfmpegPath() {
  if (app.isPackaged) {
    const ffmpegExe = path.join(process.resourcesPath, 'ffmpeg-bin', 'ffmpeg.exe');
    if (fs.existsSync(ffmpegExe)) return ffmpegExe;
  }
  const localBin = path.join(__dirname, '..', 'ffmpeg-bin', 'ffmpeg.exe');
  if (fs.existsSync(localBin)) return localBin;
  return 'ffmpeg';
}

const prefsPath = path.join(app.getPath('userData'), 'prefs.json');
function loadPrefs() {
  try { return JSON.parse(fs.readFileSync(prefsPath, 'utf8')); }
  catch { return {}; }
}
function savePrefs(prefs) {
  try {
    const existing = loadPrefs();
    fs.writeFileSync(prefsPath, JSON.stringify({ ...existing, ...prefs }, null, 2), 'utf8');
  } catch {}
}

function createWindow() {
  mainWindow = new BrowserWindow({
    width: 1400,
    height: 900,
    minWidth: 900,
    minHeight: 600,
    frame: false,
    backgroundColor: '#0a0a0b',
    webPreferences: {
      preload: path.join(__dirname, 'preload.js'),
      contextIsolation: true,
      nodeIntegration: false,
      backgroundThrottling: false,
      v8CacheOptions: 'bypassHeatCheck',
    },
    show: true,
  });
  mainWindow.loadFile(path.join(__dirname, 'index.html'));
}

app.whenReady().then(createWindow);
app.on('window-all-closed', () => app.quit());

// ── IPC ───────────────────────────────────────────────────────────────────────

ipcMain.handle('prefs:load', () => loadPrefs());
ipcMain.handle('prefs:save', (_, prefs) => savePrefs(prefs));

ipcMain.handle('dialog:openFolder', async () => {
  const result = await dialog.showOpenDialog(mainWindow, {
    properties: ['openDirectory'],
    title: 'Select root clips folder',
  });
  if (result.canceled) return null;
  return result.filePaths[0];
});

const VIDEO_EXTS = ['.mp4', '.mov', '.avi', '.mkv', '.webm', '.wmv', '.flv', '.m4v', '.mts', '.m2ts'];

ipcMain.handle('folder:scan', async (_, rootPath) => {
  try {
    const results = [];

    // Scan subfolders (categories)
    const entries = fs.readdirSync(rootPath, { withFileTypes: true });
    const subfolders = entries.filter(e => e.isDirectory()).map(e => e.name);

    // Videos directly in root (category = root folder name)
    const rootVideos = entries
      .filter(e => e.isFile() && VIDEO_EXTS.includes(path.extname(e.name).toLowerCase()))
      .map(e => {
        const fullPath = path.join(rootPath, e.name);
        const stat = fs.statSync(fullPath);
        return { name: e.name, fullPath, size: stat.size, mtime: stat.mtimeMs, category: path.basename(rootPath) };
      });
    results.push(...rootVideos);

    // Videos in each subfolder
    for (const sub of subfolders) {
      const subPath = path.join(rootPath, sub);
      try {
        const subEntries = fs.readdirSync(subPath, { withFileTypes: true });
        const videos = subEntries
          .filter(e => e.isFile() && VIDEO_EXTS.includes(path.extname(e.name).toLowerCase()))
          .map(e => {
            const fullPath = path.join(subPath, e.name);
            const stat = fs.statSync(fullPath);
            return { name: e.name, fullPath, size: stat.size, mtime: stat.mtimeMs, category: sub };
          });
        results.push(...videos);
      } catch {}
    }

    results.sort((a, b) => b.mtime - a.mtime);
    return { videos: results, categories: subfolders };
  } catch (e) {
    return { videos: [], categories: [] };
  }
});

ipcMain.handle('video:getDuration', async (_, filePath) => {
  return new Promise((resolve) => {
    const ffmpeg = getFfmpegPath();
    const proc = spawn(ffmpeg, ['-i', filePath], { windowsHide: true });
    let stderr = '';
    proc.stderr.on('data', d => stderr += d.toString());
    proc.on('close', () => {
      const match = stderr.match(/Duration:\s*(\d+):(\d+):(\d+\.?\d*)/);
      if (match) resolve(parseInt(match[1]) * 3600 + parseInt(match[2]) * 60 + parseFloat(match[3]));
      else resolve(null);
    });
    proc.on('error', () => resolve(null));
  });
});

ipcMain.handle('video:trim', async (_, { inputPath, startTime, endTime, saveMode }) => {
  return new Promise(async (resolve) => {
    const ffmpeg = getFfmpegPath();
    const ext = path.extname(inputPath);
    const base = path.basename(inputPath, ext);
    const dir = path.dirname(inputPath);
    let outputPath;

    if (saveMode === 'replace') {
      outputPath = path.join(os.tmpdir(), `clipper_tmp_${Date.now()}${ext}`);
    } else {
      let candidate = path.join(dir, `${base}_trim${ext}`);
      let i = 1;
      while (fs.existsSync(candidate)) { candidate = path.join(dir, `${base}_trim_${i}${ext}`); i++; }
      outputPath = candidate;
    }

    const args = ['-y', '-ss', String(startTime), '-i', inputPath, '-t', String(endTime - startTime), '-c', 'copy', outputPath];
    const proc = spawn(ffmpeg, args, { windowsHide: true });
    let stderr = '';
    proc.stderr.on('data', d => stderr += d.toString());
    proc.on('close', code => {
      if (code !== 0) { resolve({ success: false, error: 'ffmpeg error code ' + code, details: stderr }); return; }
      if (saveMode === 'replace') {
        try { fs.copyFileSync(outputPath, inputPath); fs.unlinkSync(outputPath); resolve({ success: true, outputPath: inputPath }); }
        catch (e) { resolve({ success: false, error: e.message }); }
      } else {
        resolve({ success: true, outputPath });
      }
    });
    proc.on('error', e => resolve({ success: false, error: e.message + '\n\nMake sure ffmpeg.exe is in ffmpeg-bin/.' }));
  });
});

ipcMain.handle('dialog:saveAs', async (_, { defaultName, defaultPath }) => {
  const result = await dialog.showSaveDialog(mainWindow, {
    title: 'Save trimmed clip as...',
    defaultPath: path.join(defaultPath, defaultName),
    filters: [{ name: 'Video Files', extensions: ['mp4', 'mov', 'avi', 'mkv', 'webm'] }, { name: 'All Files', extensions: ['*'] }],
  });
  if (result.canceled) return null;
  return result.filePath;
});

ipcMain.handle('shell:openPath', async (_, filePath) => shell.showItemInFolder(filePath));

ipcMain.handle('file:delete', async (_, filePath) => {
  try { fs.unlinkSync(filePath); return { success: true }; }
  catch (e) { return { success: false, error: e.message }; }
});

// ── Watchers ──────────────────────────────────────────────────────────────────
const watchers = new Map();

ipcMain.handle('folder:watch', (_, rootPath) => {
  watchers.forEach(w => w.close());
  watchers.clear();
  if (!rootPath) return true;

  const watchDir = (dirPath) => {
    try {
      const w = fs.watch(dirPath, { persistent: false }, (event, filename) => {
        if (filename && mainWindow) mainWindow.webContents.send('folder:changed');
      });
      watchers.set(dirPath, w);
    } catch {}
  };

  watchDir(rootPath);
  try {
    fs.readdirSync(rootPath, { withFileTypes: true })
      .filter(e => e.isDirectory())
      .forEach(e => watchDir(path.join(rootPath, e.name)));
  } catch {}
  return true;
});

// Extracts a poster frame AND the stream metadata in a single ffmpeg pass —
// ffmpeg prints the input's stream layout to stderr regardless of what we ask it to do,
// so the quality badge costs us zero extra process spawns.
ipcMain.handle('video:thumbnail', async (_, { filePath, time }) => {
  return new Promise(resolve => {
    const ffmpeg = getFfmpegPath();
    const args = [
      '-ss', String(time ?? 3),
      '-i', filePath,
      '-vframes', '1',
      '-vf', 'scale=320:-1',
      '-f', 'image2',
      '-vcodec', 'mjpeg',
      'pipe:1',
    ];
    const proc = spawn(ffmpeg, args, { windowsHide: true });
    const chunks = [];
    let stderr = '';
    proc.stdout.on('data', chunk => chunks.push(chunk));
    proc.stderr.on('data', d => stderr += d.toString());
    proc.on('close', code => {
      const meta = parseProbe(stderr);
      if (code !== 0 || !chunks.length) { resolve({ dataUrl: null, meta }); return; }
      resolve({ dataUrl: 'data:image/jpeg;base64,' + Buffer.concat(chunks).toString('base64'), meta });
    });
    proc.on('error', () => resolve({ dataUrl: null, meta: null }));
  });
});

ipcMain.handle('video:probe', async (_, filePath) => probeFile(filePath));
ipcMain.handle('encode:encoders', async () => detectEncoders());
ipcMain.handle('encode:cancel', () => {
  if (activeEncode) {
    encodeCancelled = true;
    try { activeEncode.kill('SIGKILL'); } catch {}
  }
  return true;
});
ipcMain.handle('video:encode', async (e, opts) => runEncode(opts, e.sender));

ipcMain.handle('window:minimize', () => mainWindow.minimize());
ipcMain.handle('window:maximize', () => { if (mainWindow.isMaximized()) mainWindow.unmaximize(); else mainWindow.maximize(); });
ipcMain.handle('window:close', () => mainWindow.close());

// ── Probing ───────────────────────────────────────────────────────────────────
// ffmpeg (not ffprobe) is the only binary we ship, so metadata comes from parsing
// the stream banner it writes to stderr for every input it opens.
function parseProbe(stderr) {
  if (!stderr) return null;
  const meta = {
    durationSec: null, totalKbps: null,
    width: null, height: null, videoCodec: null, fps: null, videoKbps: null,
    audioCodec: null, audioKbps: null, audioChannels: null, hasAudio: false,
  };

  const dur = stderr.match(/Duration:\s*(\d+):(\d+):(\d+\.?\d*)/);
  if (dur) meta.durationSec = +dur[1] * 3600 + +dur[2] * 60 + parseFloat(dur[3]);
  const total = stderr.match(/Duration:[^\n]*?bitrate:\s*(\d+)\s*kb\/s/);
  if (total) meta.totalKbps = parseInt(total[1]);

  for (const line of stderr.split(/\r?\n/)) {
    if (!/^\s*Stream #\d+:\d+/.test(line)) continue;

    if (!meta.videoCodec && /:\s*Video:/.test(line)) {
      const codec = line.match(/:\s*Video:\s*([a-zA-Z0-9_]+)/);
      if (codec) meta.videoCodec = codec[1].toLowerCase();
      // Read dimensions from after the codec tag so a fourcc like 0x31637661 can't match.
      const dims = line.slice(line.indexOf('Video:')).match(/\b(\d{2,5})x(\d{2,5})\b/);
      if (dims) { meta.width = parseInt(dims[1]); meta.height = parseInt(dims[2]); }
      const fps = line.match(/([\d.]+)\s*fps/);
      if (fps) meta.fps = parseFloat(fps[1]);
      const br = line.match(/,\s*(\d+)\s*kb\/s/);
      if (br) meta.videoKbps = parseInt(br[1]);
    }

    if (!meta.audioCodec && /:\s*Audio:/.test(line)) {
      const codec = line.match(/:\s*Audio:\s*([a-zA-Z0-9_]+)/);
      if (codec) { meta.audioCodec = codec[1].toLowerCase(); meta.hasAudio = true; }
      const br = line.match(/,\s*(\d+)\s*kb\/s/);
      if (br) meta.audioKbps = parseInt(br[1]);
      if (/\bmono\b/.test(line))        meta.audioChannels = 1;
      else if (/\bstereo\b/.test(line)) meta.audioChannels = 2;
    }
  }

  // No per-stream video bitrate (common in Matroska) — back it out of the container total.
  if (meta.videoKbps == null && meta.totalKbps != null) {
    meta.videoKbps = Math.max(1, meta.totalKbps - (meta.audioKbps ?? (meta.hasAudio ? 160 : 0)));
  }
  return (meta.width || meta.durationSec != null) ? meta : null;
}

function probeFile(filePath) {
  return new Promise(resolve => {
    const proc = spawn(getFfmpegPath(), ['-hide_banner', '-i', filePath], { windowsHide: true });
    let stderr = '';
    proc.stderr.on('data', d => stderr += d.toString());
    proc.on('close', () => resolve(parseProbe(stderr)));
    proc.on('error', () => resolve(null));
  });
}

// ── Encoder detection ─────────────────────────────────────────────────────────
// `-encoders` only proves ffmpeg was built with the encoder, not that this machine
// has the silicon for it. A throwaway one-frame encode is the only honest test.
const HW_CANDIDATES = [
  { id: 'h264_nvenc', family: 'h264', vendor: 'NVIDIA NVENC' },
  { id: 'hevc_nvenc', family: 'hevc', vendor: 'NVIDIA NVENC' },
  { id: 'h264_qsv',   family: 'h264', vendor: 'Intel QuickSync' },
  { id: 'hevc_qsv',   family: 'hevc', vendor: 'Intel QuickSync' },
  { id: 'h264_amf',   family: 'h264', vendor: 'AMD AMF' },
  { id: 'hevc_amf',   family: 'hevc', vendor: 'AMD AMF' },
];

let encoderProbe = null;

function testEncoder(id) {
  return new Promise(resolve => {
    const args = [
      '-hide_banner', '-loglevel', 'error',
      '-f', 'lavfi', '-i', 'color=c=black:s=320x240:d=0.1:r=10',
      '-c:v', id, '-frames:v', '1', '-f', 'null', '-',
    ];
    const proc = spawn(getFfmpegPath(), args, { windowsHide: true });
    const timer = setTimeout(() => { try { proc.kill('SIGKILL'); } catch {} resolve(false); }, 12000);
    proc.on('close', code => { clearTimeout(timer); resolve(code === 0); });
    proc.on('error', () => { clearTimeout(timer); resolve(false); });
  });
}

function detectEncoders() {
  if (encoderProbe) return encoderProbe;
  encoderProbe = (async () => {
    const results = await Promise.all(HW_CANDIDATES.map(c => testEncoder(c.id)));
    return {
      software: [
        { id: 'libx264', family: 'h264', vendor: 'CPU (x264)' },
        { id: 'libx265', family: 'hevc', vendor: 'CPU (x265)' },
      ],
      hardware: HW_CANDIDATES.filter((_, i) => results[i]),
    };
  })();
  return encoderProbe;
}

// ── Encoding (compress / convert) ─────────────────────────────────────────────
let activeEncode = null;
let encodeCancelled = false;

const MP4_SAFE_AUDIO = ['aac', 'mp3', 'ac3', 'eac3', 'alac'];
const SW_PRESETS = ['ultrafast', 'superfast', 'veryfast', 'faster', 'fast', 'medium', 'slow', 'slower'];

// Maps our shared 0..7 speed scale onto whatever each encoder family calls its presets.
function presetArgs(encoder, speed) {
  const i = Math.max(0, Math.min(7, speed ?? 4));
  if (encoder.endsWith('_nvenc')) return ['-preset', 'p' + Math.max(1, Math.min(7, 8 - i))];
  if (encoder.endsWith('_qsv'))   return ['-preset', SW_PRESETS[i] || 'medium'];
  if (encoder.endsWith('_amf'))   return ['-quality', i <= 2 ? 'speed' : i >= 6 ? 'quality' : 'balanced'];
  return ['-preset', SW_PRESETS[i] || 'medium'];
}

function rateControlArgs(encoder, mode, crf, kbps) {
  if (mode === 'bitrate') {
    const b = Math.max(100, Math.round(kbps));
    const common = ['-b:v', b + 'k', '-maxrate', Math.round(b * 1.45) + 'k', '-bufsize', (b * 2) + 'k'];
    if (encoder.endsWith('_nvenc')) return ['-rc', 'vbr', ...common];
    if (encoder.endsWith('_amf'))   return ['-rc', 'vbr_peak', ...common];
    return common;
  }
  const q = Math.max(0, Math.min(51, Math.round(crf)));
  if (encoder.endsWith('_nvenc')) return ['-rc', 'vbr', '-cq', String(q), '-b:v', '0'];
  if (encoder.endsWith('_qsv'))   return ['-global_quality', String(q)];
  if (encoder.endsWith('_amf'))   return ['-rc', 'cqp', '-qp_i', String(q), '-qp_p', String(q), '-qp_b', String(q)];
  return ['-crf', String(q)];
}

function uniquePath(dir, base, ext) {
  let candidate = path.join(dir, base + ext);
  let i = 1;
  while (fs.existsSync(candidate)) { candidate = path.join(dir, base + '_' + i + ext); i++; }
  return candidate;
}

/**
 * opts: { inputPath, saveMode:'new'|'replace', suffix, container:'mp4',
 *         encoder, speed, rateMode:'crf'|'bitrate', crf, bitrateKbps,
 *         targetHeight|null, targetFps|null,
 *         audioMode:'copy'|'encode'|'none', audioKbps }
 */
async function runEncode(opts, sender) {
  if (activeEncode) return { success: false, error: 'Another encode is already running.' };

  const ffmpeg    = getFfmpegPath();
  const inputPath = opts.inputPath;
  const dir       = path.dirname(inputPath);
  const srcExt    = path.extname(inputPath);
  const base      = path.basename(inputPath, srcExt);
  const outExt    = '.' + (opts.container || 'mp4');

  const src = await probeFile(inputPath);
  const totalSec = (src && src.durationSec) || 0;

  // Replace keeps the original's name; only the extension follows the new container.
  const finalPath = opts.saveMode === 'replace'
    ? path.join(dir, base + outExt)
    : uniquePath(dir, base + (opts.suffix || '_converted'), outExt);

  // Always stage through temp: on replace the destination may be the input itself, and
  // on save-as-new a cancelled run must not leave a half-written file in the library.
  const tmpPath = path.join(os.tmpdir(), 'clipper_enc_' + Date.now() + outExt);

  const args = ['-y', '-hide_banner', '-loglevel', 'error', '-i', inputPath];

  const vf = [];
  if (opts.targetHeight) {
    // "720p" names the short side. For portrait footage that is the width, so scale
    // the correct axis instead of squashing a 1080x1920 clip down to 270x480.
    const portrait = src && src.width && src.height && src.width < src.height;
    vf.push(portrait
      ? 'scale=' + opts.targetHeight + ':-2:flags=bicubic'
      : 'scale=-2:' + opts.targetHeight + ':flags=bicubic');
  }
  if (opts.targetFps)    vf.push('fps=' + opts.targetFps);

  args.push('-map', '0:v:0');
  if (opts.audioMode === 'none') args.push('-an');
  else args.push('-map', '0:a:0?');

  if (vf.length) args.push('-vf', vf.join(','));

  args.push('-c:v', opts.encoder);
  args.push(...rateControlArgs(opts.encoder, opts.rateMode, opts.crf, opts.bitrateKbps));
  args.push(...presetArgs(opts.encoder, opts.speed));
  args.push('-pix_fmt', 'yuv420p');

  if (opts.audioMode !== 'none') {
    // Stream-copying audio into MP4 only works for codecs MP4 players actually accept.
    const canCopy = opts.audioMode === 'copy'
      && (opts.container !== 'mp4' || MP4_SAFE_AUDIO.includes((src && src.audioCodec) || ''));
    if (canCopy) args.push('-c:a', 'copy');
    else args.push('-c:a', 'aac', '-b:a', Math.max(32, opts.audioKbps || 160) + 'k');
  }

  if (opts.container === 'mp4') args.push('-movflags', '+faststart');
  args.push('-progress', 'pipe:1', '-nostats', tmpPath);

  return new Promise(resolve => {
    const proc = spawn(ffmpeg, args, { windowsHide: true });
    activeEncode = proc;
    encodeCancelled = false;
    let stderr = '';
    let stdoutBuf = '';

    proc.stderr.on('data', d => {
      stderr += d.toString();
      if (stderr.length > 8000) stderr = stderr.slice(-8000);
    });

    proc.stdout.on('data', d => {
      stdoutBuf += d.toString();
      const lines = stdoutBuf.split(/\r?\n/);
      stdoutBuf = lines.pop() || '';
      let payload = null;
      for (const line of lines) {
        const idx = line.indexOf('=');
        if (idx === -1) continue;
        const k = line.slice(0, idx).trim();
        const v = line.slice(idx + 1).trim();
        if (k === 'out_time_us' || k === 'out_time_ms') {
          // ffmpeg reports both of these keys in microseconds, despite the _ms name.
          const sec = parseInt(v) / 1e6;
          if (!isNaN(sec)) {
            payload = Object.assign(payload || {}, {
              timeSec: sec,
              percent: totalSec ? Math.min(99.9, (sec / totalSec) * 100) : null,
            });
          }
        } else if (k === 'speed') {
          const sp = parseFloat(v);
          if (!isNaN(sp)) payload = Object.assign(payload || {}, { speed: sp });
        } else if (k === 'fps') {
          const f = parseFloat(v);
          if (!isNaN(f)) payload = Object.assign(payload || {}, { fps: f });
        }
      }
      if (payload && sender && !sender.isDestroyed()) {
        sender.send('encode:progress', Object.assign(payload, { totalSec }));
      }
    });

    const cleanupTmp = () => { try { if (fs.existsSync(tmpPath)) fs.unlinkSync(tmpPath); } catch {} };

    proc.on('close', code => {
      activeEncode = null;

      // encode:cancel kills the process, so a non-zero exit there means cancelled, not failed.
      if (encodeCancelled) { cleanupTmp(); resolve({ success: false, cancelled: true }); return; }
      if (code !== 0 || !fs.existsSync(tmpPath)) {
        cleanupTmp();
        resolve({ success: false, error: 'ffmpeg exited with code ' + code, details: stderr.trim() });
        return;
      }

      try {
        // A container change on replace means the original has a different extension —
        // remove it so we don't silently leave both copies behind.
        if (opts.saveMode === 'replace' && path.resolve(finalPath) !== path.resolve(inputPath)) {
          fs.unlinkSync(inputPath);
        }
        fs.copyFileSync(tmpPath, finalPath);
        fs.unlinkSync(tmpPath);
        resolve({ success: true, outputPath: finalPath, size: fs.statSync(finalPath).size });
      } catch (e) {
        cleanupTmp();
        resolve({ success: false, error: e.message });
      }
    });

    proc.on('error', e => {
      activeEncode = null;
      cleanupTmp();
      resolve({ success: false, error: e.message + '\n\nMake sure ffmpeg.exe is in ffmpeg-bin/.' });
    });
  });
}

// Warm the encoder probe early so the modal never has to wait on it.
app.whenReady().then(() => { detectEncoders(); });
