const { app, BrowserWindow, ipcMain, dialog, shell, Tray, Menu, nativeImage, Notification } = require('electron');
const path = require('path');
const fs = require('fs');
const { spawn } = require('child_process');
const net = require('net');
const os = require('os');

let mainWindow;
let quitting = false;

// Windows keys toasts off the app user model id, and a notification from an id it does not
// recognise is dropped without a word. It has to match the `appId` electron-builder writes into
// the Start Menu shortcut, or a packaged build gets no notifications at all.
app.setAppUserModelId('com.clipper.app');

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

  mainWindow.on('close', (e) => {
    if (quitting || !loadCaptureConfig().enabled) return;
    e.preventDefault();
    mainWindow.hide();
  });
}

// Two copies would mean two capture daemons fighting over one segment directory, so the second
// launch hands focus back to the first and exits.
if (!app.requestSingleInstanceLock()) {
  app.quit();
} else {
  app.on('second-instance', () => {
    if (mainWindow) {
      if (mainWindow.isMinimized()) mainWindow.restore();
      mainWindow.show();
      mainWindow.focus();
    }
  });
}

app.whenReady().then(() => {
  createWindow();
  setupTray();
  if (loadCaptureConfig().enabled) startCaptureDaemon();
});

// With capture on, closing the window leaves the recorder running — that is the whole point of a
// replay buffer. The tray icon is then the only way back, which is why it is always created.
app.on('window-all-closed', () => {
  if (!loadCaptureConfig().enabled) app.quit();
});

app.on('before-quit', () => { quitting = true; stopCaptureDaemon(); });

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
ipcMain.handle('encode:filters', async () => detectFilters());
ipcMain.handle('encode:cancel', () => {
  if (activeEncode) {
    encodeCancelled = true;
    try { activeEncode.kill('SIGKILL'); } catch {}
  }
  return true;
});
ipcMain.handle('video:encode', async (e, opts) => runEncode(opts, e.sender));
ipcMain.handle('video:passthrough', async (_, opts) => runPassthrough(opts));

ipcMain.handle('window:minimize', () => mainWindow.minimize());
ipcMain.handle('window:maximize', () => { if (mainWindow.isMaximized()) mainWindow.unmaximize(); else mainWindow.maximize(); });
ipcMain.handle('window:close', () => mainWindow.close());

// ── Capture daemon ────────────────────────────────────────────────────────────
// clipper-capture keeps the last N seconds of a monitor encoded in a ring buffer and writes an
// MP4 when its hotkey fires. It runs as a separate process so it survives this window closing,
// and it is configured through a file of its own rather than prefs.json — savePrefs() rewrites
// that whole file from the renderer's settings object, which would race a daemon reading it.

const CAPTURE_PIPE = '\\\\.\\pipe\\clipper-capture';
const captureConfigPath = path.join(app.getPath('userData'), 'capture.json');

const CAPTURE_DEFAULTS = {
  version: 1,
  gameDetection: 'auto',
  notifyOnSave: true,
  enabled: false,          // off until asked for: it costs GPU time and disk continuously
  recordMode: 'game',
  bufferSeconds: 60,
  monitor: { device: '', friendly: '' },
  quality: 'high',
  outputPath: '',
  perGameSubfolder: true,
  hotkey: 'Ctrl+Alt+F12',
  audio: { desktop: true, mic: true, micDevice: '', micGainDb: 0 },
  toneMap: 'auto',
  segmentDir: null,
  includeProcesses: [],
  excludeProcesses: [],
};

// The daemon process is started and stopped for exactly one reason: the feature being switched on
// or off. Every other change goes down the pipe as `reload`, and the daemon decides for itself
// whether that means rebuilding its pipeline.
//
// It used to restart the process for quality, tone mapping and the segment directory, and that is
// what made settings look like they were not applying. `stopCaptureDaemon()` returned as soon as
// it had asked the old daemon to quit, but the old daemon holds the single-instance mutex and the
// named pipe until it has finished flushing its ring — so the replacement spawned 300 ms later
// found the mutex taken and exited on the spot, leaving nothing recording and a settings panel
// quoting a process that no longer existed.
const capture = {
  proc: null,
  sock: null,
  retry: null,
  status: null,
  tray: null,
  awaitingStatus: null,
  // Bumped on every stop, so a dying socket's handlers cannot touch the daemon that replaced it.
  generation: 0,
  // The previous daemon, while it is still on its way out.
  dying: null,
  starting: false,
  // A reload asked for before the pipe was up. Dropping it is what a lost setting looks like.
  pendingReload: false,
};

function getCapturePath() {
  if (app.isPackaged) {
    const packaged = path.join(process.resourcesPath, 'capture-bin', 'clipper-capture.exe');
    if (fs.existsSync(packaged)) return packaged;
  }
  const dev = path.join(__dirname, '..', 'capture', 'target', 'release', 'clipper-capture.exe');
  if (fs.existsSync(dev)) return dev;
  return path.join(__dirname, '..', 'capture-bin', 'clipper-capture.exe');
}

function loadCaptureConfig() {
  try {
    const saved = JSON.parse(fs.readFileSync(captureConfigPath, 'utf8'));
    // `audio` is merged a level deeper than the rest. A spread would replace the whole object, so a
    // config written before the microphone existed — `{ desktop: true }` — would leave `mic`
    // undefined here while the daemon, which fills missing fields from its own defaults, has it on.
    // The panel would then show a switch that disagreed with what was being recorded.
    return {
      ...CAPTURE_DEFAULTS,
      ...saved,
      audio: { ...CAPTURE_DEFAULTS.audio, ...(saved.audio || {}) },
    };
  } catch { return { ...CAPTURE_DEFAULTS, audio: { ...CAPTURE_DEFAULTS.audio } }; }
}

// Written temp-then-rename so the daemon can never read a half-written file.
function writeCaptureConfig(config) {
  const tmp = captureConfigPath + '.tmp';
  fs.writeFileSync(tmp, JSON.stringify(config, null, 2), 'utf8');
  fs.renameSync(tmp, captureConfigPath);
}

function sendCapture(cmd) {
  if (!capture.sock) return false;
  try { capture.sock.write(JSON.stringify({ cmd }) + '\n'); return true; }
  catch { return false; }
}

function startCaptureDaemon() {
  if (capture.proc || capture.starting) return;

  // One daemon at a time, enforced by a named mutex on the other side. Wait for the last one to
  // let go rather than racing it and losing.
  if (capture.dying) {
    capture.starting = true;
    const previous = capture.dying;
    const go = () => {
      if (capture.dying !== previous && capture.dying !== null) return;
      capture.dying = null;
      capture.starting = false;
      if (loadCaptureConfig().enabled) startCaptureDaemon();
    };
    previous.once('exit', go);
    setTimeout(go, 3000);
    return;
  }

  const exe = getCapturePath();
  if (!fs.existsSync(exe)) {
    sendCaptureEvent({ event: 'error', message: 'clipper-capture.exe not found — build it with cargo build --release in capture/' });
    return;
  }
  if (!fs.existsSync(captureConfigPath)) writeCaptureConfig(loadCaptureConfig());

  // --parent-pid is how the daemon knows to exit if this process is killed outright. An orphaned
  // recorder writing to disk forever is the worst failure available here.
  const generation = ++capture.generation;
  const proc = spawn(exe, [
    'daemon', '--config', captureConfigPath, '--parent-pid', String(process.pid),
  ], { windowsHide: true, stdio: 'ignore', detached: false });
  capture.proc = proc;

  proc.on('exit', () => {
    if (capture.dying === proc) capture.dying = null;
    if (capture.generation !== generation) return;
    capture.proc = null;
    capture.status = null;
    updateTray();
    sendCaptureEvent({ event: 'state', recording: false });
  });
  proc.on('error', (e) => sendCaptureEvent({ event: 'error', message: e.message }));

  connectCapture(generation);
}

function stopCaptureDaemon() {
  const proc = capture.proc;
  if (!proc) return;

  capture.generation++;
  if (capture.retry) { clearTimeout(capture.retry); capture.retry = null; }
  const sock = capture.sock;
  capture.proc = null;
  capture.sock = null;
  capture.status = null;
  capture.pendingReload = false;
  capture.dying = proc;

  let asked = false;
  if (sock) {
    try { sock.write(JSON.stringify({ cmd: 'quit' }) + '\n'); asked = true; } catch {}
  }
  if (!asked) { try { proc.kill(); } catch {} }

  // The daemon flushes its ring on the way out; give it a moment before insisting.
  setTimeout(() => {
    try { if (sock) sock.destroy(); } catch {}
    try { proc.kill(); } catch {}
  }, 2000);

  sendCaptureEvent({ event: 'state', recording: false });
  updateTray();
}

function connectCapture(generation) {
  if (capture.generation !== generation || capture.sock || !capture.proc) return;
  const sock = net.connect({ path: CAPTURE_PIPE });
  let buffer = '';

  sock.on('connect', () => {
    if (capture.generation !== generation) { sock.destroy(); return; }
    capture.sock = sock;
    // A setting changed while the pipe was still coming up is still a setting the user changed.
    if (capture.pendingReload) { capture.pendingReload = false; sendCapture('reload'); }
    sendCapture('status');
  });
  sock.on('data', (chunk) => {
    buffer += chunk.toString();
    let nl;
    while ((nl = buffer.indexOf('\n')) >= 0) {
      const line = buffer.slice(0, nl).trim();
      buffer = buffer.slice(nl + 1);
      if (!line) continue;
      try { handleCaptureEvent(JSON.parse(line)); } catch {}
    }
  });

  const retry = () => {
    if (capture.sock === sock) capture.sock = null;
    sock.destroy();
    // The daemon may still be starting up; keep trying while the process is alive.
    if (capture.generation === generation && capture.proc && !capture.retry) {
      capture.retry = setTimeout(() => { capture.retry = null; connectCapture(generation); }, 500);
    }
  };
  sock.on('error', retry);
  sock.on('close', retry);
}

function handleCaptureEvent(event) {
  if (event.event === 'status') {
    capture.status = event;
    if (capture.awaitingStatus) { capture.awaitingStatus(event); capture.awaitingStatus = null; }
  }
  if (event.event === 'state') capture.status = { ...(capture.status || {}), recording: event.recording };
  if (event.event === 'clip-saved') notifyClipSaved(event);
  updateTray();
  sendCaptureEvent(event);
}

// A toast in the corner of the screen, because the hotkey is pressed while you are looking at a
// game and the app's own toast is behind it. Silent on purpose: this fires mid-play, and a
// notification chime over your own game audio is worse than no notification at all.
function notifyClipSaved(event) {
  if (!Notification.isSupported()) return;
  if (loadCaptureConfig().notifyOnSave === false) return;

  const seconds = Math.round((event.durationMs || 0) / 1000);
  const where = event.game && event.game !== 'Desktop' ? ' \u00b7 ' + event.game : '';
  const toast = new Notification({
    title: 'Replay saved',
    body: 'The last ' + seconds + 's' + where,
    icon: trayIcon('tray.png'),
    silent: true,
  });
  toast.on('click', showWindow);
  toast.show();
}

function sendCaptureEvent(event) {
  if (mainWindow && !mainWindow.isDestroyed()) mainWindow.webContents.send('capture:event', event);
}

// ── Tray ──────────────────────────────────────────────────────────────────────

function trayIcon(name) {
  return nativeImage.createFromPath(path.join(__dirname, '..', 'assets', name));
}

function setupTray() {
  if (capture.tray) return;
  capture.tray = new Tray(trayIcon('tray-idle.png'));
  capture.tray.on('click', showWindow);
  updateTray();
}

function showWindow() {
  if (!mainWindow || mainWindow.isDestroyed()) return;
  mainWindow.show();
  mainWindow.focus();
}

function updateTray() {
  if (!capture.tray) return;
  const config = loadCaptureConfig();
  const recording = !!(capture.status && capture.status.recording);

  capture.tray.setImage(trayIcon(recording ? 'tray.png' : 'tray-idle.png'));
  capture.tray.setToolTip(
    !config.enabled ? 'Clipper — instant replay off'
      : recording ? `Clipper — buffering the last ${Math.round(config.bufferSeconds)}s`
      : 'Clipper — waiting for a game');

  capture.tray.setContextMenu(Menu.buildFromTemplate([
    { label: 'Open Clipper', click: showWindow },
    { type: 'separator' },
    {
      label: 'Instant replay',
      type: 'checkbox',
      checked: config.enabled,
      click: () => applyCaptureConfig({ enabled: !config.enabled }),
    },
    {
      label: `Save clip  (${config.hotkey})`,
      enabled: recording,
      click: () => sendCapture('save'),
    },
    { type: 'separator' },
    { label: 'Quit Clipper', click: () => { quitting = true; app.quit(); } },
  ]));
}

// Writes a patch to the config file and gets the daemon into the right state for it.
function applyCaptureConfig(patch) {
  const before = loadCaptureConfig();
  // Same reason as the merge in loadCaptureConfig: a caller sending one audio key must not blank
  // the other two.
  const after = { ...before, ...patch };
  if (patch.audio) after.audio = { ...before.audio, ...patch.audio };
  writeCaptureConfig(after);

  if (!after.enabled) {
    stopCaptureDaemon();
  } else if (!capture.proc) {
    startCaptureDaemon();
  } else if (!sendCapture('reload')) {
    // The pipe is not up yet. Remember, and send it the moment it is.
    capture.pendingReload = true;
  }

  updateTray();
  // The settings panel is not the only thing that writes this file — the tray menu toggles capture
  // on and off too, and a panel still showing the old config would hand it straight back on the
  // next change. Tell the window what the config is now, whoever changed it.
  sendCaptureEvent({ event: 'config', config: after });
  return after;
}

ipcMain.handle('capture:config', () => loadCaptureConfig());
ipcMain.handle('capture:setConfig', (_, patch) => applyCaptureConfig(patch));
// Waits for a fresh reply rather than handing back the cache. A daemon that has stopped
// answering is exactly the failure worth seeing, and a stale cache hides it behind numbers that
// look fine.
ipcMain.handle('capture:status', () => new Promise((resolve) => {
  if (!sendCapture('status')) return resolve(null);
  const timer = setTimeout(() => { capture.awaitingStatus = null; resolve(null); }, 1500);
  capture.awaitingStatus = (status) => { clearTimeout(timer); resolve(status); };
}));
ipcMain.handle('capture:save', () => sendCapture('save'));
ipcMain.handle('capture:available', () => fs.existsSync(getCapturePath()));

// The daemon is the authority on what monitors exist and whether each is in HDR right now, so ask
// it rather than duplicating the detection in Electron.
function askCapture(command) {
  return new Promise((resolve) => {
    const exe = getCapturePath();
    if (!fs.existsSync(exe)) return resolve([]);
    const proc = spawn(exe, [command], { windowsHide: true });
    let out = '';
    proc.stdout.on('data', (d) => out += d.toString());
    proc.on('error', () => resolve([]));
    proc.on('close', () => { try { resolve(JSON.parse(out)); } catch { resolve([]); } });
  });
}

ipcMain.handle('capture:monitors', () => askCapture('monitors'));

// Microphones, asked for fresh every time the panel opens rather than cached: a headset that was
// plugged in a minute ago is exactly the device somebody is coming here to pick.
ipcMain.handle('capture:inputs', () => askCapture('inputs'));

// ── Probing ───────────────────────────────────────────────────────────────────
// ffmpeg (not ffprobe) is the only binary we ship, so metadata comes from parsing
// the stream banner it writes to stderr for every input it opens.

// The pixel format and its colour tags sit immediately before the frame size on the
// Stream line: "..., yuv420p10le(tv, bt2020nc/bt2020/smpte2084), 3840x2160". Anchoring
// on the dimensions is what stops a codec profile or fourcc from matching first.
const COLOR_RANGES = new Set(['tv', 'pc', 'limited', 'full', 'unknown']);

function parseColorInfo(videoPart, meta) {
  const m = videoPart.match(/,\s*([a-z0-9]+)\s*(?:\(([^)]*)\))?\s*,\s*\d{2,5}x\d{2,5}/);
  if (!m) return;
  meta.pixFmt = m[1];

  if (/10(le|be)$/.test(m[1]) || m[1] === 'p010') meta.bitDepth = 10;
  else if (/12(le|be)$/.test(m[1])) meta.bitDepth = 12;
  else if (/16(le|be)$/.test(m[1])) meta.bitDepth = 16;
  else meta.bitDepth = 8;

  for (const raw of (m[2] || '').split(',')) {
    const part = raw.trim();
    if (!part || part === 'progressive') continue;
    if (COLOR_RANGES.has(part)) { meta.colorRange = part; continue; }
    if (part.includes('/')) {
      // matrix/primaries/transfer, with "unknown" standing in for anything untagged.
      const [mx, pr, tr] = part.split('/').map(x => x.trim());
      if (mx && mx !== 'unknown') meta.colorMatrix = mx;
      if (pr && pr !== 'unknown') meta.colorPrimaries = pr;
      if (tr && tr !== 'unknown') meta.colorTransfer = tr;
    } else if (/^[a-z0-9+._-]+$/.test(part)) {
      // ffmpeg collapses the triplet to a single token when all three agree.
      meta.colorMatrix = meta.colorPrimaries = meta.colorTransfer = part;
    }
  }

  // The transfer curve is what actually makes a clip look washed out on an SDR screen —
  // bt2020 primaries on their own are a wide gamut, not HDR.
  if (meta.colorTransfer === 'smpte2084')         meta.hdrFormat = 'pq';
  else if (meta.colorTransfer === 'arib-std-b67') meta.hdrFormat = 'hlg';
  else if (meta.hasDolbyVision)                   meta.hdrFormat = 'pq';
  meta.isHdr = meta.hdrFormat !== null;
}

function parseProbe(stderr) {
  if (!stderr) return null;
  const meta = {
    durationSec: null, totalKbps: null,
    width: null, height: null, videoCodec: null, fps: null, videoKbps: null,
    audioCodec: null, audioKbps: null, audioChannels: null, hasAudio: false,
    pixFmt: null, bitDepth: null, colorRange: null,
    colorMatrix: null, colorPrimaries: null, colorTransfer: null,
    isHdr: false, hdrFormat: null, hasDolbyVision: false,
  };

  const dur = stderr.match(/Duration:\s*(\d+):(\d+):(\d+\.?\d*)/);
  if (dur) meta.durationSec = +dur[1] * 3600 + +dur[2] * 60 + parseFloat(dur[3]);
  const total = stderr.match(/Duration:[^\n]*?bitrate:\s*(\d+)\s*kb\/s/);
  if (total) meta.totalKbps = parseInt(total[1]);

  // A Dolby Vision layer is printed as stream side data, not on the Stream line itself.
  meta.hasDolbyVision = /DOVI configuration record/i.test(stderr);

  for (const line of stderr.split(/\r?\n/)) {
    if (!/^\s*Stream #\d+:\d+/.test(line)) continue;

    if (!meta.videoCodec && /:\s*Video:/.test(line)) {
      const codec = line.match(/:\s*Video:\s*([a-zA-Z0-9_]+)/);
      if (codec) meta.videoCodec = codec[1].toLowerCase();
      // Read dimensions from after the codec tag so a fourcc like 0x31637661 can't match.
      const videoPart = line.slice(line.indexOf('Video:'));
      const dims = videoPart.match(/\b(\d{2,5})x(\d{2,5})\b/);
      if (dims) { meta.width = parseInt(dims[1]); meta.height = parseInt(dims[2]); }
      parseColorInfo(videoPart, meta);
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
  { id: 'av1_nvenc',  family: 'av1',  vendor: 'NVIDIA NVENC' },
  { id: 'h264_qsv',   family: 'h264', vendor: 'Intel QuickSync' },
  { id: 'hevc_qsv',   family: 'hevc', vendor: 'Intel QuickSync' },
  { id: 'av1_qsv',    family: 'av1',  vendor: 'Intel QuickSync' },
  { id: 'h264_amf',   family: 'h264', vendor: 'AMD AMF' },
  { id: 'hevc_amf',   family: 'hevc', vendor: 'AMD AMF' },
  { id: 'av1_amf',    family: 'av1',  vendor: 'AMD AMF' },
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

// Tone mapping leans on zscale (libzimg), which plenty of ffmpeg builds ship without.
// Ask the binary we are actually going to run rather than assuming the bundled one.
let filterProbe = null;

function detectFilters() {
  if (filterProbe) return filterProbe;
  filterProbe = new Promise(resolve => {
    const proc = spawn(getFfmpegPath(), ['-hide_banner', '-filters'], { windowsHide: true });
    let out = '';
    proc.stdout.on('data', d => out += d.toString());
    const done = () => resolve({
      zscale:  /\bzscale\b/.test(out),
      tonemap: /^\s*\S+\s+tonemap\s/m.test(out),
    });
    proc.on('close', done);
    proc.on('error', () => resolve({ zscale: false, tonemap: false }));
  });
  return filterProbe;
}

function detectEncoders() {
  if (encoderProbe) return encoderProbe;
  encoderProbe = (async () => {
    const results = await Promise.all(HW_CANDIDATES.map(c => testEncoder(c.id)));
    return {
      software: [
        { id: 'libx264',   family: 'h264', vendor: 'CPU (x264)' },
        { id: 'libx265',   family: 'hevc', vendor: 'CPU (x265)' },
        { id: 'libsvtav1', family: 'av1',  vendor: 'CPU (SVT-AV1)' },
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
  if (encoder === 'libsvtav1')    return ['-preset', String(12 - i)];   // 13 = fastest, 0 = slowest
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

// HDR footage is graded against a PQ/HLG curve and the bt2020 gamut. Handing those
// values to an SDR player unchanged is exactly what makes a clip look flat and grey,
// so the picture has to be taken back to linear light, compressed into SDR's range,
// and re-encoded against bt709 — a conversion, not a tag change.
const TONEMAP_OPERATORS = {
  balanced: 'mobius:param=0.3:desat=0',
  filmic:   'hable:desat=0',
  punchy:   'clip:desat=0',
};

function tonemapFilters(op, hdrFormat) {
  const operator = TONEMAP_OPERATORS[op] || TONEMAP_OPERATORS.balanced;
  // Force the input curve from what we probed: untagged-but-HDR files exist, and
  // zscale would otherwise guess bt709 and leave the picture untouched.
  const tin = hdrFormat === 'hlg' ? 'arib-std-b67' : 'smpte2084';
  return [
    `zscale=tin=${tin}:pin=bt2020:min=bt2020nc:t=linear:npl=100`,
    'format=gbrpf32le',          // tonemap works in linear float RGB
    'zscale=p=bt709',            // gamut first, so the operator only handles brightness
    `tonemap=tonemap=${operator}`,
    'zscale=t=bt709:m=bt709:r=tv',
    'format=yuv420p',
  ];
}

function uniquePath(dir, base, ext) {
  let candidate = path.join(dir, base + ext);
  let i = 1;
  while (fs.existsSync(candidate)) { candidate = path.join(dir, base + '_' + i + ext); i++; }
  return candidate;
}

/**
 * Re-wraps or duplicates a clip without touching the picture. Export reaches for this
 * when a clip already looks the way the preset wants it to: re-encoding would cost
 * quality and time to arrive at the same frames.
 *
 * opts: { inputPath, saveMode:'new'|'replace', suffix, container:'mp4', remux:boolean }
 */
async function runPassthrough(opts) {
  const inputPath = opts.inputPath;
  const dir       = path.dirname(inputPath);
  const srcExt    = path.extname(inputPath);
  const base      = path.basename(inputPath, srcExt);
  const outExt    = opts.remux ? '.' + (opts.container || 'mp4') : srcExt;

  const finalPath = opts.saveMode === 'replace'
    ? path.join(dir, base + outExt)
    : uniquePath(dir, base + (opts.suffix || '_export'), outExt);

  // Nothing to do at all — the file already is what was asked for.
  if (path.resolve(finalPath) === path.resolve(inputPath)) {
    try { return { success: true, outputPath: inputPath, size: fs.statSync(inputPath).size, untouched: true }; }
    catch (e) { return { success: false, error: e.message }; }
  }

  if (!opts.remux) {
    try {
      fs.copyFileSync(inputPath, finalPath);
      return { success: true, outputPath: finalPath, size: fs.statSync(finalPath).size, copied: true };
    } catch (e) { return { success: false, error: e.message }; }
  }

  const src = await probeFile(inputPath);
  const tmpPath = path.join(os.tmpdir(), 'clipper_mux_' + Date.now() + outExt);
  const args = ['-y', '-hide_banner', '-loglevel', 'error', '-i', inputPath,
                '-map', '0:v:0', '-map', '0:a:0?', '-c:v', 'copy'];

  // The video survives the container change untouched; the audio only does when the
  // new container actually accepts that codec.
  if (opts.container === 'mp4' && !MP4_SAFE_AUDIO.includes((src && src.audioCodec) || '')) {
    args.push('-c:a', 'aac', '-b:a', '160k');
  } else {
    args.push('-c:a', 'copy');
  }
  if (opts.container === 'mp4') args.push('-movflags', '+faststart');
  args.push(tmpPath);

  return new Promise(resolve => {
    const proc = spawn(getFfmpegPath(), args, { windowsHide: true });
    let stderr = '';
    proc.stderr.on('data', d => stderr += d.toString());
    proc.on('close', code => {
      const cleanup = () => { try { if (fs.existsSync(tmpPath)) fs.unlinkSync(tmpPath); } catch {} };
      if (code !== 0 || !fs.existsSync(tmpPath)) {
        cleanup();
        resolve({ success: false, error: 'ffmpeg exited with code ' + code, details: stderr.trim() });
        return;
      }
      try {
        if (opts.saveMode === 'replace' && path.resolve(finalPath) !== path.resolve(inputPath)) {
          fs.unlinkSync(inputPath);
        }
        fs.copyFileSync(tmpPath, finalPath);
        fs.unlinkSync(tmpPath);
        resolve({ success: true, outputPath: finalPath, size: fs.statSync(finalPath).size, remuxed: true });
      } catch (e) { cleanup(); resolve({ success: false, error: e.message }); }
    });
    proc.on('error', e => resolve({ success: false, error: e.message }));
  });
}

/**
 * opts: { inputPath, saveMode:'new'|'replace', suffix, container:'mp4',
 *         encoder, speed, rateMode:'crf'|'bitrate', crf, bitrateKbps,
 *         targetHeight|null, targetFps|null,
 *         toneMap: null|'balanced'|'filmic'|'punchy',
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

  // Scale first: tone mapping is the expensive filter, so give it fewer pixels to chew on.
  const toneMapping = !!opts.toneMap && (await detectFilters()).zscale;
  if (toneMapping) vf.push(...tonemapFilters(opts.toneMap, src && src.hdrFormat));

  args.push('-map', '0:v:0');
  if (opts.audioMode === 'none') args.push('-an');
  else args.push('-map', '0:a:0?');

  if (vf.length) args.push('-vf', vf.join(','));

  args.push('-c:v', opts.encoder);
  args.push(...rateControlArgs(opts.encoder, opts.rateMode, opts.crf, opts.bitrateKbps));
  args.push(...presetArgs(opts.encoder, opts.speed));
  args.push('-pix_fmt', 'yuv420p');

  // ffmpeg copies the source's colour tags onto the output by default, which would
  // leave a tone-mapped file still claiming to be HDR. Re-tag it as plain bt709.
  if (toneMapping) {
    args.push('-color_primaries', 'bt709', '-color_trc', 'bt709', '-colorspace', 'bt709');
  }

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
app.whenReady().then(() => { detectEncoders(); detectFilters(); });
