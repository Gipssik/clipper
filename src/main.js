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
    proc.stdout.on('data', chunk => chunks.push(chunk));
    proc.on('close', code => {
      if (code !== 0 || !chunks.length) { resolve(null); return; }
      resolve('data:image/jpeg;base64,' + Buffer.concat(chunks).toString('base64'));
    });
    proc.on('error', () => resolve(null));
  });
});

ipcMain.handle('window:minimize', () => mainWindow.minimize());
ipcMain.handle('window:maximize', () => { if (mainWindow.isMaximized()) mainWindow.unmaximize(); else mainWindow.maximize(); });
ipcMain.handle('window:close', () => mainWindow.close());