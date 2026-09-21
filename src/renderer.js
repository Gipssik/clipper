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
const metaCache           = new Map(); // fullPath → probed stream metadata
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

function stopHoverPreview() {
  hoverVid.pause();
  hoverVid.style.display = 'none';
  hoverVid.src = '';
  if (hoverVid.parentNode) hoverVid.parentNode.removeChild(hoverVid);
}

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

// ── Quality badges ────────────────────────────────────────────────────────────
// Quality is named after the short side, so a 1080x1920 phone clip still reads "1080p".
function qualityLabel(meta) {
  if (!meta || !meta.width || !meta.height) return null;
  const q = Math.min(meta.width, meta.height);
  let label;
  if (q >= 4320) label = '8K';
  else if (q >= 2160) label = '4K';
  else if (q >= 1440) label = '1440p';
  else if (q >= 1080) label = '1080p';
  else if (q >= 900)  label = '900p';
  else if (q >= 720)  label = '720p';
  else if (q >= 540)  label = '540p';
  else if (q >= 480)  label = '480p';
  else if (q >= 360)  label = '360p';
  else label = q + 'p';
  // High-framerate footage is worth calling out — "1080p60" is how people describe it.
  if (meta.fps && meta.fps >= 50 && label.endsWith('p')) label += Math.round(meta.fps);
  return label;
}

const CODEC_NAMES = { h264: 'H.264', hevc: 'H.265', av1: 'AV1', vp9: 'VP9', vp8: 'VP8', mpeg4: 'MPEG-4', wmv3: 'WMV', vc1: 'VC-1' };
function codecLabel(codec) {
  return CODEC_NAMES[codec] || (codec ? codec.toUpperCase() : '');
}

function applyBadges(cardEl, meta) {
  const isAv1 = meta && meta.videoCodec === 'av1';
  const isHdr = !!(meta && meta.isHdr);

  // Converting is only offered for AV1 — for anything else it would just be a
  // lossy round-trip with nothing gained, so the entry stays hidden.
  const convertItem = cardEl.querySelector('[data-action="convert"]');
  if (convertItem) convertItem.style.display = isAv1 ? '' : 'none';

  // Same idea for tone mapping: pointless on footage that is already SDR.
  const sdrItem = cardEl.querySelector('[data-action="sdr"]');
  if (sdrItem) sdrItem.style.display = isHdr ? '' : 'none';

  const wrap = cardEl.querySelector('.vid-badges');
  if (!wrap) return;
  const label = qualityLabel(meta);
  if (!label) { wrap.innerHTML = ''; return; }

  const short = Math.min(meta.width, meta.height);
  const cls = short >= 1080 ? 'res-high' : short <= 480 ? 'res-low' : '';
  let html = `<span class="q-badge ${cls}">${label}</span>`;
  if (isAv1) html += '<span class="q-badge codec-av1">AV1</span>';
  if (isHdr) html += `<span class="q-badge hdr">${meta.hdrFormat === 'hlg' ? 'HLG' : 'HDR'}</span>`;
  wrap.innerHTML = html;
}

function applyBadgesFor(fullPath) {
  const card = cardMap.get(fullPath);
  const meta = metaCache.get(fullPath);
  if (card && meta) applyBadges(card, meta);
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

// Re-arms lazy extraction for a card whose cached still was invalidated.
function requeueThumb(fullPath, cardEl) {
  const imgEl = cardEl.querySelector('.vid-thumb-img');
  if (!imgEl) return;
  const observer = new IntersectionObserver(entries => {
    entries.forEach(entry => {
      if (entry.isIntersecting) queueThumb(fullPath, imgEl, cardEl, observer);
    });
  });
  observer.observe(cardEl);
}

async function extractThumb(fullPath, imgEl) {
  if (thumbCache.has(fullPath)) { if (imgEl.isConnected) imgEl.src = thumbCache.get(fullPath); return; }
  // One ffmpeg pass yields both the poster frame and the stream metadata behind the badge.
  const res = await api.getThumbnail(fullPath, settings.thumbTime);
  if (res && res.meta) {
    metaCache.set(fullPath, res.meta);
    applyBadgesFor(fullPath);
  }
  if (res && res.dataUrl) {
    thumbCache.set(fullPath, res.dataUrl);
    if (imgEl.isConnected) imgEl.src = res.dataUrl;
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
    } else if (card._video.mtime !== v.mtime || card._video.size !== v.size) {
      // Same path, different file: something rewrote it in place.
      refreshCard(card, v);
    }
    // Handlers read through this, so they never act on a record the file outgrew.
    card._video = v;
    const sibling = videoGrid.children[i];
    if (sibling !== card) videoGrid.insertBefore(card, sibling ?? null);
  });
}

// Everything a card displays — badges, still, size and date — is derived once, when the
// card is built. A replace-mode trim, compress, convert or export (or an edit from
// outside the app) leaves the path alone and changes the file under it, so the card has
// to drop all of that and derive it again rather than keep asserting what used to be true.
function refreshCard(card, v) {
  thumbCache.delete(v.fullPath);
  metaCache.delete(v.fullPath);

  const badges = card.querySelector('.vid-badges');
  if (badges) badges.innerHTML = '';

  // Both of these are offered only for particular source formats, and until the probe
  // lands again we no longer know what this file is.
  ['convert', 'sdr'].forEach(action => {
    const item = card.querySelector(`[data-action="${action}"]`);
    if (item) item.style.display = 'none';
  });

  const metaText = card.querySelector('.vid-meta-text');
  if (metaText) metaText.innerHTML = `<span>${fmtBytes(v.size)}</span><span>${fmtDate(v.mtime)}</span>`;

  const img = card.querySelector('.vid-thumb-img');
  if (img) img.removeAttribute('src');
  requeueThumb(v.fullPath, card);
}

function makeCard(v) {
  const card = document.createElement('div');
  card.className = 'vid-card';

  // Dropdown
  const dropdown = document.createElement('div');
  dropdown.className = 'vid-dropdown';
  dropdown.innerHTML = `
    <div class="vid-dropdown-item" data-action="export">
      <svg width="12" height="12" viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round"><path d="M6 8V1.5M6 1.5L3.8 3.7M6 1.5l2.2 2.2M1.5 7.5v2a1 1 0 001 1h7a1 1 0 001-1v-2"/></svg>
      <span class="export-label">${exportMenuLabel()}</span>
    </div>
    <div class="vid-dropdown-sep"></div>
    <div class="vid-dropdown-item" data-action="explorer">
      <svg width="12" height="12" viewBox="0 0 12 12" fill="none"><rect x="1" y="1" width="4.5" height="4.5" rx="1" stroke="currentColor" stroke-width="1.2"/><rect x="6.5" y="1" width="4.5" height="4.5" rx="1" stroke="currentColor" stroke-width="1.2"/><rect x="1" y="6.5" width="4.5" height="4.5" rx="1" stroke="currentColor" stroke-width="1.2"/><rect x="6.5" y="6.5" width="4.5" height="4.5" rx="1" stroke="currentColor" stroke-width="1.2"/></svg>
      Show in Explorer
    </div>
    <div class="vid-dropdown-sep"></div>
    <div class="vid-dropdown-item" data-action="compress">
      <svg width="12" height="12" viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round"><path d="M1 1.5h10M1 10.5h10M6 3.5v5M4 5.5L6 3.5l2 2M4 6.5L6 8.5l2-2"/></svg>
      Compress…
    </div>
    <div class="vid-dropdown-item" data-action="convert" style="display:none">
      <svg width="12" height="12" viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round"><path d="M2 4.5h7L7 2.5M10 7.5H3l2 2"/></svg>
      Convert to MP4…
    </div>
    <div class="vid-dropdown-item" data-action="sdr" style="display:none">
      <svg width="12" height="12" viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="1.2"><circle cx="6" cy="6" r="4.2"/><path d="M6 1.8v8.4" stroke-linecap="round"/><path d="M6 1.8a4.2 4.2 0 010 8.4z" fill="currentColor" stroke="none"/></svg>
      Convert HDR → SDR…
    </div>
    <div class="vid-dropdown-sep"></div>
    <div class="vid-dropdown-item danger" data-action="delete">
      <svg width="12" height="12" viewBox="0 0 12 12" fill="none"><path d="M2 3h8M5 3V2h2v1M5 5v4M7 5v4M3 3l.5 7h5l.5-7" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round"/></svg>
      Delete file
    </div>`;

  dropdown.addEventListener('click', async e => {
    const action = e.target.closest('[data-action]')?.dataset.action;
    closeOpenDropdown();
    const rec = card._video || v;   // this file may have been rewritten since the card was built
    if (action === 'export') {
      startExport(rec);
    } else if (action === 'explorer') {
      api.openInExplorer(rec.fullPath);
    } else if (action === 'compress') {
      openEncodeModal(rec, 'compress');
    } else if (action === 'convert') {
      openEncodeModal(rec, 'convert');
    } else if (action === 'sdr') {
      openEncodeModal(rec, 'sdr');
    } else if (action === 'delete') {
      if (!confirm(`Delete "${rec.name}"?\n\nThis cannot be undone.`)) return;
      api.watchFolder('');
      thumbCache.delete(rec.fullPath);
      if (currentClip?.fullPath === rec.fullPath) { closeModal(); }
      await new Promise(r => setTimeout(r, 500));
      const result = await api.deleteFile(rec.fullPath);
      if (result.success) {
        showToast(`✓ Deleted ${rec.name}`, 'success');
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
      <div class="vid-badges"></div>
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
  if (metaCache.has(v.fullPath)) applyBadges(card, metaCache.get(v.fullPath));

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
    if (!settings.hoverPreview) return;
    if (modalOverlay.classList.contains('open')) return;
    thumbEl.appendChild(hoverVid);
    hoverVid.style.display = '';
    hoverVid.src = srcUrl;
    hoverVid.currentTime = 0;
    hoverVid.play().catch(() => {});
  });
  thumbEl.addEventListener('mouseleave', stopHoverPreview);

  // Click card body = open modal
  card.querySelector('.vid-card-body').addEventListener('click', () => openModal(card._video || v));
  card.querySelector('.vid-thumb').addEventListener('click', () => openModal(card._video || v));

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

  stopHoverPreview();

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
  if (prefs?.settings) settings = { ...SETTINGS_DEFAULTS, ...prefs.settings };
  applySettings();
  if (prefs?.volume != null) applyVolume(prefs.volume, prefs.muted ?? false);
  if (prefs?.lastFolder) {
    rootFolder = prefs.lastFolder;
    rootFolderDisplay.textContent = prefs.lastFolder;
    await scanAndRender();
  } else {
    gridEmpty.classList.add('show');
  }
}, 0);
// ── Encode modal (compress / convert) ─────────────────────────────────────────
const encodeOverlay   = document.getElementById('encode-overlay');
const encodeTitle     = document.getElementById('encode-title');
const encodeSubject   = document.getElementById('encode-subject');
const encodeCloseBtn  = document.getElementById('encode-close-btn');
const encodeNote      = document.getElementById('encode-note');
const srcRes          = document.getElementById('src-res');
const srcCodec        = document.getElementById('src-codec');
const srcBitrate      = document.getElementById('src-bitrate');
const srcSize         = document.getElementById('src-size');
const resSeg          = document.getElementById('res-seg');
const rateSeg         = document.getElementById('rate-seg');
const crfField        = document.getElementById('crf-field');
const crfSlider       = document.getElementById('crf-slider');
const crfValue        = document.getElementById('crf-value');
const crfTip          = document.getElementById('crf-tip');
const bitrateField    = document.getElementById('bitrate-field');
const bitrateInput    = document.getElementById('bitrate-input');
const tonemapField    = document.getElementById('tonemap-field');
const tonemapSeg      = document.getElementById('tonemap-seg');
const tonemapValue    = document.getElementById('tonemap-value');
const tonemapTip      = document.getElementById('tonemap-tip');
const fpsSelect       = document.getElementById('fps-select');
const audioSelect     = document.getElementById('audio-select');
const encoderSelect   = document.getElementById('encoder-select');
const encoderTip      = document.getElementById('encoder-tip');
const speedSlider     = document.getElementById('speed-slider');
const speedValue      = document.getElementById('speed-value');
const estSize         = document.getElementById('est-size');
const estDelta        = document.getElementById('est-delta');
const encodeSaveNew   = document.getElementById('encode-save-new');
const encodeReplace   = document.getElementById('encode-replace');
const progTitle       = document.getElementById('prog-title');
const progBarWrap     = document.getElementById('prog-bar-wrap');
const progBar         = document.getElementById('prog-bar');
const progCancelBtn   = document.getElementById('prog-cancel-btn');

let encodeClip   = null;   // the clip the modal is acting on
let encodeMeta   = null;   // its probed stream metadata
let encodeKind   = 'compress';
let encoderList  = null;   // { software: [...], hardware: [...] } — probed once
let filterCaps   = null;   // { zscale, tonemap } — whether this ffmpeg can tone map

const RES_LADDER = [1440, 1080, 720, 480];
const SPEED_NAMES = ['Ultra fast', 'Super fast', 'Very fast', 'Faster', 'Fast', 'Medium', 'Slow', 'Slower'];

let encoderTouched = false;  // once the user picks an encoder, stop re-defaulting it

const enc = {
  targetHeight: null,
  rateMode: 'crf',
  crf: 23,
  bitrateKbps: 4000,
  fps: null,
  audio: 'copy',
  encoder: 'libx264',
  speed: 5,
  toneMap: null,      // null = leave the source's dynamic range alone
};

// ── Quality descriptors ───────────────────────────────────────────────────────
function crfDescriptor(crf) {
  if (crf <= 18) return 'Visually lossless';
  if (crf <= 21) return 'Excellent';
  if (crf <= 24) return 'Good';
  if (crf <= 27) return 'Fair';
  if (crf <= 30) return 'Noticeably soft';
  return 'Low — visible blocking';
}

function crfTipText(crf, family) {
  const scaleNote = family === 'hevc'
    ? ' On H.265 the scale runs a few points higher than H.264 — CRF 28 here looks about like CRF 23 there.'
    : family === 'av1'
    ? ' On AV1 the scale runs higher still — CRF 30 here looks about like CRF 23 on H.264.'
    : '';
  if (crf <= 18) return 'Barely distinguishable from the source. Files stay large — often close to the original.' + scaleNote;
  if (crf <= 21) return 'Sharp enough that differences are hard to spot in motion. A safe choice for footage you might edit later.' + scaleNote;
  if (crf <= 24) return 'The usual sweet spot. Roughly half the size of the original with no obvious loss at normal viewing distance.' + scaleNote;
  if (crf <= 27) return 'Clearly smaller. Fine for sharing and watching; fast motion and fine textures start to smear.' + scaleNote;
  if (crf <= 30) return 'Small files, but softness and banding are visible — especially in dark scenes and smoke.' + scaleNote;
  return 'Aggressive. Expect blocking in any busy scene. Use only when size matters more than looks.' + scaleNote;
}

// ── HDR ───────────────────────────────────────────────────────────────────────
const TONEMAP_NAMES = {
  balanced: 'Balanced',
  filmic:   'Filmic',
  punchy:   'Punchy',
};

const TONEMAP_LOOK_TIPS = {
  balanced: 'Holds midtone brightness close to the original and only rolls off the top highlights — the closest match to how the game actually looked. Start here.',
  filmic:   'Filmic S-curve that protects detail in skies, explosions and muzzle flashes. It darkens the whole picture though, so dark scenes can come out murky.',
  punchy:   'Leaves everything below SDR white exactly as graded and hard-clips above it. Most contrast, but the brightest highlights lose all detail.',
};

function tonemapTipText(op, hdrFormat) {
  const curve = hdrFormat === 'hlg' ? 'HLG' : 'HDR10 (PQ)';
  if (!op) {
    return `The clip stays ${curve}. It will look right on an HDR display and washed-out, grey `
         + `and flat on everything else — which is what happens when you send it to someone.`;
  }
  return `Remaps the ${curve} picture into normal SDR colour so it looks the same everywhere. `
       + TONEMAP_LOOK_TIPS[op];
}

function refreshToneMapUi() {
  const m = encodeMeta;
  const isHdr = !!m?.isHdr;
  tonemapField.style.display = isHdr ? '' : 'none';
  if (!isHdr) return;

  // No zscale means no honest tone mapping, so say so instead of offering a broken button.
  const usable = filterCaps ? filterCaps.zscale : true;
  [...tonemapSeg.children].forEach(b => {
    const op = b.dataset.tonemap || null;
    b.disabled = !usable && op !== null;
    b.classList.toggle('active', op === enc.toneMap);
  });

  tonemapValue.textContent = enc.toneMap ? TONEMAP_NAMES[enc.toneMap] : 'Keep HDR';
  tonemapTip.textContent = usable
    ? tonemapTipText(enc.toneMap, m.hdrFormat)
    : 'This ffmpeg build has no zscale filter, so it cannot tone map. Drop a full ffmpeg build into ffmpeg-bin/ to enable it.';
}

const FAMILY_NAMES = { h264: 'H.264', hevc: 'H.265', av1: 'AV1' };

function encoderFamily(id) {
  if (id === 'libsvtav1' || id.startsWith('av1')) return 'av1';
  return id.startsWith('hevc') || id === 'libx265' ? 'hevc' : 'h264';
}
function isHardware(id) {
  return /_(nvenc|qsv|amf)$/.test(id);
}

function encoderTipText(id) {
  const hw = isHardware(id);
  const fam = encoderFamily(id);
  if (hw && fam === 'h264') return 'Runs on the GPU — typically 5–10× faster than the CPU. Files land somewhat larger at the same quality setting.';
  if (hw && fam === 'av1')  return 'GPU AV1 — the smallest files of the three and still fast, but only recent players, browsers and editors open it.';
  if (hw) return 'GPU H.265 — fast and compact, but some editors and older devices will not open it.';
  if (fam === 'av1')  return 'Smallest files at matched quality, and royalty-free. Slow on the CPU and the least widely supported — use the GPU encoder if you have one.';
  if (fam === 'hevc') return 'About 30–40% smaller than H.264 at matched quality, but slow to encode and less widely supported.';
  return 'Best size-for-quality and plays literally everywhere. Slowest option — roughly real-time on long clips.';
}

// ── Estimation ────────────────────────────────────────────────────────────────
// Bits-per-pixel model anchored on a reference CRF per codec family. It is a
// ballpark, not a promise — actual size swings with how much motion is in the clip.
const BPP_REF = {
  h264: { crf: 23, bpp: 0.085 },
  hevc: { crf: 28, bpp: 0.051 },
  av1:  { crf: 30, bpp: 0.045 },
};

function outputDims() {
  const w = encodeMeta?.width, h = encodeMeta?.height;
  if (!w || !h) return null;
  if (!enc.targetHeight) return { w, h };
  // Portrait clips are scaled on their short side, matching what ffmpeg is told to do.
  const portrait = w < h;
  if (portrait) {
    const nw = enc.targetHeight;
    return { w: nw, h: Math.round((h / w) * nw / 2) * 2 };
  }
  const nh = enc.targetHeight;
  return { w: Math.round((w / h) * nh / 2) * 2, h: nh };
}

function audioKbps() {
  if (enc.audio === 'none') return 0;
  if (enc.audio === 'copy') return encodeMeta?.hasAudio ? (encodeMeta.audioKbps || 160) : 0;
  return parseInt(enc.audio);
}

function estimateVideoKbps() {
  const dims = outputDims();
  if (!dims) return null;
  const fps = enc.fps || encodeMeta?.fps || 30;

  if (enc.rateMode === 'bitrate') return enc.bitrateKbps;

  const fam = encoderFamily(enc.encoder);
  const ref = BPP_REF[fam];
  let bpp = ref.bpp * Math.pow(1.13, ref.crf - enc.crf);
  if (isHardware(enc.encoder)) bpp *= 1.25; // fixed-function encoders spend more bits for the same look

  let kbps = (dims.w * dims.h * fps * bpp) / 1000;

  // A re-encode almost never needs more bits than the source did for the same pixel rate,
  // so cap the model against what the original actually used.
  const srcKbps = encodeMeta?.videoKbps;
  if (srcKbps && encodeMeta.width && encodeMeta.height) {
    const srcFps = encodeMeta.fps || fps;
    const ratio = (dims.w * dims.h * fps) / (encodeMeta.width * encodeMeta.height * srcFps);
    kbps = Math.min(kbps, srcKbps * ratio * 1.1);
  }
  return Math.max(80, kbps);
}

function updateEstimate() {
  const dur = encodeMeta?.durationSec;
  const vk = estimateVideoKbps();
  if (!dur || vk == null) { estSize.textContent = '—'; estDelta.textContent = ''; return; }

  const bytes = ((vk + audioKbps()) * 1000 / 8) * dur;
  estSize.textContent = '≈ ' + fmtBytes(bytes);

  const orig = encodeClip?.size;
  if (orig) {
    const pct = Math.round((bytes / orig) * 100);
    const grew = bytes > orig;
    estDelta.textContent = grew
      ? `${pct}% of original — larger`
      : `${pct}% of original — saves ${fmtBytes(orig - bytes)}`;
    estDelta.classList.toggle('grow', grew);
  } else {
    estDelta.textContent = '';
  }
}

// ── Modal population ──────────────────────────────────────────────────────────
function buildResSegment() {
  const srcShort = encodeMeta?.width && encodeMeta?.height
    ? Math.min(encodeMeta.width, encodeMeta.height) : null;

  resSeg.innerHTML = '';
  const opts = [{ h: null, label: 'Keep original' }];
  RES_LADDER.forEach(h => {
    // Upscaling is never a compression win, so only offer rungs below the source.
    if (!srcShort || h < srcShort) opts.push({ h, label: h + 'p' });
  });

  opts.forEach(o => {
    const b = document.createElement('button');
    b.textContent = o.label;
    b.classList.toggle('active', o.h === enc.targetHeight);
    b.addEventListener('click', () => {
      enc.targetHeight = o.h;
      [...resSeg.children].forEach(c => c.classList.remove('active'));
      b.classList.add('active');
      syncBitrateDefault();
      updateEstimate();
    });
    resSeg.appendChild(b);
  });
}

function availableEncoders() {
  const hw = encoderList?.hardware ?? [];
  const sw = encoderList?.software ?? [{ id: 'libx264', family: 'h264', vendor: 'CPU (x264)' }];
  const all = [...hw, ...sw];
  // Convert and HDR→SDR both exist to produce something a friend can just open,
  // so they pin the output to H.264 — offering H.265 would defeat the point.
  return encodeKind === 'compress' ? all : all.filter(e => e.family === 'h264');
}

function buildEncoderSelect() {
  const list = availableEncoders();
  encoderSelect.innerHTML = '';
  list.forEach(e => {
    const o = document.createElement('option');
    o.value = e.id;
    o.textContent = `${e.vendor} · ${FAMILY_NAMES[e.family] || e.family}` + (isHardware(e.id) ? ' (fast)' : '');
    encoderSelect.appendChild(o);
  });

  // Honour the Settings default when it is usable here, else prefer a hardware
  // H.264 encoder, else whatever is left — until the user overrides per clip.
  if (!encoderTouched || !list.some(e => e.id === enc.encoder)) {
    const fromSettings = list.find(e => e.id === settings.defaultEncoder);
    const preferred = fromSettings || list.find(e => e.family === 'h264' && isHardware(e.id)) || list[0];
    enc.encoder = preferred.id;
  }
  encoderSelect.value = enc.encoder;
  encoderTip.textContent = encoderTipText(enc.encoder);
}

function syncBitrateDefault() {
  // Keep the manual bitrate box tracking the CRF model so switching modes is not a cliff.
  if (enc.rateMode === 'crf') {
    const modelled = estimateVideoKbps();
    if (modelled) {
      enc.bitrateKbps = Math.round(modelled / 100) * 100;
      bitrateInput.value = enc.bitrateKbps;
    }
  }
}

function refreshQualityUi() {
  const fam = encoderFamily(enc.encoder);
  crfValue.textContent = `CRF ${enc.crf} · ${crfDescriptor(enc.crf)}`;
  crfTip.textContent = crfTipText(enc.crf, fam);
  speedValue.textContent = SPEED_NAMES[enc.speed] || 'Medium';
}

function setEncodeNote(html) {
  encodeNote.innerHTML = html || '';
  encodeNote.classList.toggle('show', !!html);
}

function fillSourceCells() {
  const m = encodeMeta;
  srcRes.textContent = m?.width
    ? `${m.width}×${m.height}${m.fps ? ' · ' + Math.round(m.fps) + 'fps' : ''}`
    : '—';
  srcCodec.textContent = m?.videoCodec ? codecLabel(m.videoCodec) : '—';
  srcBitrate.textContent = m?.videoKbps ? Math.round(m.videoKbps).toLocaleString() + ' kbps' : '—';
  srcSize.textContent = encodeClip ? fmtBytes(encodeClip.size) : '—';
}

async function openEncodeModal(clip, kind) {
  encodeClip = clip;
  encodeKind = kind;
  encodeMeta = metaCache.get(clip.fullPath) || null;

  encodeTitle.textContent = kind === 'convert' ? 'Convert to MP4'
                          : kind === 'sdr'     ? 'Convert HDR → SDR'
                          : 'Compress';
  encodeSubject.textContent = clip.name;
  encodeOverlay.classList.add('open');

  // Show what we already know, then fill in the rest once the probe lands.
  fillSourceCells();
  setEncodeNote('');
  estSize.textContent = '…';
  estDelta.textContent = '';

  tonemapField.style.display = 'none';

  if (!encoderList) encoderList = await api.getEncoders();
  if (!filterCaps)  filterCaps  = await api.getFilters();
  if (!encodeMeta) {
    const m = await api.probeVideo(clip.fullPath);
    if (m) { encodeMeta = m; metaCache.set(clip.fullPath, m); applyBadgesFor(clip.fullPath); }
  }
  if (encodeClip !== clip) return; // modal was closed or retargeted while probing

  applyKindDefaults();
  fillSourceCells();
  buildResSegment();
  buildEncoderSelect();
  refreshToneMapUi();
  refreshQualityUi();
  syncBitrateDefault();
  updateEstimate();
}

function applyKindDefaults() {
  const m = encodeMeta;
  const srcShort = m?.width && m?.height ? Math.min(m.width, m.height) : null;

  enc.rateMode = 'crf';
  enc.fps = null;
  enc.audio = 'copy';
  enc.speed = 5;
  [...rateSeg.children].forEach(b => b.classList.toggle('active', b.dataset.rate === 'crf'));
  crfField.style.display = '';
  bitrateField.style.display = 'none';
  fpsSelect.value = '';
  audioSelect.value = 'copy';
  speedSlider.value = 5;

  // Tone mapping is only ever on the table for HDR sources, and when it is, leaving it
  // off is almost certainly not what the user wants — an untouched HDR clip is the grey
  // one. Default it on for every kind, including a plain Compress.
  const canToneMap = !!m?.isHdr && (filterCaps ? filterCaps.zscale : true);
  enc.toneMap = canToneMap ? 'balanced' : null;

  if (encodeKind === 'convert' || encodeKind === 'sdr') {
    // Both of these are about playability, not shrinking — keep the picture as-is and
    // pin the output to H.264 so the result opens in anything.
    enc.targetHeight = null;
    enc.crf = 20;

    const codec = m?.videoCodec ? codecLabel(m.videoCodec) : 'this codec';
    if (encodeKind === 'sdr') {
      const curve = m?.hdrFormat === 'hlg' ? 'HLG' : 'HDR10';
      setEncodeNote(canToneMap
        ? `This clip is <strong>${curve}</strong>. Its colours are graded for an HDR display, which is why it looks <em>grey and washed out</em> anywhere else. It will be tone mapped down to normal <strong>SDR</strong> and re-encoded to <strong>H.264 in an .mp4</strong> — the result looks the same on every screen. This is a one-way conversion; keep the original if you still want the HDR version.`
        : `This clip is <strong>${curve}</strong>, but this ffmpeg build cannot tone map — see below. Encoding it now would produce the same washed-out picture, just in a different file.`);
    } else if (m?.videoCodec === 'av1') {
      setEncodeNote(`This clip is <strong>AV1</strong>. It will be decoded and re-encoded to <strong>H.264 in an .mp4</strong>, which every editor, player and upload target accepts. Expect a somewhat <em>larger</em> file than the AV1 original — that is the cost of compatibility.`);
    } else {
      setEncodeNote(`This clip is <strong>${codec}</strong>. It will be re-encoded to <strong>H.264 in an .mp4</strong> for maximum compatibility.`);
    }
  } else {
    // Compress opens one rung below the source so the default already does something.
    enc.targetHeight = srcShort ? (RES_LADDER.find(h => h < srcShort) ?? null) : null;
    enc.crf = 23;
    setEncodeNote(canToneMap
      ? `This clip is <strong>HDR</strong>. Compressing it to 8-bit without tone mapping is what leaves it looking grey, so <strong>HDR → SDR</strong> is switched on below. Turn it off to keep the HDR grade.`
      : '');
  }
  crfSlider.value = enc.crf;
}

function closeEncodeModal() {
  encodeOverlay.classList.remove('open');
  encodeClip = null;
  encodeMeta = null;
}

// ── Modal wiring ──────────────────────────────────────────────────────────────
encodeCloseBtn.addEventListener('click', closeEncodeModal);
encodeOverlay.addEventListener('click', e => { if (e.target === encodeOverlay) closeEncodeModal(); });
document.addEventListener('keydown', e => {
  if (e.key === 'Escape' && encodeOverlay.classList.contains('open')) closeEncodeModal();
});

tonemapSeg.addEventListener('click', e => {
  const btn = e.target.closest('[data-tonemap]');
  if (!btn || btn.disabled) return;
  enc.toneMap = btn.dataset.tonemap || null;
  refreshToneMapUi();
});

rateSeg.addEventListener('click', e => {
  const btn = e.target.closest('[data-rate]');
  if (!btn) return;
  syncBitrateDefault();               // carry the CRF estimate over before switching
  enc.rateMode = btn.dataset.rate;
  [...rateSeg.children].forEach(b => b.classList.remove('active'));
  btn.classList.add('active');
  crfField.style.display     = enc.rateMode === 'crf' ? '' : 'none';
  bitrateField.style.display = enc.rateMode === 'crf' ? 'none' : '';
  updateEstimate();
});

crfSlider.addEventListener('input', () => {
  enc.crf = parseInt(crfSlider.value);
  refreshQualityUi();
  updateEstimate();
});

bitrateInput.addEventListener('input', () => {
  const v = parseInt(bitrateInput.value);
  enc.bitrateKbps = isNaN(v) ? 4000 : Math.max(100, v);
  updateEstimate();
});

fpsSelect.addEventListener('change', () => {
  enc.fps = fpsSelect.value ? parseInt(fpsSelect.value) : null;
  updateEstimate();
});

audioSelect.addEventListener('change', () => {
  enc.audio = audioSelect.value;
  updateEstimate();
});

encoderSelect.addEventListener('change', () => {
  enc.encoder = encoderSelect.value;
  encoderTouched = true;
  encoderTip.textContent = encoderTipText(enc.encoder);
  refreshQualityUi();
  updateEstimate();
});

speedSlider.addEventListener('input', () => {
  enc.speed = parseInt(speedSlider.value);
  refreshQualityUi();
});

encodeSaveNew.addEventListener('click', () => runEncodeJob('new'));
encodeReplace.addEventListener('click', () => {
  const outExt = '.mp4';
  const srcExt = (encodeClip?.name.match(/\.[^.]+$/) || [''])[0].toLowerCase();
  const extNote = srcExt && srcExt !== outExt
    ? `\n\nThe original is a ${srcExt} file — it will be deleted and replaced by an .mp4 with the same name.`
    : '';
  if (!confirm(`Replace "${encodeClip?.name}" with the re-encoded version?${extNote}\n\nThis cannot be undone.`)) return;
  runEncodeJob('replace');
});

// ── Running the job ───────────────────────────────────────────────────────────
function encodeSuffix() {
  if (encodeKind === 'sdr') return '_sdr';
  if (encodeKind === 'convert') return '_h264';
  const dims = outputDims();
  const base = dims ? '_' + Math.min(dims.w, dims.h) + 'p' : '_compressed';
  return enc.toneMap ? base + '_sdr' : base;
}

async function runEncodeJob(saveMode) {
  if (!encodeClip) return;
  const clip = encodeClip;
  const kind = encodeKind;
  const suffix = encodeSuffix();   // depends on encodeMeta, which closing the modal clears
  const toneMap = enc.toneMap;

  closeEncodeModal();
  api.watchFolder('');

  progTitle.textContent = kind === 'sdr' ? 'Converting to SDR…'
                        : kind === 'convert' ? 'Converting…'
                        : 'Compressing…';
  progSub.textContent = 'Starting ffmpeg';
  progBar.style.width = '0%';
  progBarWrap.classList.add('show');
  progCancelBtn.classList.add('show');
  progressOverlay.classList.add('show');

  const result = await api.encodeVideo({
    inputPath: clip.fullPath,
    saveMode,
    suffix,
    container: 'mp4',
    encoder: enc.encoder,
    speed: enc.speed,
    rateMode: enc.rateMode,
    crf: enc.crf,
    bitrateKbps: enc.bitrateKbps,
    targetHeight: enc.targetHeight,
    targetFps: enc.fps,
    toneMap,
    audioMode: enc.audio === 'none' ? 'none' : enc.audio === 'copy' ? 'copy' : 'encode',
    audioKbps: enc.audio === 'copy' || enc.audio === 'none' ? 160 : parseInt(enc.audio),
  });

  progressOverlay.classList.remove('show');
  progBarWrap.classList.remove('show');
  progCancelBtn.classList.remove('show');
  progTitle.textContent = 'Trimming…';

  if (result.cancelled) {
    startWatching(rootFolder);
    showToast('Encode cancelled', 'info');
    return;
  }

  if (result.success) {
    // Cache invalidation belongs to syncGrid, which can tell whether this clip's file
    // actually changed — dropping it here would also throw away a save-as-new source's
    // still and metadata, which are still perfectly good.
    const saved = clip.size - result.size;
    const pct = Math.round((result.size / clip.size) * 100);
    showToast(
      `✓ ${result.outputPath.split('\\').pop()} — ${fmtBytes(result.size)} (${pct}% of original${saved > 0 ? ', saved ' + fmtBytes(saved) : ''})`,
      'success', 5000);
    await scanAndRender();
  } else {
    startWatching(rootFolder);
    showToast('Encode failed: ' + (result.details || result.error), 'error', 6000);
  }
}

progCancelBtn.addEventListener('click', () => {
  progSub.textContent = 'Cancelling…';
  api.cancelEncode();
});

api.onEncodeProgress(p => {
  if (p.percent != null) progBar.style.width = p.percent.toFixed(1) + '%';
  const bits = [];
  if (p.percent != null) bits.push(p.percent.toFixed(0) + '%');
  if (p.speed) {
    bits.push(p.speed.toFixed(1) + '×');
    if (p.totalSec && p.timeSec != null) {
      const remain = (p.totalSec - p.timeSec) / p.speed;
      if (remain > 0 && isFinite(remain)) bits.push(fmt(remain) + ' left');
    }
  }
  progSub.textContent = bits.length ? bits.join('  ·  ') : 'Running ffmpeg';
});

// ── Settings ──────────────────────────────────────────────────────────────────
const settingsBtn       = document.getElementById('settings-btn');
const settingsOverlay   = document.getElementById('settings-overlay');
const settingsCloseBtn  = document.getElementById('settings-close-btn');
const settingsDoneBtn   = document.getElementById('settings-done');
const settingsResetBtn  = document.getElementById('settings-reset');
const fontsizeSlider    = document.getElementById('fontsize-slider');
const fontsizeValue     = document.getElementById('fontsize-value');
const cardsizeSeg       = document.getElementById('cardsize-seg');
const hoverpreviewRow   = document.getElementById('hoverpreview-row');
const hoverpreviewSwitch= document.getElementById('hoverpreview-switch');
const thumbtimeSlider   = document.getElementById('thumbtime-slider');
const thumbtimeValue    = document.getElementById('thumbtime-value');
const defaultEncoderSel = document.getElementById('default-encoder-select');

const SETTINGS_DEFAULTS = {
  fontScale: 100,      // percent
  cardMin: 280,        // px, grid column floor
  hoverPreview: true,
  thumbTime: 3,        // seconds into the clip
  defaultEncoder: '',  // '' = pick the best available automatically
  exportPreset: null,  // null until the user configures it; see PRESET_DEFAULTS
  exportAskEveryTime: false,
};

let settings = { ...SETTINGS_DEFAULTS };

// Applies settings to the DOM. Everything here is idempotent so it can run on
// load, on every slider tick, and after a reset.
function applySettings() {
  refreshPresetUi();
  document.documentElement.style.setProperty('--font-scale', settings.fontScale / 100);
  document.documentElement.style.setProperty('--card-min', settings.cardMin + 'px');

  fontsizeSlider.value = settings.fontScale;
  fontsizeValue.textContent = settings.fontScale + '%';
  thumbtimeSlider.value = settings.thumbTime;
  thumbtimeValue.textContent = settings.thumbTime + 's';
  hoverpreviewSwitch.classList.toggle('on', settings.hoverPreview);
  presetaskSwitch.classList.toggle('on', settings.exportAskEveryTime);
  [...cardsizeSeg.children].forEach(b =>
    b.classList.toggle('active', parseInt(b.dataset.min) === settings.cardMin));
}

function persistSettings() {
  api.savePrefs({ settings });
}

async function openSettings() {
  settingsOverlay.classList.add('open');
  // The probe is lazy, so Settings may be the first thing that needs it.
  if (!encoderList) encoderList = await api.getEncoders();
  buildDefaultEncoderSelect();
}
function closeSettings() {
  settingsOverlay.classList.remove('open');
}

settingsBtn.addEventListener('click', openSettings);
settingsCloseBtn.addEventListener('click', closeSettings);
settingsDoneBtn.addEventListener('click', closeSettings);
settingsOverlay.addEventListener('click', e => { if (e.target === settingsOverlay) closeSettings(); });
document.addEventListener('keydown', e => {
  if (e.key === 'Escape' && settingsOverlay.classList.contains('open')) closeSettings();
});

fontsizeSlider.addEventListener('input', () => {
  settings.fontScale = parseInt(fontsizeSlider.value);
  applySettings();
  persistSettings();
});

cardsizeSeg.addEventListener('click', e => {
  const btn = e.target.closest('[data-min]');
  if (!btn) return;
  settings.cardMin = parseInt(btn.dataset.min);
  applySettings();
  persistSettings();
});

hoverpreviewRow.addEventListener('click', () => {
  settings.hoverPreview = !settings.hoverPreview;
  // Kill any preview already playing so toggling off takes effect immediately.
  if (!settings.hoverPreview) stopHoverPreview();
  applySettings();
  persistSettings();
});

thumbtimeSlider.addEventListener('input', () => {
  settings.thumbTime = parseInt(thumbtimeSlider.value);
  applySettings();
});
thumbtimeSlider.addEventListener('change', () => {
  // Re-grab every still only once the user lets go of the slider.
  thumbCache.clear();
  cardMap.forEach((card, fullPath) => {
    const img = card.querySelector('.vid-thumb-img');
    if (img) img.removeAttribute('src');
    requeueThumb(fullPath, card);
  });
  persistSettings();
});

function buildDefaultEncoderSelect() {
  const hw = encoderList?.hardware ?? [];
  const sw = encoderList?.software ?? [{ id: 'libx264', family: 'h264', vendor: 'CPU (x264)' }];
  defaultEncoderSel.innerHTML = '';

  const auto = document.createElement('option');
  auto.value = '';
  auto.textContent = hw.length ? 'Automatic — use GPU when available' : 'Automatic — CPU (no GPU encoder found)';
  defaultEncoderSel.appendChild(auto);

  [...hw, ...sw].forEach(e => {
    const o = document.createElement('option');
    o.value = e.id;
    o.textContent = `${e.vendor} · ${FAMILY_NAMES[e.family] || e.family}` + (isHardware(e.id) ? ' (fast)' : '');
    defaultEncoderSel.appendChild(o);
  });
  defaultEncoderSel.value = settings.defaultEncoder;
}

defaultEncoderSel.addEventListener('change', () => {
  settings.defaultEncoder = defaultEncoderSel.value;
  encoderTouched = false;   // let the new default take effect next time a modal opens
  persistSettings();
});

settingsResetBtn.addEventListener('click', () => {
  settings = { ...SETTINGS_DEFAULTS };
  applySettings();
  buildDefaultEncoderSelect();
  refreshExportLabels();
  encoderTouched = false;
  persistSettings();
  showToast('Settings reset to defaults', 'info');
});


// ── Export preset ─────────────────────────────────────────────────────────────
// One saved recipe, plus a card action that works out the shortest route from a given
// clip to it. The point is that a clip already matching the preset costs nothing — no
// re-encode, no generation loss, just the file handed over.
const presetOverlay       = document.getElementById('preset-overlay');
const presetSubject       = document.getElementById('preset-subject');
const presetNote          = document.getElementById('preset-note');
const presetCloseBtn      = document.getElementById('preset-close-btn');
const presetCancelBtn     = document.getElementById('preset-cancel');
const presetSaveBtn       = document.getElementById('preset-save');
const presetSummary       = document.getElementById('preset-summary');
const presetSettingsMount = document.getElementById('preset-settings-mount');
const presetModalMount    = document.getElementById('preset-modal-mount');
const presetaskRow        = document.getElementById('presetask-row');
const presetaskSwitch     = document.getElementById('presetask-switch');

const PRESET_DEFAULTS = {
  height: 1080,        // cap on the short side; null = keep whatever the clip has
  codec: 'h264',       // 'h264' | 'av1'
  range: 'sdr',        // 'sdr' = tone map HDR sources down | 'keep' = leave them HDR
  toneMap: 'balanced',
  crf: 23,
  saveMode: 'new',     // 'new' | 'replace'
};

const PRESET_HEIGHTS = [
  { v: null, label: 'Keep original' },
  { v: 1440, label: '1440p' },
  { v: 1080, label: '1080p' },
  { v: 720,  label: '720p' },
  { v: 480,  label: '480p' },
];

function presetOrDefaults() {
  return { ...PRESET_DEFAULTS, ...(settings.exportPreset || {}) };
}

function presetSummaryShort(preset) {
  const p = preset || presetOrDefaults();
  return [
    p.height ? p.height + 'p' : 'Source res',
    FAMILY_NAMES[p.codec] || p.codec,
    p.range === 'sdr' ? 'SDR' : 'Keep HDR',
  ].join(' · ');
}

// ── The control panel, mounted in both Settings and the export modal ──────────
// Building it rather than writing the markup twice keeps the two copies honestly
// identical, and lets the modal edit a draft while Settings edits the real thing.
function buildPresetUi(mount, draft, onChange) {
  mount.innerHTML = '';
  const refreshers = [];
  const commit = () => { onChange(); refreshAll(); };

  function segment(labelText, options, read, write, tip) {
    const wrap = document.createElement('div');
    wrap.className = 'enc-field';

    const lab = document.createElement('div');
    lab.className = 'enc-label';
    const labName = document.createElement('span');
    labName.textContent = labelText;
    lab.appendChild(labName);

    const seg = document.createElement('div');
    seg.className = 'enc-seg';
    options.forEach(o => {
      const b = document.createElement('button');
      b.textContent = o.label;
      b._presetValue = o.v;
      b.addEventListener('click', () => { write(o.v); commit(); });
      seg.appendChild(b);
    });

    const tipEl = document.createElement('div');
    tipEl.className = 'enc-tip';

    wrap.append(lab, seg, tipEl);
    mount.appendChild(wrap);
    refreshers.push(() => {
      [...seg.children].forEach(b => b.classList.toggle('active', b._presetValue === read()));
      tipEl.textContent = typeof tip === 'function' ? tip() : tip;
    });
    return wrap;
  }

  segment('Resolution', PRESET_HEIGHTS, () => draft.height, v => draft.height = v,
    () => draft.height
      ? 'Caps the short side at ' + draft.height + 'p. Clips already at or below it are left alone — nothing is ever upscaled.'
      : 'Keeps whatever resolution each clip already has.');

  segment('Codec', [{ v: 'h264', label: 'H.264' }, { v: 'av1', label: 'AV1' }],
    () => draft.codec, v => draft.codec = v,
    () => draft.codec === 'av1'
      ? 'Smallest files at matched quality. Recent players, browsers and phones handle AV1; older ones and some editors do not.'
      : 'Opens in literally everything — every editor, player, chat app and upload target. The safe choice for clips you are sending to people.');

  segment('Dynamic range', [{ v: 'sdr', label: 'Convert to SDR' }, { v: 'keep', label: 'Keep HDR' }],
    () => draft.range, v => draft.range = v,
    () => draft.range === 'sdr'
      ? 'HDR clips get tone mapped so they look right on ordinary screens. Clips that are already SDR are untouched by this.'
      : 'Leaves HDR clips as they are. They will still look grey and flat to anyone without an HDR display.');

  const toneWrap = segment('SDR look',
    [{ v: 'balanced', label: 'Balanced' }, { v: 'filmic', label: 'Filmic' }, { v: 'punchy', label: 'Punchy' }],
    () => draft.toneMap, v => draft.toneMap = v,
    () => TONEMAP_LOOK_TIPS[draft.toneMap]);

  // ── Quality ──
  const qWrap = document.createElement('div');
  qWrap.className = 'enc-field';

  const qLab = document.createElement('div');
  qLab.className = 'enc-label';
  const qLabName = document.createElement('span');
  qLabName.textContent = 'Quality';
  const qLabValue = document.createElement('span');
  qLabValue.className = 'enc-value';
  qLab.append(qLabName, qLabValue);

  const qSlider = document.createElement('input');
  qSlider.type = 'range';
  qSlider.className = 'enc-slider';
  qSlider.min = '16';
  qSlider.max = '34';
  qSlider.step = '1';
  qSlider.addEventListener('input', () => { draft.crf = parseInt(qSlider.value); commit(); });

  const qScale = document.createElement('div');
  qScale.className = 'slider-scale';
  const qScaleLo = document.createElement('span');
  qScaleLo.textContent = '16 — bigger, sharper';
  const qScaleHi = document.createElement('span');
  qScaleHi.textContent = '34 — smaller, blockier';
  qScale.append(qScaleLo, qScaleHi);

  const qTip = document.createElement('div');
  qTip.className = 'enc-tip';

  qWrap.append(qLab, qSlider, qScale, qTip);
  mount.appendChild(qWrap);
  refreshers.push(() => {
    qSlider.value = draft.crf;
    qLabValue.textContent = 'CRF ' + draft.crf + ' · ' + crfDescriptor(draft.crf);
    qTip.textContent = 'Only applies when a clip actually has to be re-encoded. ' + crfTipText(draft.crf, draft.codec);
  });

  segment('Output', [{ v: 'new', label: 'Save as a new file' }, { v: 'replace', label: 'Replace original' }],
    () => draft.saveMode, v => draft.saveMode = v,
    () => draft.saveMode === 'new'
      ? 'Writes clip_export.mp4 next to the original and leaves the original alone. A clip that already matches the preset is copied, not re-encoded.'
      : 'Overwrites the original in place. A clip that already matches the preset is left exactly as it is.');

  function refreshAll() {
    // The SDR look only means anything when HDR is actually being converted.
    toneWrap.style.display = draft.range === 'sdr' ? '' : 'none';
    refreshers.forEach(fn => fn());
  }
  refreshAll();
  return { refresh: refreshAll, draft };
}

let presetSettingsUi    = null;
let presetSettingsDraft = null;

function refreshPresetUi() {
  if (!presetSettingsUi) {
    // Settings edits the saved preset in place — there is nothing to cancel back to.
    presetSettingsDraft = presetOrDefaults();
    presetSettingsUi = buildPresetUi(presetSettingsMount, presetSettingsDraft, () => {
      settings.exportPreset = { ...presetSettingsDraft };
      persistSettings();
      refreshExportLabels();
    });
    return;
  }
  // Re-sync before redrawing: prefs loading, a Reset, or the export modal can all move
  // the preset out from under this panel, and a stale draft would write itself back
  // over the real one the next time anything here was clicked.
  Object.assign(presetSettingsDraft, presetOrDefaults());
  presetSettingsUi.refresh();
}

// ── Working out what a clip still needs ──────────────────────────────────────
function presetOutputDims(meta, height) {
  const w = meta && meta.width, h = meta && meta.height;
  if (!w || !h || !height) return { w, h };
  // Portrait clips are capped on their short side too, which is the width.
  if (w < h) return { w: height, h: Math.round((h / w) * height / 2) * 2 };
  return { w: Math.round((w / h) * height / 2) * 2, h: height };
}

function planExport(clip, meta, preset) {
  const ext   = (clip.name.match(/\.[^.]+$/) || [''])[0].toLowerCase();
  const short = meta && meta.width && meta.height ? Math.min(meta.width, meta.height) : null;

  const canToneMap = filterCaps ? filterCaps.zscale : true;
  const needResize = !!(preset.height && short && short > preset.height);
  const needCodec  = !!(meta && meta.videoCodec && meta.videoCodec !== preset.codec);
  const needTone   = preset.range === 'sdr' && !!(meta && meta.isHdr) && canToneMap;
  const needMp4    = ext !== '.mp4';

  // Everything that touches the picture collapses into one ffmpeg pass — chaining
  // separate encodes would just stack generation loss to arrive at the same frames.
  const encode = needResize || needCodec || needTone;
  const remux  = !encode && needMp4;

  const steps = [];
  if (needResize) {
    const out = presetOutputDims(meta, preset.height);
    steps.push('Scale ' + meta.width + '×' + meta.height + ' → ' + out.w + '×' + out.h);
  }
  if (needCodec) steps.push('Re-encode ' + codecLabel(meta.videoCodec) + ' → ' + FAMILY_NAMES[preset.codec]);
  if (needTone)  steps.push('Tone map HDR → SDR (' + TONEMAP_NAMES[preset.toneMap] + ')');
  if (remux)     steps.push('Repackage ' + ext + ' → .mp4, no re-encode');
  if (encode && needMp4) steps.push('Write it out as .mp4');

  // Worth saying out loud where we deliberately decline to do something.
  const skipped = [];
  if (preset.height && short && short <= preset.height) {
    skipped.push(short === preset.height
      ? 'Already ' + short + 'p'
      : 'Already ' + short + 'p — not upscaling to ' + preset.height + 'p');
  }
  if (meta && meta.videoCodec && meta.videoCodec === preset.codec) {
    skipped.push('Already ' + FAMILY_NAMES[preset.codec]);
  }
  if (preset.range === 'sdr' && meta && !meta.isHdr) skipped.push('Already SDR');
  if (preset.range === 'sdr' && meta && meta.isHdr && !canToneMap) {
    skipped.push('Cannot tone map — this ffmpeg build has no zscale filter');
  }

  return { encode, remux, steps, skipped, ext, needResize, needTone,
           nothingToDo: !encode && !remux };
}

function encoderForFamily(family) {
  const list = [...(encoderList?.hardware ?? []), ...(encoderList?.software ?? [])];
  const pick = list.find(e => e.id === settings.defaultEncoder && e.family === family)
    || list.find(e => e.family === family && isHardware(e.id))
    || list.find(e => e.family === family);
  return pick ? pick.id : (family === 'av1' ? 'libsvtav1' : 'libx264');
}

function exportOutputName(clip, plan) {
  const base = clip.name.replace(/\.[^.]+$/, '');
  return base + '_export' + (plan.encode || plan.remux ? '.mp4' : plan.ext);
}

// ── The export action ────────────────────────────────────────────────────────
let presetPending = null;   // { clip, meta, draft } while the modal is open

async function startExport(clip) {
  if (!encoderList) encoderList = await api.getEncoders();
  if (!filterCaps)  filterCaps  = await api.getFilters();

  let meta = metaCache.get(clip.fullPath) || null;
  if (!meta) {
    meta = await api.probeVideo(clip.fullPath);
    if (meta) { metaCache.set(clip.fullPath, meta); applyBadgesFor(clip.fullPath); }
  }

  const configured = !!settings.exportPreset;
  if (!configured || settings.exportAskEveryTime) {
    openPresetModal(clip, meta, !configured);
    return;
  }
  await runExport(clip, meta, presetOrDefaults());
}

function openPresetModal(clip, meta, firstTime) {
  const draft = presetOrDefaults();
  presetSubject.textContent = clip.name;
  presetSaveBtn.textContent = firstTime ? 'Save & export' : 'Export';

  presetNote.innerHTML = firstTime
    ? 'This is your first export, so set the recipe up once here. <strong>Export</strong> reuses it for every clip after this — doing only whatever that clip still needs — and you can change it any time under <strong>Settings → Export preset</strong>.'
    : 'Changes here save back to your preset. Turn off <strong>Ask before every export</strong> in Settings to skip this step.';
  presetNote.classList.add('show');

  buildPresetUi(presetModalMount, draft, () => renderPresetPlan(clip, meta, draft));
  presetPending = { clip, meta, draft };
  renderPresetPlan(clip, meta, draft);
  presetOverlay.classList.add('open');
}

function renderPresetPlan(clip, meta, draft) {
  const plan = planExport(clip, meta, draft);
  const items = [];

  if (plan.nothingToDo) {
    items.push(draft.saveMode === 'replace'
      ? '<li>Nothing to do — this clip already matches</li>'
      : '<li>Copy it as-is — the picture already matches</li>');
  }
  plan.steps.forEach(t => items.push('<li>' + t + '</li>'));
  plan.skipped.forEach(t => items.push('<li class="skip">' + t + '</li>'));

  const dest = draft.saveMode === 'replace'
    ? (plan.nothingToDo ? 'leaves the original as it is' : 'replaces the original')
    : exportOutputName(clip, plan);

  presetSummary.innerHTML =
    '<strong>' + presetSummaryShort(draft) + '</strong> · ' + dest +
    '<div class="preset-plan"><div class="preset-plan-head">For this clip</div><ul>' +
    items.join('') + '</ul></div>';
}

function closePresetModal() {
  presetOverlay.classList.remove('open');
  presetPending = null;
}

presetaskRow.addEventListener('click', () => {
  settings.exportAskEveryTime = !settings.exportAskEveryTime;
  applySettings();
  persistSettings();
});

presetCloseBtn.addEventListener('click', closePresetModal);
presetCancelBtn.addEventListener('click', closePresetModal);
presetOverlay.addEventListener('click', e => { if (e.target === presetOverlay) closePresetModal(); });
document.addEventListener('keydown', e => {
  if (e.key === 'Escape' && presetOverlay.classList.contains('open')) closePresetModal();
});

presetSaveBtn.addEventListener('click', async () => {
  if (!presetPending) return;
  const { clip, meta, draft } = presetPending;
  settings.exportPreset = { ...draft };
  persistSettings();
  refreshExportLabels();
  refreshPresetUi();   // pull what was just decided here through to the Settings copy
  closePresetModal();
  await runExport(clip, meta, draft);
});

async function runExport(clip, meta, preset) {
  const plan = planExport(clip, meta, preset);

  // Nothing to re-encode, nothing to re-wrap, nothing to copy: hand the file over.
  if (plan.nothingToDo && preset.saveMode === 'replace') {
    api.openInExplorer(clip.fullPath);
    showToast('✓ Already matches your preset — showing it in Explorer', 'success', 4000);
    return;
  }

  api.watchFolder('');
  progTitle.textContent = 'Exporting…';
  progSub.textContent = plan.encode ? 'Starting ffmpeg' : 'Copying';
  progBar.style.width = '0%';
  progBarWrap.classList.toggle('show', plan.encode);
  progCancelBtn.classList.toggle('show', plan.encode);
  progressOverlay.classList.add('show');

  const result = plan.encode
    ? await api.encodeVideo({
        inputPath: clip.fullPath,
        saveMode: preset.saveMode,
        suffix: '_export',
        container: 'mp4',
        encoder: encoderForFamily(preset.codec),
        speed: 5,
        rateMode: 'crf',
        crf: preset.crf,
        bitrateKbps: 4000,
        targetHeight: plan.needResize ? preset.height : null,
        targetFps: null,
        toneMap: plan.needTone ? preset.toneMap : null,
        audioMode: 'copy',
        audioKbps: 160,
      })
    : await api.passthrough({
        inputPath: clip.fullPath,
        saveMode: preset.saveMode,
        suffix: '_export',
        container: 'mp4',
        remux: plan.remux,
      });

  progressOverlay.classList.remove('show');
  progBarWrap.classList.remove('show');
  progCancelBtn.classList.remove('show');
  progTitle.textContent = 'Trimming…';

  if (result.cancelled) {
    startWatching(rootFolder);
    showToast('Export cancelled', 'info');
    return;
  }
  if (!result.success) {
    startWatching(rootFolder);
    showToast('Export failed: ' + (result.details || result.error), 'error', 6000);
    return;
  }

  api.openInExplorer(result.outputPath);

  const name = result.outputPath.split('\\').pop();
  const how = plan.encode
    ? fmtBytes(result.size) + ' · ' + Math.round((result.size / clip.size) * 100) + '% of original'
    : plan.remux ? 'repackaged as .mp4, nothing re-encoded'
                 : 'copied as-is, nothing re-encoded';
  showToast('✓ ' + name + ' — ' + how, 'success', 5000);
  await scanAndRender();
}

// The menu entry carries the preset, so you know what Export will do before clicking.
function exportMenuLabel() {
  return settings.exportPreset ? 'Export · ' + presetSummaryShort() : 'Set up export…';
}

function refreshExportLabels() {
  const label = exportMenuLabel();
  cardMap.forEach(card => {
    const el = card.querySelector('[data-action="export"] .export-label');
    if (el) el.textContent = label;
  });
}

applySettings();
