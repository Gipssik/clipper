'use strict';

// ── State ─────────────────────────────────────────────────────────────────────
let rootFolder    = null;
let allVideos     = [];       // full list from scan
let categories    = [];       // subfolder names
let activeCategory = 'all';
let sortOrder     = 'newest'; // 'newest' | 'oldest'
let currentClip   = null;
let videoDuration = 0;
let trimStart     = 0;
let trimEnd       = 0;
let isDragging    = null;
let reloadTimer   = null;
let watchedFolder = null;
let openDropdown  = null;
const cardMap     = new Map(); // fullPath → card element

// ── Thumbnail cache & queue ───────────────────────────────────────────────────
const thumbCache          = new Map(); // fullPath → data URL
const thumbQueue          = [];        // { fullPath, imgEl, cardEl, observer }
let   activeExtractions   = 0;
const MAX_THUMB_CONCURRENT = 4;

// Single shared <video> reused for hover previews — avoids N live decode pipelines
const hoverVid = (() => {
  const v = document.createElement('video');
  v.muted       = true;
  v.playsInline = true;
  v.style.cssText = 'position:absolute;inset:0;width:100%;height:100%;object-fit:cover;pointer-events:none;display:none;z-index:5;';
  return v;
})();

// ── DOM ───────────────────────────────────────────────────────────────────────
const rootFolderDisplay = document.getElementById('root-folder-display');
const changeFolderBtn   = document.getElementById('change-folder-btn');
const emptyOpenBtn      = document.getElementById('empty-open-btn');
const filtersBar        = document.getElementById('filters-bar');
const catTrigger     = document.getElementById('cat-trigger');
const catTriggerLabel= document.getElementById('cat-trigger-label');
const catDropdownMenu= document.getElementById('cat-dropdown-menu');
const catSearch      = document.getElementById('cat-search');
const catDropdownList= document.getElementById('cat-dropdown-list');
const videoGrid         = document.getElementById('video-grid');
const gridEmpty         = document.getElementById('grid-empty');
const modalOverlay      = document.getElementById('modal-overlay');
const modalFilename     = document.getElementById('modal-filename');
const modalCategory     = document.getElementById('modal-category');
const modalDuration     = document.getElementById('modal-duration');
const modalExplorerBtn  = document.getElementById('modal-explorer-btn');
const modalCloseBtn     = document.getElementById('modal-close-btn');
const prevClipBtn       = document.getElementById('prev-clip-btn');
const nextClipBtn       = document.getElementById('next-clip-btn');
const modalFullscreenBtn = document.getElementById('modal-fullscreen-btn');
const fsExpandIcon      = document.getElementById('fs-expand-icon');
const fsCompressIcon    = document.getElementById('fs-compress-icon');
const trimModal         = document.getElementById('trim-modal');
const previewVideo      = document.getElementById('preview-video');
const timeline          = document.getElementById('timeline');
const selOverlay        = document.getElementById('selection-overlay');
const playhead          = document.getElementById('playhead');
const handleStart       = document.getElementById('handle-start');
const handleEnd         = document.getElementById('handle-end');
const tdStart           = document.getElementById('td-start');
const tdEnd             = document.getElementById('td-end');
const tdSel             = document.getElementById('td-sel');
const playBtn           = document.getElementById('play-btn');
const playSelBtn        = document.getElementById('play-sel-btn');
const muteBtn           = document.getElementById('mute-btn');
const volIcon           = document.getElementById('vol-icon');
const volumeSlider      = document.getElementById('volume-slider');
const saveNewBtn        = document.getElementById('save-new-btn');
const replaceBtn        = document.getElementById('replace-btn');
const progressOverlay   = document.getElementById('progress-overlay');
const progSub           = document.getElementById('prog-sub');
const sortBtn           = document.getElementById('sort-btn');
const sortLabel         = document.getElementById('sort-label');
const sortArrow         = document.getElementById('sort-arrow');
const toast             = document.getElementById('toast');

// ── Helpers ───────────────────────────────────────────────────────────────────
function fmt(secs, ms = false) {
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = Math.floor(secs % 60);
  const msv = Math.round((secs % 1) * 1000);
  const base = h > 0 ? `${h}:${String(m).padStart(2,'0')}:${String(s).padStart(2,'0')}` : `${m}:${String(s).padStart(2,'0')}`;
  return ms ? `${base}.${String(msv).padStart(3,'0')}` : base;
}

function fmtBytes(bytes) {
  if (bytes > 1e9) return (bytes/1e9).toFixed(1) + ' GB';
  if (bytes > 1e6) return (bytes/1e6).toFixed(1) + ' MB';
  return (bytes/1e3).toFixed(0) + ' KB';
}

function fmtDate(ms) {
  const d = new Date(ms);
  const pad = n => String(n).padStart(2,'0');
  return `${d.getFullYear()}-${pad(d.getMonth()+1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

function showToast(msg, type = 'info', duration = 2800) {
  toast.textContent = msg;
  toast.className = 'show ' + type;
  clearTimeout(toast._t);
  toast._t = setTimeout(() => toast.className = '', duration);
}

function timelineWidth() { return timeline.getBoundingClientRect().width; }
function posToTime(px) { return Math.max(0, Math.min(videoDuration, (px / timelineWidth()) * videoDuration)); }

function updateTimelineUI() {
  if (!videoDuration) return;
  const s = (trimStart / videoDuration) * 100;
  const e = (trimEnd   / videoDuration) * 100;
  selOverlay.style.left  = s + '%';
  selOverlay.style.width = (e - s) + '%';
  handleStart.style.left = s + '%';
  handleEnd.style.left   = e + '%';
  tdStart.textContent = fmt(trimStart, true);
  tdEnd.textContent   = fmt(trimEnd,   true);
  tdSel.textContent   = '▶ ' + fmt(trimEnd - trimStart, true);
}

function updatePlayhead() {
  if (!videoDuration) return;
  playhead.style.left = (previewVideo.currentTime / videoDuration) * 100 + '%';
}

// ── Thumbnail extraction ──────────────────────────────────────────────────────
function queueThumb(fullPath, imgEl, cardEl, observer) {
  if (thumbCache.has(fullPath)) { imgEl.src = thumbCache.get(fullPath); observer.disconnect(); return; }
  // Remove any stale entry for this path so the fresh one takes priority
  const existing = thumbQueue.findIndex(t => t.fullPath === fullPath);
  if (existing !== -1) thumbQueue.splice(existing, 1);
  // Prepend so currently-visible cards are always processed before older queued ones
  thumbQueue.unshift({ fullPath, imgEl, cardEl, observer });
  drainThumbQueue();
}

function drainThumbQueue() {
  while (activeExtractions < MAX_THUMB_CONCURRENT && thumbQueue.length > 0) {
    const item = thumbQueue.shift();
    if (!item.cardEl.isConnected) continue; // card was removed from DOM, skip
    if (thumbCache.has(item.fullPath)) {     // cached since it was queued
      if (item.imgEl.isConnected) item.imgEl.src = thumbCache.get(item.fullPath);
      item.observer.disconnect();
      continue;
    }
    activeExtractions++;
    extractThumb(item.fullPath, item.imgEl).then(() => {
      item.observer.disconnect();
    }).finally(() => {
      activeExtractions--;
      drainThumbQueue();
    });
  }
}

async function extractThumb(fullPath, imgEl) {
  if (thumbCache.has(fullPath)) { if (imgEl.isConnected) imgEl.src = thumbCache.get(fullPath); return; }
  const url = await api.getThumbnail(fullPath, 3);
  if (url) {
    thumbCache.set(fullPath, url);
    if (imgEl.isConnected) imgEl.src = url;
  }
}

// ── Dropdown ──────────────────────────────────────────────────────────────────
function closeOpenDropdown() {
  if (openDropdown) { openDropdown.classList.remove('open'); openDropdown = null; }
}
document.addEventListener('click', e => {
  if (!e.target.closest('.vid-menu-btn') && !e.target.closest('.vid-dropdown')) closeOpenDropdown();
});

// ── Folder / scan ─────────────────────────────────────────────────────────────
async function openFolder() {
  const folder = await api.openFolder();
  if (!folder) return;
  rootFolder = folder;
  await api.savePrefs({ lastFolder: folder });
  rootFolderDisplay.textContent = folder;
  await scanAndRender();
  startWatching(folder);
}

async function scanAndRender() {
  if (!rootFolder) return;
  const { videos, categories: cats } = await api.scanFolder(rootFolder);
  allVideos  = videos;
  categories = cats;
  buildCategoryPills(cats);
  syncGrid();
  startWatching(rootFolder);
}

function openCatDropdown() {
  const rect = catTrigger.getBoundingClientRect();
  catDropdownMenu.style.top  = (rect.bottom + 6) + 'px';
  catDropdownMenu.style.left = rect.left + 'px';
  catDropdownMenu.classList.add('open');
  catTrigger.classList.add('open');
  catSearch.value = '';
  renderCatOptions('');
  catSearch.focus();
}

function closeCatDropdown() {
  catDropdownMenu.classList.remove('open');
  catTrigger.classList.remove('open');
}

catTrigger.addEventListener('click', e => {
  e.stopPropagation();
  catDropdownMenu.classList.contains('open') ? closeCatDropdown() : openCatDropdown();
});

catSearch.addEventListener('input', () => renderCatOptions(catSearch.value.toLowerCase()));

document.addEventListener('click', e => {
  if (!e.target.closest('#cat-dropdown-wrap')) closeCatDropdown();
});

function renderCatOptions(filter) {
  const all = ['all', ...categories];
  const filtered = all.filter(c => c.toLowerCase().includes(filter));
  catDropdownList.innerHTML = '';
  if (!filtered.length) {
    catDropdownList.innerHTML = '<div class="cat-option-empty">No matches</div>';
    return;
  }
  filtered.forEach(cat => {
    const el = document.createElement('div');
    el.className = 'cat-option' + (cat === activeCategory ? ' active' : '');
    el.textContent = cat === 'all' ? 'All' : cat;
    el.addEventListener('click', () => {
      activeCategory = cat;
      catTriggerLabel.textContent = cat === 'all' ? 'All' : cat;
      catTrigger.classList.toggle('active', cat !== 'all');
      closeCatDropdown();
      syncGrid();
    });
    catDropdownList.appendChild(el);
  });
}

function buildCategoryPills(cats) {
  categories = cats;
  // Reset if active category no longer exists
  if (activeCategory !== 'all' && !cats.includes(activeCategory)) {
    activeCategory = 'all';
    catTriggerLabel.textContent = 'All';
    catTrigger.classList.remove('active');
  }
  // Update label in case it was set before
  catTriggerLabel.textContent = activeCategory === 'all' ? 'All' : activeCategory;
  catTrigger.classList.toggle('active', activeCategory !== 'all');
}


function getFilteredVideos() {
  let list = activeCategory === 'all' ? [...allVideos] : allVideos.filter(v => v.category === activeCategory);
  list.sort((a, b) => sortOrder === 'newest' ? b.mtime - a.mtime : a.mtime - b.mtime);
  return list;
}

function syncGrid() {
  const list = getFilteredVideos();

  if (!list.length) {
    cardMap.forEach((card, path) => {
      const vid = card.querySelector('video');
      if (vid) { vid.pause(); vid.src = ''; vid.load(); }
      card.remove();
    });
    cardMap.clear();
    gridEmpty.classList.add('show');
    return;
  }
  gridEmpty.classList.remove('show');

  // Remove cards whose files are no longer in the list
  const newPaths = new Set(list.map(v => v.fullPath));
  cardMap.forEach((card, path) => {
    if (!newPaths.has(path)) {
      const vid = card.querySelector('video');
      if (vid) { vid.pause(); vid.src = ''; vid.load(); }
      card.remove();
      cardMap.delete(path);
    }
  });

  // Add new cards and enforce sort order via DOM position
  list.forEach((v, i) => {
    let card = cardMap.get(v.fullPath);
    if (!card) {
      card = makeCard(v);
      cardMap.set(v.fullPath, card);
    }
    const sibling = videoGrid.children[i];
    if (sibling !== card) videoGrid.insertBefore(card, sibling ?? null);
  });
}

function makeCard(v) {
  const card = document.createElement('div');
  card.className = 'vid-card';

  // Dropdown
  const dropdown = document.createElement('div');
  dropdown.className = 'vid-dropdown';
  dropdown.innerHTML = `
    <div class="vid-dropdown-item" data-action="explorer">
      <svg width="12" height="12" viewBox="0 0 12 12" fill="none"><rect x="1" y="1" width="4.5" height="4.5" rx="1" stroke="currentColor" stroke-width="1.2"/><rect x="6.5" y="1" width="4.5" height="4.5" rx="1" stroke="currentColor" stroke-width="1.2"/><rect x="1" y="6.5" width="4.5" height="4.5" rx="1" stroke="currentColor" stroke-width="1.2"/><rect x="6.5" y="6.5" width="4.5" height="4.5" rx="1" stroke="currentColor" stroke-width="1.2"/></svg>
      Show in Explorer
    </div>
    <div class="vid-dropdown-sep"></div>
    <div class="vid-dropdown-item danger" data-action="delete">
      <svg width="12" height="12" viewBox="0 0 12 12" fill="none"><path d="M2 3h8M5 3V2h2v1M5 5v4M7 5v4M3 3l.5 7h5l.5-7" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round"/></svg>
      Delete file
    </div>`;

  dropdown.addEventListener('click', async e => {
    const action = e.target.closest('[data-action]')?.dataset.action;
    closeOpenDropdown();
    if (action === 'explorer') {
      api.openInExplorer(v.fullPath);
    } else if (action === 'delete') {
      if (!confirm(`Delete "${v.name}"?\n\nThis cannot be undone.`)) return;
      api.watchFolder('');
      thumbCache.delete(v.fullPath);
      if (currentClip?.fullPath === v.fullPath) { closeModal(); }
      await new Promise(r => setTimeout(r, 500));
      const result = await api.deleteFile(v.fullPath);
      if (result.success) {
        showToast(`✓ Deleted ${v.name}`, 'success');
        await scanAndRender();
      } else {
        startWatching(rootFolder);
        showToast('Delete failed: ' + result.error, 'error', 4000);
      }
    }
  });

  const srcUrl = 'file:///' + v.fullPath.replace(/\\/g, '/');

  card.innerHTML = `
    <div class="vid-thumb">
      <img class="vid-thumb-img" alt="" />
      <div class="thumb-overlay">
        <div class="play-circle">
          <svg width="13" height="14" viewBox="0 0 13 14" fill="#0a0a0b"><path d="M0 0L13 7L0 14V0Z"/></svg>
        </div>
      </div>
    </div>
    <div class="vid-card-body">
      <div class="vid-card-top">
        <div class="vid-name">${v.name}</div>
        <button class="vid-menu-btn" title="Options">⋮</button>
      </div>
      <div class="vid-card-meta">
        <span class="vid-category">${v.category}</span>
        <div class="vid-meta-text">
          <span>${fmtBytes(v.size)}</span>
          <span>${fmtDate(v.mtime)}</span>
        </div>
      </div>
    </div>`;

  card.appendChild(dropdown);

  const thumbImg = card.querySelector('.vid-thumb-img');

  // Keep observing until extraction succeeds — scroll-away dequeues stale items,
  // scroll-back re-queues with priority (unshift). Observer is disconnected after extraction.
  const observer = new IntersectionObserver(entries => {
    entries.forEach(entry => {
      if (entry.isIntersecting) queueThumb(v.fullPath, thumbImg, card, observer);
    });
  });
  observer.observe(card);

  // Hover play preview — reuses single shared hoverVid element
  const thumbEl = card.querySelector('.vid-thumb');
  thumbEl.addEventListener('mouseenter', () => {
    if (modalOverlay.classList.contains('open')) return;
    thumbEl.appendChild(hoverVid);
    hoverVid.style.display = '';
    hoverVid.src = srcUrl;
    hoverVid.currentTime = 0;
    hoverVid.play().catch(() => {});
  });
  thumbEl.addEventListener('mouseleave', () => {
    hoverVid.pause();
    hoverVid.style.display = 'none';
    hoverVid.src = '';
    if (hoverVid.parentNode) hoverVid.parentNode.removeChild(hoverVid);
  });

  // Click card body = open modal
  card.querySelector('.vid-card-body').addEventListener('click', () => openModal(v));
  card.querySelector('.vid-thumb').addEventListener('click', () => openModal(v));

  // Menu button
  card.querySelector('.vid-menu-btn').addEventListener('click', e => {
    e.stopPropagation();
    if (openDropdown === dropdown) { closeOpenDropdown(); return; }
    closeOpenDropdown();
    dropdown.classList.add('open');
    openDropdown = dropdown;
  });

  return card;
}

// ── Modal ─────────────────────────────────────────────────────────────────────
function openModal(clip) {
  currentClip = clip;
  modalFilename.textContent = clip.name;
  modalCategory.textContent = clip.category;
  previewVideo.src = 'file:///' + clip.fullPath.replace(/\\/g, '/');
  previewVideo.load();
  previewVideo.addEventListener('loadedmetadata', onVideoLoaded, { once: true });

  // Stop any active hover preview
  hoverVid.pause();
  hoverVid.style.display = 'none';
  hoverVid.src = '';
  if (hoverVid.parentNode) hoverVid.parentNode.removeChild(hoverVid);

  updateNavButtons();
  modalOverlay.classList.add('open');
}

function updateNavButtons() {
  const disabled = getFilteredVideos().length < 2;
  prevClipBtn.disabled = disabled;
  nextClipBtn.disabled = disabled;
}

function navigateClip(dir) {
  const list = getFilteredVideos();
  if (list.length < 2) return;
  const idx  = list.findIndex(v => v.fullPath === currentClip?.fullPath);
  const next = list[(idx + dir + list.length) % list.length];
  previewVideo.pause();
  openModal(next);
}

function closeModal() {
  if (document.fullscreenElement) document.exitFullscreen().catch(() => {});
  previewVideo.pause();
  previewVideo.src = '';
  previewVideo.load();
  modalOverlay.classList.remove('open');
  currentClip = null;
  videoDuration = 0;
}

function onVideoLoaded() {
  videoDuration = previewVideo.duration;
  modalDuration.textContent = fmt(videoDuration);
  trimStart = 0;
  trimEnd = videoDuration;
  updateTimelineUI();
}

modalCloseBtn.addEventListener('click', closeModal);
modalOverlay.addEventListener('click', e => { if (e.target === modalOverlay) closeModal(); });
prevClipBtn.addEventListener('click', () => navigateClip(-1));
nextClipBtn.addEventListener('click', () => navigateClip(1));
document.addEventListener('keydown', e => {
  if (!modalOverlay.classList.contains('open')) return;
  if (e.key === 'Escape') { closeModal(); return; }
  if (document.activeElement?.tagName === 'INPUT') return;
  if (e.key === 'ArrowLeft')  navigateClip(-1);
  if (e.key === 'ArrowRight') navigateClip(1);
});

modalExplorerBtn.addEventListener('click', () => { if (currentClip) api.openInExplorer(currentClip.fullPath); });

// ── Fullscreen ────────────────────────────────────────────────────────────────
function toggleFullscreen() {
  if (!document.fullscreenElement) {
    trimModal.requestFullscreen().catch(() => {});
  } else {
    document.exitFullscreen().catch(() => {});
  }
}

modalFullscreenBtn.addEventListener('click', toggleFullscreen);

document.addEventListener('fullscreenchange', () => {
  const isFs = !!document.fullscreenElement;
  fsExpandIcon.style.display   = isFs ? 'none'  : '';
  fsCompressIcon.style.display = isFs ? ''      : 'none';
  modalFullscreenBtn.title = isFs ? 'Exit fullscreen (F)' : 'Fullscreen (F)';
});

document.addEventListener('keydown', e => {
  if (e.key === 'f' || e.key === 'F') {
    if (modalOverlay.classList.contains('open') && document.activeElement?.tagName !== 'INPUT') {
      toggleFullscreen();
    }
  }
});

// ── Volume ────────────────────────────────────────────────────────────────────
const VOL_ICONS = {
  muted: `<path d="M0 4.5h2.5L6 1.5v11L2.5 9.5H0V4.5ZM13.5 3.5l-5 7M8.5 3.5l5 7" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" fill="none"/>`,
  low:   `<path d="M0 4.5h2.5L6 1.5v11L2.5 9.5H0V4.5Z"/><path d="M8.5 5.5a3 3 0 0 1 0 3" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" fill="none"/>`,
  high:  `<path d="M0 4.5h2.5L6 1.5v11L2.5 9.5H0V4.5Z"/><path d="M8.5 4a5 5 0 0 1 0 6M10.5 2a8 8 0 0 1 0 10" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" fill="none"/>`,
};

function updateVolIcon() {
  if (previewVideo.muted || previewVideo.volume === 0) {
    volIcon.innerHTML = VOL_ICONS.muted;
  } else if (previewVideo.volume < 0.5) {
    volIcon.innerHTML = VOL_ICONS.low;
  } else {
    volIcon.innerHTML = VOL_ICONS.high;
  }
}

function applyVolume(vol, muted) {
  previewVideo.volume = vol;
  previewVideo.muted  = muted;
  volumeSlider.value  = muted ? 0 : vol;
  updateVolIcon();
}

muteBtn.addEventListener('click', () => {
  const nowMuted = !previewVideo.muted;
  previewVideo.muted = nowMuted;
  if (!nowMuted && previewVideo.volume === 0) previewVideo.volume = 0.5;
  volumeSlider.value = previewVideo.muted ? 0 : previewVideo.volume;
  updateVolIcon();
  api.savePrefs({ volume: previewVideo.volume, muted: previewVideo.muted });
});

volumeSlider.addEventListener('input', () => {
  const vol = parseFloat(volumeSlider.value);
  previewVideo.volume = vol;
  previewVideo.muted  = vol === 0;
  updateVolIcon();
  api.savePrefs({ volume: vol, muted: previewVideo.muted });
});

document.addEventListener('keydown', e => {
  if (e.key === 'm' || e.key === 'M') {
    if (modalOverlay.classList.contains('open') && document.activeElement?.tagName !== 'INPUT') {
      muteBtn.click();
    }
  }
});

updateVolIcon();

// ── Timeline ──────────────────────────────────────────────────────────────────
handleStart.addEventListener('mousedown', e => { e.preventDefault(); isDragging = 'start'; });
handleEnd.addEventListener('mousedown',   e => { e.preventDefault(); isDragging = 'end'; });
timeline.addEventListener('mousedown', e => {
  if (e.target === handleStart || e.target === handleEnd) return;
  e.preventDefault();
  const rect = timeline.getBoundingClientRect();
  previewVideo.currentTime = posToTime(e.clientX - rect.left);
  isDragging = 'seek';
});

document.addEventListener('mousemove', e => {
  if (!isDragging) return;
  const rect = timeline.getBoundingClientRect();
  const t = posToTime(e.clientX - rect.left);
  if (isDragging === 'start') { trimStart = Math.max(0, Math.min(t, trimEnd - 0.05)); updateTimelineUI(); }
  else if (isDragging === 'end') { trimEnd = Math.min(videoDuration, Math.max(t, trimStart + 0.05)); updateTimelineUI(); }
  else if (isDragging === 'seek') { previewVideo.currentTime = Math.max(0, Math.min(t, videoDuration)); }
});
document.addEventListener('mouseup', () => { isDragging = null; });
previewVideo.addEventListener('timeupdate', updatePlayhead);

// ── Playback ──────────────────────────────────────────────────────────────────
const playIconSVG  = `<svg width="13" height="14" viewBox="0 0 13 14" fill="currentColor"><path d="M0 0L13 7L0 14V0Z"/></svg>`;
const pauseIconSVG = `<svg width="13" height="14" viewBox="0 0 13 14" fill="currentColor"><rect x="0" y="0" width="4" height="14" rx="1"/><rect x="8" y="0" width="4" height="14" rx="1"/></svg>`;

playBtn.addEventListener('click', () => { if (previewVideo.paused) previewVideo.play(); else previewVideo.pause(); });
previewVideo.addEventListener('play', () => { const i = document.getElementById('play-icon'); if (i) i.outerHTML = pauseIconSVG; });
previewVideo.addEventListener('pause', () => {
  const i = playBtn.querySelector('svg');
  if (i) i.outerHTML = playIconSVG.replace('<svg ', '<svg id="play-icon" ');
  else playBtn.innerHTML = playIconSVG.replace('<svg ', '<svg id="play-icon" ');
});

playSelBtn.addEventListener('click', () => {
  previewVideo.currentTime = trimStart;
  previewVideo.play();
  const check = () => { if (previewVideo.currentTime >= trimEnd) { previewVideo.pause(); previewVideo.removeEventListener('timeupdate', check); } };
  previewVideo.addEventListener('timeupdate', check);
});

// ── Sort ──────────────────────────────────────────────────────────────────────
sortBtn.addEventListener('click', () => {
  sortOrder = sortOrder === 'newest' ? 'oldest' : 'newest';
  sortLabel.textContent = sortOrder === 'newest' ? 'Newest' : 'Oldest';
  sortArrow.textContent = sortOrder === 'newest' ? '↓' : '↑';
  syncGrid();
});

// ── Trim ──────────────────────────────────────────────────────────────────────
async function doTrim(saveMode) {
  if (!currentClip) return;
  const duration = trimEnd - trimStart;
  if (duration < 0.1) { showToast('Selection too short', 'error'); return; }
  if (trimStart === 0 && Math.abs(trimEnd - videoDuration) < 0.05) { showToast('No trimming needed', 'error'); return; }

  api.watchFolder('');
  progressOverlay.classList.add('show');
  progSub.textContent = saveMode === 'replace' ? 'Replacing original…' : 'Saving new file…';

  const result = await api.trimVideo({
    inputPath: currentClip.fullPath,
    startTime: trimStart,
    endTime: trimEnd,
    saveMode: saveMode === 'replace' ? 'replace' : 'new',
  });

  progressOverlay.classList.remove('show');

  if (result.success) {
    showToast(saveMode === 'replace' ? '✓ Original replaced' : '✓ Saved: ' + result.outputPath.split('\\').pop(), 'success', 3500);
    if (saveMode === 'replace') {
      thumbCache.delete(currentClip.fullPath); // force fresh thumbnail after replace
      previewVideo.load();
      previewVideo.addEventListener('loadedmetadata', onVideoLoaded, { once: true });
    }
    closeModal();
    await scanAndRender();
  } else {
    startWatching(rootFolder);
    showToast('Error: ' + result.error, 'error', 5000);
  }
}

saveNewBtn.addEventListener('click', () => doTrim('new'));
replaceBtn.addEventListener('click', () => {
  if (!confirm(`Replace "${currentClip?.name}" with the trimmed version? This cannot be undone.`)) return;
  doTrim('replace');
});

// ── Watcher ───────────────────────────────────────────────────────────────────
function startWatching(folder) {
  watchedFolder = folder;
  api.watchFolder(folder);
}

api.onFolderChange(() => {
  if (!rootFolder) return;
  clearTimeout(reloadTimer);
  reloadTimer = setTimeout(() => scanAndRender(), 800);
});

// ── Change folder button ──────────────────────────────────────────────────────
changeFolderBtn.addEventListener('click', openFolder);
emptyOpenBtn.addEventListener('click', openFolder);
window.addEventListener('resize', updateTimelineUI);

// ── Init ──────────────────────────────────────────────────────────────────────
setTimeout(async () => {
  const prefs = await api.loadPrefs();
  if (prefs?.volume != null) applyVolume(prefs.volume, prefs.muted ?? false);
  if (prefs?.lastFolder) {
    rootFolder = prefs.lastFolder;
    rootFolderDisplay.textContent = prefs.lastFolder;
    await scanAndRender();
  } else {
    gridEmpty.classList.add('show');
  }
}, 0);