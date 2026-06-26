const { contextBridge, ipcRenderer } = require('electron');

contextBridge.exposeInMainWorld('api', {
  loadPrefs:      ()          => ipcRenderer.invoke('prefs:load'),
  savePrefs:      (p)         => ipcRenderer.invoke('prefs:save', p),
  openFolder:     ()          => ipcRenderer.invoke('dialog:openFolder'),
  scanFolder:     (root)      => ipcRenderer.invoke('folder:scan', root),
  getDuration:    (fp)        => ipcRenderer.invoke('video:getDuration', fp),
  getThumbnail:   (fp, time)  => ipcRenderer.invoke('video:thumbnail', { filePath: fp, time }),
  trimVideo:      (opts)      => ipcRenderer.invoke('video:trim', opts),
  saveAsDialog:   (opts)      => ipcRenderer.invoke('dialog:saveAs', opts),
  openInExplorer: (fp)        => ipcRenderer.invoke('shell:openPath', fp),
  deleteFile:     (fp)        => ipcRenderer.invoke('file:delete', fp),
  watchFolder:    (root)      => ipcRenderer.invoke('folder:watch', root),
  onFolderChange: (cb)        => ipcRenderer.on('folder:changed', () => cb()),
  minimize:       ()          => ipcRenderer.invoke('window:minimize'),
  maximize:       ()          => ipcRenderer.invoke('window:maximize'),
  close:          ()          => ipcRenderer.invoke('window:close'),
});