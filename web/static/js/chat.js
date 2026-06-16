// ---------------------------------------------------------------------------
// Auto-scroll helper — returns true only when user is near the bottom
// ---------------------------------------------------------------------------
function shouldAutoScroll(container, threshold = 150) {
  return container.scrollHeight - container.scrollTop - container.clientHeight < threshold;
}

let activeRoundRatings = {};
let compactMode = 'compact';

function setActiveRoundRatings(roundRatings) {
  activeRoundRatings = roundRatings || {};
  globalThis.ACTIVE_ROUND_RATINGS = activeRoundRatings;
}

function getRoundRating(roundId) {
  return activeRoundRatings[String(roundId)] || null;
}

function escapeChatAttr(str) {
  return escapeHtml(String(str)).replace(/"/g, '&quot;').replace(/'/g, '&#39;');
}

function escapeJsSingleQuoted(str) {
  return String(str)
    .replace(/\\/g, '\\\\')
    .replace(/'/g, "\\'")
    .replace(/\n/g, '\\n')
    .replace(/\r/g, '\\r');
}

function roundRatingButtonClass(buttonRating, activeRating) {
  const active = buttonRating === activeRating;
  const activeClass = buttonRating === 'thumbs_up' ? 'text-green-400' : 'text-red-400';
  const hoverClass = buttonRating === 'thumbs_up' ? 'hover:text-green-400' : 'hover:text-red-400';
  return 'round-rating-button p-0.5 text-sm leading-none transition-colors duration-150 ' + (active ? activeClass : 'text-slate-500 ' + hoverClass);
}

function renderRoundRatingButtons(sessionId, roundId) {
  const activeRating = getRoundRating(roundId);
  const sessionArg = escapeJsSingleQuoted(sessionId);
  const roundKey = String(roundId);
  const roundArg = escapeJsSingleQuoted(roundKey);
  const sharedAttrs = ' data-round-rating-session="' + escapeChatAttr(sessionId) + '"'
    + ' data-round-rating-event="' + escapeChatAttr(roundKey) + '"';
  return '<button type="button" data-round-rating="thumbs_up"' + sharedAttrs
    + ' aria-pressed="' + String(activeRating === 'thumbs_up') + '"'
    + ' onclick="rateRound(\'' + sessionArg + '\', \'' + roundArg + '\', \'thumbs_up\')"'
    + ' class="' + roundRatingButtonClass('thumbs_up', activeRating) + '" title="Thumbs up">'
    + '<svg class="w-3.5 h-3.5" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M6.633 10.5c.806 0 1.533-.446 2.031-1.08a9.041 9.041 0 0 1 2.861-2.4c.723-.384 1.35-.956 1.653-1.715a4.498 4.498 0 0 0 .322-1.672V2.75A.75.75 0 0 1 14.25 2a2.25 2.25 0 0 1 2.25 2.25c0 1.152-.26 2.243-.723 3.218-.266.558.107 1.282.725 1.282h3.126c1.026 0 1.945.694 2.054 1.715.045.422.068.85.068 1.285a11.95 11.95 0 0 1-2.649 7.521c-.388.482-.987.729-1.605.729H13.48c-.483 0-.964-.078-1.423-.23l-3.114-1.04a4.501 4.501 0 0 0-1.423-.23H5.904M14.25 9h2.25M5.904 18.75c.083.205.173.405.27.602.197.4-.078.898-.523.898h-.908c-.889 0-1.713-.518-1.972-1.368a12 12 0 0 1-.521-3.507c0-1.553.295-3.036.831-4.398C3.387 10.203 4.167 9.75 5 9.75h1.053c.472 0 .745.556.5.96a8.958 8.958 0 0 0-1.302 4.665c0 1.194.232 2.333.654 3.375Z"/></svg></button>'
    + '<button type="button" data-round-rating="thumbs_down"' + sharedAttrs
    + ' aria-pressed="' + String(activeRating === 'thumbs_down') + '"'
    + ' onclick="rateRound(\'' + sessionArg + '\', \'' + roundArg + '\', \'thumbs_down\')"'
    + ' class="' + roundRatingButtonClass('thumbs_down', activeRating) + '" title="Thumbs down">'
    + '<svg class="w-3.5 h-3.5" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M7.5 15h2.25m8.024-9.75c.011.05.028.1.052.148.591 1.2.924 2.55.924 3.977a8.96 8.96 0 0 1-.999 4.125m.023-8.25c-.076-.365.183-.75.575-.75h.908c.889 0 1.713.518 1.972 1.368.339 1.11.521 2.287.521 3.507 0 1.553-.295 3.036-.831 4.398C20.613 14.547 19.833 15 19 15h-1.053c-.472 0-.745-.556-.5-.96a8.95 8.95 0 0 0 .303-.54m.023-8.25H16.48a4.5 4.5 0 0 1-1.423-.23l-3.114-1.04a4.5 4.5 0 0 0-1.423-.23H6.504c-.618 0-1.217.247-1.605.729A11.95 11.95 0 0 0 2.25 12c0 .434.023.863.068 1.285C2.427 14.306 3.346 15 4.372 15h3.126c.618 0 .991.724.725 1.282A7.471 7.471 0 0 0 7.5 19.5a2.25 2.25 0 0 0 2.25 2.25.75.75 0 0 0 .75-.75v-.633c0-.573.11-1.14.322-1.672.304-.76.93-1.33 1.653-1.715a9.04 9.04 0 0 0 2.86-2.4c.498-.634 1.226-1.08 2.032-1.08h.384"/></svg></button>';
}

function applyRoundRatingButtonState(btn, activeRating) {
  const buttonRating = btn.dataset.roundRating;
  const isActive = buttonRating === activeRating;
  btn.classList.toggle('text-slate-500', !isActive);
  btn.classList.toggle('text-green-400', buttonRating === 'thumbs_up' && isActive);
  btn.classList.toggle('text-red-400', buttonRating === 'thumbs_down' && isActive);
  btn.classList.toggle('hover:text-green-400', buttonRating === 'thumbs_up' && !isActive);
  btn.classList.toggle('hover:text-red-400', buttonRating === 'thumbs_down' && !isActive);
  btn.setAttribute('aria-pressed', String(isActive));
}

function updateRoundRatingButtons(sessionId, roundId, activeRating) {
  const roundKey = String(roundId);
  document.querySelectorAll('[data-round-rating]').forEach(btn => {
    if (btn.dataset.roundRatingSession === String(sessionId) && btn.dataset.roundRatingEvent === roundKey) {
      applyRoundRatingButtonState(btn, activeRating);
    }
  });
}

function refreshAllRoundRatingButtons() {
  document.querySelectorAll('[data-round-rating]').forEach(btn => {
    applyRoundRatingButtonState(btn, getRoundRating(btn.dataset.roundRatingEvent));
  });
}

function messageRenderId(msg) {
  if (!msg || msg.id == null || msg.id === '') return '';
  return String(msg.id);
}

function messageIdentityAttrs(msg) {
  const id = messageRenderId(msg);
  let attrs = id ? ' data-message-id="' + escapeChatAttr(id) + '"' : '';
  if (msg && msg.role) attrs += ' data-message-role="' + escapeChatAttr(msg.role) + '"';
  return attrs;
}

function isRenderedMessage(msg) {
  const id = messageRenderId(msg);
  if (!id) return false;
  return document.querySelector('[data-message-id="' + CSS.escape(id) + '"]') !== null;
}

async function initializeRoundRatings() {
  if (!SESSION_ID) return;
  const sessionId = SESSION_ID;
  try {
    const res = await fetch('/api/sessions/' + sessionId);
    if (!res.ok) throw new Error(`Load round ratings failed: ${res.status}`);
    const session = await res.json();
    if (SESSION_ID !== sessionId) return;
    setActiveRoundRatings(session.round_ratings || {});
    refreshAllRoundRatingButtons();
  } catch (err) {
    console.error('Load round ratings failed:', err);
  }
}

async function rateRound(sessionId, roundId, rating) {
  const roundKey = String(roundId);
  const nextRating = activeRoundRatings[roundKey] === rating ? null : rating;
  try {
    const res = await fetch('/api/sessions/' + sessionId + '/rounds/' + encodeURIComponent(roundKey) + '/rate', {
      method: 'POST',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({rating: nextRating}),
    });
    if (!res.ok) throw new Error(`Rate round failed: ${res.status}`);
    const session = await res.json();
    if (SESSION_ID !== sessionId) return;
    setActiveRoundRatings(session.round_ratings || {});
    updateRoundRatingButtons(sessionId, roundKey, getRoundRating(roundKey));
  } catch (err) {
    console.error('Rate round failed:', err);
  }
}

// ---------------------------------------------------------------------------
// In-session recap: zero-token extraction + opt-in cached Haiku summary
// ---------------------------------------------------------------------------
const RECAP_ASK_CAP = 6;

function toggleRecapPanel(btn, sessionId, eventIndex) {
  const sep = btn.closest('.separator-line');
  if (!sep) return;
  const next = sep.nextElementSibling;
  if (next && next.classList.contains('recap-panel')) {
    next.remove();
    btn.classList.remove('text-sky-400');
    return;
  }
  btn.classList.add('text-sky-400');
  const panel = document.createElement('div');
  panel.className = 'recap-panel mx-4 my-1 px-3 py-2 bg-slate-800/70 border border-slate-700/60 rounded-lg';
  panel.dataset.sessionId = sessionId;
  panel.dataset.eventIndex = eventIndex;
  panel.innerHTML = '<div class="recap-body text-slate-500 text-xs">Loading recap…</div>';
  sep.parentNode.insertBefore(panel, sep.nextSibling);
  loadRecap(sessionId, eventIndex, panel);
}

async function loadRecap(sessionId, eventIndex, panel) {
  const body = panel.querySelector('.recap-body');
  let data;
  try {
    const res = await fetch('/api/sessions/' + sessionId + '/recap?upto=' + eventIndex);
    if (!res.ok) throw new Error('HTTP ' + res.status);
    data = await res.json();
  } catch (err) {
    console.error('Load recap failed:', err);
    body.innerHTML = '<div class="text-red-400 text-xs">Failed to load recap</div>';
    return;
  }
  body.classList.remove('text-slate-500', 'text-xs');
  body.innerHTML =
    recapSectionLabel('What was discussed')
    + renderRecapAsks(data.asks)
    + '<div class="recap-summary mt-2 pt-2 border-t border-slate-700/60"></div>';
  applyRecapSummary(panel, sessionId, eventIndex, data);
}

function recapSectionLabel(text) {
  return '<div class="text-[11px] uppercase tracking-wide text-slate-500 mb-1">' + escapeHtml(text) + '</div>';
}

function renderRecapAsks(asks) {
  if (!asks || !asks.length) return '<div class="text-slate-500 text-xs">(none)</div>';
  const items = asks.map((ask, i) =>
    '<li class="' + (i >= RECAP_ASK_CAP ? 'recap-ask-extra hidden' : '') + '">' + escapeHtml(ask) + '</li>'
  ).join('');
  let html = '<ul class="list-disc pl-5 space-y-0.5 text-xs text-slate-300">' + items + '</ul>';
  if (asks.length > RECAP_ASK_CAP) {
    html += '<button class="mt-1 text-[11px] text-sky-400 hover:text-sky-300" onclick="toggleRecapAsks(this)">'
      + 'Show all (' + asks.length + ')</button>';
  }
  return html;
}

function toggleRecapAsks(btn) {
  const panel = btn.closest('.recap-panel');
  const extras = panel.querySelectorAll('.recap-ask-extra');
  const collapsed = extras.length && extras[0].classList.contains('hidden');
  extras.forEach((el) => el.classList.toggle('hidden', !collapsed));
  btn.textContent = collapsed ? 'Collapse' : 'Show all (' + (RECAP_ASK_CAP + extras.length) + ')';
}

function applyRecapSummary(panel, sessionId, eventIndex, data) {
  const sumEl = panel.querySelector('.recap-summary');
  if (data.summary && !data.summary_stale) {
    sumEl.innerHTML = recapSectionLabel('Summary') + recapSummaryText(data.summary);
    return;
  }
  if (data.summary && data.summary_stale) {
    sumEl.innerHTML = recapSectionLabel('Summary (stale)') + recapSummaryText(data.summary) + recapRerunButton();
    return;
  }
  // No summary yet for any point up to here -> the explicit recap-button click generates one.
  fetchRecapSummary(sessionId, eventIndex, panel);
}

function recapSummaryText(text) {
  return '<div class="text-xs text-slate-300 whitespace-pre-wrap">' + escapeHtml(text || '') + '</div>';
}

function recapRerunButton() {
  return '<button class="mt-1 text-[11px] text-sky-400 hover:text-sky-300" onclick="rerunRecapSummary(this)">↻ Re-summarize</button>';
}

function rerunRecapSummary(btn) {
  const panel = btn.closest('.recap-panel');
  fetchRecapSummary(panel.dataset.sessionId, panel.dataset.eventIndex, panel);
}

async function fetchRecapSummary(sessionId, eventIndex, panel) {
  const sumEl = panel.querySelector('.recap-summary');
  sumEl.innerHTML = recapSectionLabel('Summary') + '<div class="text-slate-500 text-xs">Summarizing…</div>';
  try {
    const res = await fetch('/api/sessions/' + sessionId + '/recap/summarize?upto=' + eventIndex, {method: 'POST'});
    if (!res.ok) throw new Error('HTTP ' + res.status);
    const data = await res.json();
    sumEl.innerHTML = recapSectionLabel('Summary') + recapSummaryText(data.summary);
  } catch (err) {
    console.error('Summarize recap failed:', err);
    sumEl.innerHTML = recapSectionLabel('Summary') + '<div class="text-red-400 text-xs">Failed to summarize</div>' + recapRerunButton();
  }
}

// ---------------------------------------------------------------------------
// Send message
// ---------------------------------------------------------------------------
function bumpCurrentSessionToTop() {
  const nav = document.getElementById('session-list');
  const el = document.getElementById('session-' + SESSION_ID);
  if (!nav || !el) return;

  const groupItems = el.closest('.session-group-items');
  const parent = groupItems || nav;
  if (parent.firstElementChild !== el) {
    parent.insertBefore(el, parent.firstElementChild);
  }

  const timeEl = el.querySelector('.session-time');
  if (timeEl) {
    const now = new Date().toISOString();
    timeEl.dataset.time = now;
    timeEl.textContent = relativeTime(now);
  }
}

function formatBubbleTime(isoStr) {
  if (!isoStr) return '';
  const d = new Date(isoStr);
  return d.toLocaleString(undefined, {
    month: 'short', day: 'numeric',
    hour: 'numeric', minute: '2-digit', second: '2-digit', hour12: true, timeZoneName: 'short'
  });
}

function getUploadedFileDisplayName(file) {
  if (file && file.filename) return file.filename;
  const path = file && file.path ? String(file.path) : '';
  const normalized = path.replaceAll('\\', '/').replace(/\/+$/, '');
  const parts = normalized.split('/');
  return parts[parts.length - 1] || path;
}

function normalizeUploadedFiles(uploadedFiles) {
  if (!Array.isArray(uploadedFiles) || !uploadedFiles.length) return [];
  return uploadedFiles.map((file) => {
    if (typeof file === 'string') {
      return {filename: getUploadedFileDisplayName({path: file}), path: file};
    }
    return {
      filename: getUploadedFileDisplayName(file),
      path: file.path || '',
      size: file.size,
    };
  });
}

function splitLegacyUploadedFiles(content) {
  const text = typeof content === 'string' ? content : '';
  const marker = '\n\n[Attached files]\n';
  const markerIndex = text.lastIndexOf(marker);
  if (markerIndex === -1) return {content: text, uploadedFiles: []};

  const attachmentBlock = text.slice(markerIndex + marker.length);
  const lines = attachmentBlock.split('\n').filter((line) => line.length > 0);
  if (!lines.length || lines.some((line) => !line.startsWith('- ') || !line.slice(2).trim())) {
    return {content: text, uploadedFiles: []};
  }

  return {
    content: text.slice(0, markerIndex),
    uploadedFiles: lines.map((line) => {
      const path = line.slice(2).trim();
      return {filename: getUploadedFileDisplayName({path}), path};
    }),
  };
}

function normalizeUserMessage(content, uploadedFiles) {
  const normalizedFiles = normalizeUploadedFiles(uploadedFiles);
  if (normalizedFiles.length) {
    return {content: typeof content === 'string' ? content : '', uploadedFiles: normalizedFiles};
  }
  return splitLegacyUploadedFiles(content);
}

function renderUserAttachments(uploadedFiles, withTopMargin) {
  const files = normalizeUploadedFiles(uploadedFiles);
  if (!files.length) return '';
  return `<div class="message-attachments${withTopMargin ? ' mt-2' : ''}">`
    + files.map((file) => {
      const title = file.path ? ` title="${escapeHtml(file.path)}"` : '';
      return `<div class="message-attachment"${title}>`
        + '<svg class="w-3.5 h-3.5 flex-shrink-0" fill="none" stroke="currentColor" viewBox="0 0 24 24">'
        + '<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M15.172 7l-6.586 6.586a2 2 0 102.828 2.828l6.414-6.586a4 4 0 00-5.656-5.656l-6.415 6.585a6 6 0 108.486 8.486L20.5 13"/></svg>'
        + `<span>${escapeHtml(file.filename)}</span></div>`;
    }).join('')
    + '</div>';
}

function renderUserMessageBubble(content, isVoice, timestamp, uploadedFiles) {
  const normalized = normalizeUserMessage(content, uploadedFiles);
  const timeHtml = timestamp ? '<div class="text-[10px] text-slate-400/60 mt-1">' + formatBubbleTime(timestamp) + '</div>' : '';
  const textHtml = normalized.content
    ? `<div class="whitespace-pre-wrap">${escapeHtml(normalized.content)}</div>`
    : '';
  const attachmentsHtml = renderUserAttachments(normalized.uploadedFiles, Boolean(textHtml));
  return `<div class="max-w-[75%] overflow-hidden bg-blue-600 rounded-2xl rounded-br-md px-4 py-2.5 text-sm">`
    + (isVoice ? '<span class="text-xs text-blue-200 block mb-1">&#127908; Voice</span>' : '')
    + textHtml
    + attachmentsHtml
    + timeHtml
    + '</div>';
}

function toolInputSummary(tool) {
  var input = tool.input || {};
  if (tool.name === 'Bash') return {text: input.command || '', limit: 80};
  if (tool.name === 'Read' || tool.name === 'Edit' || tool.name === 'Write') return {text: input.file_path || '', limit: 0};
  if (tool.name === 'Glob') return {text: input.pattern || '', limit: 0};
  if (tool.name === 'Grep') return {text: (input.pattern || '') + (input.path ? ' in ' + input.path : ''), limit: 0};
  var first = Object.values(input)[0];
  if (first == null || first === '') return {text: '', limit: 0};
  var display = typeof first === 'object' ? JSON.stringify(first) : String(first);
  return {text: display, limit: 60};
}

// ---------------------------------------------------------------------------
// HTML artifact rendering — inline sandboxed iframes for linked artifacts/*.html.
// ---------------------------------------------------------------------------
var HTML_ARTIFACT_LINK_RE = /(^|\/)artifacts\/[^/]+\.html$/;
var htmlArtifactFetchCache = new Map();

function basename(path) {
  var parts = String(path || '').split('/');
  return parts[parts.length - 1];
}

function resolveArtifactAbsolutePath(filePath) {
  if (filePath.charAt(0) === '/') return filePath;
  var home = (typeof window !== 'undefined' && window.USER_HOME) ? window.USER_HOME : '';
  if (!home) {
    throw new Error('window.USER_HOME not injected');
  }
  return home + '/.charliebot/sessions/' + SESSION_ID + '/' + filePath;
}

function escapeForSrcdoc(html) {
  return String(html || '').replace(/&/g, '&amp;').replace(/"/g, '&quot;');
}

function injectResizeScript(html, frameId) {
  var script = '<script>(function(){'
    + 'var frameId=' + JSON.stringify(frameId) + ';'
    + 'function send(){parent.postMessage({type:"html-artifact-height",id:frameId,height:document.documentElement.scrollHeight},"*");}'
    + 'new ResizeObserver(send).observe(document.documentElement);'
    + 'window.addEventListener("load",send);'
    + 'send();'
    + '})();<\/script>';
  var src = String(html || '');
  var idx = src.lastIndexOf('</body>');
  if (idx === -1) return src + script;
  return src.slice(0, idx) + script + src.slice(idx);
}

function htmlArtifactSizeStorageKey(filePath) {
  return filePath ? 'html-artifact-size:' + filePath : '';
}

function loadHtmlArtifactSavedSize(filePath) {
  var key = htmlArtifactSizeStorageKey(filePath);
  if (!key) return null;
  try {
    var raw = localStorage.getItem(key);
    if (!raw) return null;
    var parsed = JSON.parse(raw);
    if (!parsed || typeof parsed !== 'object') return null;
    var height = (typeof parsed.height === 'number' && parsed.height > 0) ? parsed.height : 0;
    var width = (typeof parsed.width === 'number' && parsed.width > 0) ? parsed.width : 0;
    if (height || width) return {height: height, width: width};
  } catch (e) {}
  return null;
}

function saveHtmlArtifactSavedSize(filePath, size) {
  var key = htmlArtifactSizeStorageKey(filePath);
  if (!key) return;
  try {
    localStorage.setItem(key, JSON.stringify(size));
  } catch (e) {}
}

function buildHtmlArtifactCard(opts) {
  var filePath = opts.filePath || opts.absPath || '';
  var absPath = opts.absPath || resolveArtifactAbsolutePath(filePath);
  var rawHtml = opts.html || '';
  var frameId = 'hf-' + Math.random().toString(36).slice(2);
  var withScript = injectResizeScript(rawHtml, frameId);
  var srcdoc = escapeForSrcdoc(withScript);
  var openUrl = '/files' + absPath;
  var sourceHighlighted = hljs.highlight(rawHtml, {language: 'xml'}).value;
  var savedSize = loadHtmlArtifactSavedSize(filePath);
  var iframeSizeStyle = 'min-height:60px;max-height:80vh;';
  var widthStyle = '';
  if (savedSize) {
    if (savedSize.height) iframeSizeStyle = 'height:' + savedSize.height + 'px;max-height:none;';
    if (savedSize.width) widthStyle = 'width:' + savedSize.width + 'px;';
  }
  var iframeStyle = 'width:100%;' + iframeSizeStyle
    + 'border:1px solid rgba(148,163,184,0.2);border-bottom:none;border-right:none;border-radius:0;'
    + 'background:white;display:block;';
  var manualHeightAttr = (savedSize && savedSize.height) ? ' data-manual-height="1"' : '';
  var filePathAttr = ' data-file-path="' + escapeHtml(filePath) + '"';
  return '<div class="html-artifact" data-artifact-path="' + escapeHtml(absPath) + '">'
    + '<div class="html-artifact-toolbar" style="' + widthStyle + '">'
    + '<span class="filename">' + escapeHtml(basename(filePath)) + '</span>'
    + '<button type="button" onclick="expandHtmlArtifact(this)">Expand</button>'
    + '<a href="' + escapeHtml(openUrl) + '" target="_blank" rel="noopener noreferrer">Open in tab</a>'
    + '<button type="button" onclick="toggleHtmlArtifactSource(this)">View source</button>'
    + '</div>'
    + '<div class="html-artifact-frame-wrap" style="' + widthStyle + '">'
    + '<iframe class="html-artifact-frame" data-frame-id="' + frameId + '"' + manualHeightAttr + filePathAttr
    + ' sandbox="allow-scripts allow-popups allow-popups-to-escape-sandbox"'
    + ' srcdoc="' + srcdoc + '"'
    + ' style="' + iframeStyle + '"></iframe>'
    + '<div class="html-artifact-resize-handle html-artifact-resize-s" title="Drag to resize height"'
    + ' onpointerdown="startHtmlArtifactResize(event, this, \'y\')"></div>'
    + '<div class="html-artifact-resize-handle html-artifact-resize-e" title="Drag to resize width"'
    + ' onpointerdown="startHtmlArtifactResize(event, this, \'x\')"></div>'
    + '<div class="html-artifact-resize-handle html-artifact-resize-se" title="Drag to resize"'
    + ' onpointerdown="startHtmlArtifactResize(event, this, \'xy\')"></div>'
    + '</div>'
    + '<div class="html-artifact-source"><pre><code class="hljs language-html">'
    + sourceHighlighted + '</code></pre></div>'
    + '</div>';
}

function resolveHtmlArtifactLink(source) {
  var href = '';
  if (typeof source === 'string') {
    href = source;
  } else if (source && source.getAttribute) {
    href = source.getAttribute('href') || '';
  }
  if (!href) return null;
  var url;
  try {
    url = new URL(href, window.location.href);
  } catch (e) {
    console.warn('Invalid HTML artifact link', href, e);
    return null;
  }
  var pathname = url.pathname || '';
  if (!HTML_ARTIFACT_LINK_RE.test(pathname)) return null;
  if (pathname.indexOf('/files/') !== 0) return null;
  var encodedAbsPath = pathname.slice('/files/'.length);
  var absPath;
  try {
    absPath = '/' + decodeURIComponent(encodedAbsPath);
  } catch (e) {
    console.warn('Invalid HTML artifact file path', pathname, e);
    return null;
  }
  return {fetchUrl: pathname, absPath: absPath};
}

var HTML_ARTIFACT_CODE_RE = /(?:https?:\/\/[^\s]+?)?\/files\/[^\s]*\/artifacts\/[^/\s]+\.html/g;

function findArtifactLinkInCode(code) {
  var text = code.textContent || '';
  if (!text) return null;
  var resolved = resolveHtmlArtifactLink(text);
  if (resolved) return resolved;
  var matches = text.match(HTML_ARTIFACT_CODE_RE);
  if (!matches) return null;
  for (var i = 0; i < matches.length; i++) {
    resolved = resolveHtmlArtifactLink(matches[i]);
    if (resolved) return resolved;
  }
  return null;
}

function fetchHtmlArtifact(absPath, fetchUrl) {
  if (!htmlArtifactFetchCache.has(absPath)) {
    var promise = fetch(fetchUrl).then(function(resp) {
      if (!resp.ok) {
        throw new Error('HTTP ' + resp.status + ' fetching ' + fetchUrl);
      }
      return resp.text();
    }).catch(function(e) {
      htmlArtifactFetchCache.delete(absPath);
      throw e;
    });
    htmlArtifactFetchCache.set(absPath, promise);
  }
  return htmlArtifactFetchCache.get(absPath);
}

function findEmbeddedHtmlArtifactCard(prose, absPath) {
  var el = prose.nextElementSibling;
  while (el) {
    if (el.classList.contains('html-artifact') && el.dataset.artifactPath === absPath) {
      return el;
    }
    el = el.nextElementSibling;
  }
  return null;
}

function insertHtmlArtifactCard(prose, card, ordinal) {
  var parent = prose.parentNode;
  if (!parent) return;
  card.dataset.artifactOrdinal = String(ordinal);
  var before = prose.nextSibling;
  while (before && before.nodeType === Node.ELEMENT_NODE && before.classList.contains('html-artifact')) {
    var existingOrdinal = Number(before.dataset.artifactOrdinal || 0);
    if (existingOrdinal > ordinal) break;
    before = before.nextSibling;
  }
  parent.insertBefore(card, before);
}

function embedLinkedHtmlArtifacts(root) {
  root.querySelectorAll('.prose-msg').forEach(function(prose) {
    if (prose.id === 'streaming-msg' || prose.closest('#streaming-msg')) return;
    var links = [];
    var seen = new Set();
    Array.from(prose.querySelectorAll('a[href]')).forEach(function(anchor) {
      if (anchor.dataset.embedded === '1') return;
      var resolved = resolveHtmlArtifactLink(anchor);
      if (!resolved) return;
      if (seen.has(resolved.absPath)) return;
      seen.add(resolved.absPath);
      links.push({
        el: anchor,
        absPath: resolved.absPath,
        fetchUrl: resolved.fetchUrl,
      });
    });
    Array.from(prose.querySelectorAll('code')).forEach(function(code) {
      if (code.dataset.embedded === '1') return;
      var resolved = findArtifactLinkInCode(code);
      if (!resolved) return;
      if (seen.has(resolved.absPath)) return;
      seen.add(resolved.absPath);
      links.push({
        el: code,
        absPath: resolved.absPath,
        fetchUrl: resolved.fetchUrl,
      });
    });
    links.forEach(function(link, ordinal) {
      if (findEmbeddedHtmlArtifactCard(prose, link.absPath)) {
        link.el.dataset.embedded = '1';
        return;
      }
      fetchHtmlArtifact(link.absPath, link.fetchUrl).then(function(html) {
        if (!link.el.isConnected) return;
        if (link.el.dataset.embedded === '1') return;
        var currentProse = link.el.closest('.prose-msg');
        if (!currentProse || currentProse.closest('#streaming-msg')) return;
        if (findEmbeddedHtmlArtifactCard(currentProse, link.absPath)) {
          link.el.dataset.embedded = '1';
          return;
        }
        var template = document.createElement('template');
        template.innerHTML = buildHtmlArtifactCard({
          absPath: link.absPath,
          filePath: link.absPath,
          html: html,
        });
        var card = template.content.firstElementChild;
        if (!card) {
          throw new Error('HTML artifact card render produced no element');
        }
        insertHtmlArtifactCard(currentProse, card, ordinal);
        link.el.dataset.embedded = '1';
      }).catch(function(e) {
        console.warn('Failed to render linked HTML artifact', link.fetchUrl, e);
      });
    });
  });
}

function toggleHtmlArtifactSource(btn) {
  var card = btn.closest('.html-artifact');
  if (!card) return;
  var wrap = card.querySelector('.html-artifact-frame-wrap');
  var source = card.querySelector('.html-artifact-source');
  if (!wrap || !source) return;
  var showingSource = source.style.display === 'block';
  if (showingSource) {
    source.style.display = 'none';
    wrap.style.display = '';
    btn.textContent = 'View source';
  } else {
    wrap.style.display = 'none';
    source.style.display = 'block';
    btn.textContent = 'View rendered';
  }
}

function startHtmlArtifactResize(e, handle, axis) {
  e.preventDefault();
  var card = handle.closest('.html-artifact');
  if (!card) return;
  var wrap = card.querySelector('.html-artifact-frame-wrap');
  var frame = card.querySelector('.html-artifact-frame');
  if (!wrap || !frame) return;
  var resizeY = axis.indexOf('y') !== -1;
  var resizeX = axis.indexOf('x') !== -1;
  var startX = e.clientX;
  var startY = e.clientY;
  var startHeight = frame.getBoundingClientRect().height;
  var startWidth = wrap.getBoundingClientRect().width;
  // Block the iframe from absorbing pointer events while dragging — without
  // this, pointermove stops firing the moment the cursor enters the iframe.
  frame.style.pointerEvents = 'none';
  var prevBodyCursor = document.body.style.cursor;
  document.body.style.cursor = resizeX && resizeY ? 'nwse-resize'
    : resizeX ? 'ew-resize' : 'ns-resize';
  function onMove(ev) {
    if (resizeY) {
      var newHeight = Math.max(60, startHeight + (ev.clientY - startY));
      frame.style.height = newHeight + 'px';
      frame.style.maxHeight = 'none';
    }
    if (resizeX) {
      var newWidth = Math.max(120, startWidth + (ev.clientX - startX));
      wrap.style.width = newWidth + 'px';
      var toolbar = card.querySelector('.html-artifact-toolbar');
      if (toolbar) toolbar.style.width = newWidth + 'px';
    }
  }
  function onUp() {
    document.removeEventListener('pointermove', onMove);
    document.removeEventListener('pointerup', onUp);
    document.removeEventListener('pointercancel', onUp);
    frame.style.pointerEvents = '';
    document.body.style.cursor = prevBodyCursor;
    if (resizeY) frame.dataset.manualHeight = '1';
    if (!frame.dataset.filePath) return;
    var existing = loadHtmlArtifactSavedSize(frame.dataset.filePath) || {height: 0, width: 0};
    var newSize = {height: existing.height, width: existing.width};
    if (resizeY) newSize.height = frame.getBoundingClientRect().height;
    if (resizeX) newSize.width = wrap.getBoundingClientRect().width;
    saveHtmlArtifactSavedSize(frame.dataset.filePath, newSize);
  }
  document.addEventListener('pointermove', onMove);
  document.addEventListener('pointerup', onUp);
  document.addEventListener('pointercancel', onUp);
}

function expandHtmlArtifact(btn) {
  var card = btn.closest('.html-artifact');
  if (!card) return;
  var inlineFrame = card.querySelector('.html-artifact-frame');
  if (!inlineFrame) return;
  var srcdoc = inlineFrame.getAttribute('srcdoc') || '';
  var sandbox = inlineFrame.getAttribute('sandbox') || '';
  var filenameEl = card.querySelector('.html-artifact-toolbar .filename');
  var filenameText = filenameEl ? filenameEl.textContent : '';

  var overlay = document.createElement('div');
  overlay.className = 'html-artifact-modal-overlay';

  var content = document.createElement('div');
  content.className = 'html-artifact-modal-content';
  content.addEventListener('click', function (e) { e.stopPropagation(); });

  var toolbar = document.createElement('div');
  toolbar.className = 'html-artifact-modal-toolbar';

  var filenameSpan = document.createElement('span');
  filenameSpan.className = 'filename';
  filenameSpan.textContent = filenameText;
  toolbar.appendChild(filenameSpan);

  var closeBtn = document.createElement('button');
  closeBtn.type = 'button';
  closeBtn.textContent = 'Close';
  toolbar.appendChild(closeBtn);

  var modalFrame = document.createElement('iframe');
  modalFrame.setAttribute('sandbox', sandbox);
  modalFrame.setAttribute('srcdoc', srcdoc);

  content.appendChild(toolbar);
  content.appendChild(modalFrame);
  overlay.appendChild(content);

  function onKeydown(e) {
    if (e.key === 'Escape') close();
  }
  function close() {
    document.removeEventListener('keydown', onKeydown);
    if (overlay.parentNode) overlay.parentNode.removeChild(overlay);
  }
  closeBtn.addEventListener('click', close);
  overlay.addEventListener('click', close);
  document.addEventListener('keydown', onKeydown);

  document.body.appendChild(overlay);
}

function installHtmlArtifactListener() {
  if (window.__htmlArtifactListenerInstalled) return;
  window.__htmlArtifactListenerInstalled = true;
  window.addEventListener('message', function(event) {
    var data = event.data;
    if (!data || data.type !== 'html-artifact-height' || !data.id) return;
    var sel = '.html-artifact-frame[data-frame-id="' + CSS.escape(data.id) + '"]';
    var frame = document.querySelector(sel);
    if (!frame) return;
    // Stop auto-fitting height once the user has manually resized the frame.
    if (frame.dataset.manualHeight === '1') return;
    var cap = Math.floor(window.innerHeight * 0.8);
    frame.style.height = Math.min(Number(data.height) + 2, cap) + 'px';
  });
}
installHtmlArtifactListener();

function renderToolActivity(tools) {
  if (!Array.isArray(tools) || !tools.length) return '';
  var rows = tools.map(function(tool, i) {
    var summary = toolInputSummary(tool);
    var text = summary.text;
    var limit = summary.limit;
    var summaryHtml;
    if (limit > 0 && text.length > limit) {
      var sid = 'ts-' + Math.random().toString(36).slice(2);
      summaryHtml = escapeHtml(text.substring(0, limit))
        + '<span id="' + sid + '-short">… <button onclick="document.getElementById(\'' + sid + '-short\').style.display=\'none\';document.getElementById(\'' + sid + '-full\').style.display=\'inline\'" class="text-blue-400 hover:underline">Show more</button></span>'
        + '<span id="' + sid + '-full" style="display:none">' + escapeHtml(text.substring(limit)) + '</span>';
    } else {
      summaryHtml = escapeHtml(text);
    }
    var outputHtml = '';
    if (tool.output) {
      var outText = String(tool.output);
      var colorCls = tool.is_error ? 'text-red-400' : 'text-slate-400';
      if (outText.length > 500) {
        var oid = 'to-' + Math.random().toString(36).slice(2);
        outputHtml = '<pre class="mt-1 text-xs ' + colorCls + ' whitespace-pre-wrap break-all">'
          + escapeHtml(outText.substring(0, 500))
          + '<span id="' + oid + '-short">… <button onclick="document.getElementById(\'' + oid + '-short\').style.display=\'none\';document.getElementById(\'' + oid + '-full\').style.display=\'inline\'" class="text-blue-400 hover:underline">Show more</button></span>'
          + '<span id="' + oid + '-full" style="display:none">' + escapeHtml(outText.substring(500)) + '</span>'
          + '</pre>';
      } else {
        outputHtml = '<pre class="mt-1 text-xs ' + colorCls + ' whitespace-pre-wrap break-all">' + escapeHtml(outText) + '</pre>';
      }
    }
    var borderCls = i > 0 ? 'border-t border-slate-600/50 ' : '';
    var truncCls = (limit > 0 && text.length > limit) ? '' : 'truncate ';
    return '<div class="' + borderCls + 'py-1.5">'
      + '<div class="flex items-center gap-2">'
      + '<span class="px-2 py-0.5 rounded-full text-xs font-medium bg-blue-900/60 text-blue-300 border border-blue-700/50">' + escapeHtml(tool.name || '') + '</span>'
      + '<span class="text-xs text-slate-400 ' + truncCls + 'flex-1 min-w-0">' + summaryHtml + '</span>'
      + '</div>'
      + outputHtml
      + '</div>';
  }).join('');
  var label = tools.length + ' tool call' + (tools.length !== 1 ? 's' : '');
  return '<div class="mt-2 border border-slate-600/30 rounded-lg overflow-hidden">'
    + '<button onclick="this.nextElementSibling.classList.toggle(\'hidden\')" class="w-full flex items-center justify-between px-3 py-1.5 bg-slate-800/50 hover:bg-slate-800 transition-colors text-xs text-slate-400">'
    + '<span>' + label + '</span>'
    + '<svg class="w-3 h-3" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M19 9l-7 7-7-7"/></svg>'
    + '</button>'
    + '<div class="hidden bg-slate-800/30 px-3 py-2">' + rows + '</div>'
    + '</div>';
}

function renderedMessageId(el) {
  return el && el.dataset ? (el.dataset.messageId || '') : '';
}

function renderedMessageRole(el) {
  return el && el.dataset ? (el.dataset.messageRole || '') : '';
}

function isStableRenderedMessage(el) {
  return Boolean(renderedMessageId(el) && renderedMessageRole(el));
}

function resetTurnFolds(root) {
  Array.from(root.querySelectorAll('.turn-fold-content')).forEach(content => {
    const parent = content.parentNode;
    while (content.firstElementChild) {
      parent.insertBefore(content.firstElementChild, content);
    }
    content.remove();
  });
  Array.from(root.querySelectorAll('.turn-fold-bar')).forEach(bar => bar.remove());
}

function collectCompletedTurns(root) {
  const turns = [];
  let turnStart = null;

  Array.from(root.children).forEach(el => {
    if (!isStableRenderedMessage(el)) return;
    const role = renderedMessageRole(el);
    if (role === 'user') {
      turnStart = el;
      return;
    }
    if (role === 'separator') {
      if (turnStart) turns.push({start: turnStart, separator: el});
      turnStart = null;
    }
  });

  return turns;
}

function findTurnConclusion(turn) {
  let el = turn.separator.previousElementSibling;
  while (el && el !== turn.start) {
    if (isStableRenderedMessage(el) && renderedMessageRole(el) === 'assistant') return el;
    el = el.previousElementSibling;
  }
  return null;
}

function collectIntermediateTurnMessages(turn, conclusion) {
  const messages = [];
  let el = turn.start.nextElementSibling;
  while (el && el !== conclusion) {
    if (isStableRenderedMessage(el)) messages.push(el);
    el = el.nextElementSibling;
  }
  return messages;
}

function turnFoldKey(turn, conclusion) {
  return [
    renderedMessageId(turn.start),
    renderedMessageId(conclusion),
    renderedMessageId(turn.separator),
  ].join('|');
}

function turnFoldLabel(count) {
  return count + ' step' + (count === 1 ? '' : 's');
}

function setTurnFoldBarExpanded(btn, expanded) {
  btn.setAttribute('aria-expanded', String(expanded));
  btn.setAttribute('title', expanded ? 'Collapse steps' : 'Expand steps');
}

function buildTurnFoldBar(turnKey, count, expanded) {
  const btn = document.createElement('button');
  btn.type = 'button';
  btn.className = 'turn-fold-bar';
  btn.dataset.turnFoldKey = turnKey;
  btn.dataset.turnFoldCount = String(count);
  btn.onclick = function() { toggleTurnFold(this); };
  btn.innerHTML = '<span class="turn-fold-label">' + turnFoldLabel(count) + '</span>'
    + '<svg class="turn-fold-chevron" fill="none" stroke="currentColor" viewBox="0 0 24 24">'
    + '<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M19 9l-7 7-7-7"/></svg>';
  setTurnFoldBarExpanded(btn, expanded);
  return btn;
}

function installTurnFold(root, turn, collapsed) {
  const conclusion = findTurnConclusion(turn);
  if (!conclusion) return;

  const messages = collectIntermediateTurnMessages(turn, conclusion);
  if (!messages.length) return;

  const content = document.createElement('div');
  content.className = 'turn-fold-content space-y-3';
  content.dataset.turnFoldKey = turnFoldKey(turn, conclusion);
  content.classList.toggle('hidden', collapsed);

  const bar = buildTurnFoldBar(content.dataset.turnFoldKey, messages.length, !collapsed);
  const ref = messages[0];
  root.insertBefore(bar, ref);
  root.insertBefore(content, ref);
  messages.forEach(el => content.appendChild(el));
}

function applyCompactMode(root) {
  if (!root) {
    updateCompactModeButton();
    return;
  }

  resetTurnFolds(root);
  const turns = collectCompletedTurns(root);
  turns.forEach(turn => {
    installTurnFold(root, turn, shouldCollapseTurn());
  });
  updateCompactModeButton();
}

function shouldCollapseTurn() {
  return compactMode !== 'expanded';
}

function toggleTurnFold(btn) {
  const content = btn.nextElementSibling;
  if (!content || !content.classList.contains('turn-fold-content')) {
    throw new Error('Turn fold content missing');
  }
  const collapsed = content.classList.toggle('hidden');
  setTurnFoldBarExpanded(btn, !collapsed);
}

function updateCompactModeButton() {
  const btn = document.getElementById('compact-mode-toggle');
  if (!btn) return;
  const expanded = compactMode === 'expanded';
  btn.textContent = expanded ? 'Compact' : 'Expand all';
  btn.setAttribute('title', expanded ? 'Collapse completed turns' : 'Expand collapsed turns');
  btn.setAttribute('aria-pressed', String(!expanded));
}

function toggleCompactMode() {
  compactMode = compactMode === 'expanded' ? 'compact' : 'expanded';
  applyCompactMode(document.getElementById('messages'));
}

function renderMessagesIntoContainer(container, messages, sessionId) {
  const streamEl = document.getElementById('streaming-msg');
  const streamHtml = streamEl ? streamEl.outerHTML : '';
  const parts = (messages || []).map(msg => renderMessage(msg, sessionId));
  container.innerHTML = parts.join('') + streamHtml;
  postProcessRenderedMessages(container);
  applyCompactMode(container);
}

function postProcessRenderedMessages(root) {
  root.querySelectorAll('.prose-msg').forEach(renderChatMath);
  embedLinkedHtmlArtifacts(root);
  root.querySelectorAll('.bubble-time[data-ts]').forEach(el => {
    el.textContent = formatBubbleTime(el.dataset.ts);
  });
  root.querySelectorAll('.rounded-full[title]').forEach(el => {
    const t = el.getAttribute('title');
    if (t && t.includes('T')) el.title = formatBubbleTime(t);
  });
}

function renderMessagesToDetachedContainer(messages, sessionId) {
  const tempDiv = document.createElement('div');
  tempDiv.innerHTML = (messages || []).map(msg => renderMessage(msg, sessionId)).join('');
  postProcessRenderedMessages(tempDiv);
  return tempDiv;
}

function renderMessage(msg, sessionId) {
  function timeDiv(colorClass) {
    if (!msg.timestamp) return "";
    var cls = colorClass || "text-slate-400/60";
    return "<div class=\"text-[10px] " + cls + " mt-1\">" + formatBubbleTime(msg.timestamp) + "</div>";
  }
  function mdDiv(text) {
    var raw = escapeHtml(text || "").replace(/"/g, "&quot;");
    return "<div class=\"prose-msg\" data-raw=\"" + raw + "\">" + marked.parse(fixNestedFences(text || "")) + "</div>";
  }

  if (msg.role === "user") {
    return "<div class=\"flex justify-end\"" + messageIdentityAttrs(msg) + ">"
      + renderUserMessageBubble(msg.content, msg.is_voice, msg.timestamp, msg.uploaded_files) + "</div>";
  }
  if (msg.role === "assistant") {
    var toolsHtml = renderToolActivity(msg.tools);
    return "<div class=\"flex justify-start\"" + messageIdentityAttrs(msg) + "><div class=\"max-w-[90%] overflow-hidden bg-slate-700 rounded-2xl rounded-bl-md px-4 py-2.5 text-sm\">"
      + mdDiv(msg.content) + toolsHtml + timeDiv() + "</div></div>";
  }
  if (msg.role === "system") {
    var titleAttr = msg.timestamp ? " title=\"" + formatBubbleTime(msg.timestamp) + "\"" : "";
    return "<div class=\"flex justify-center\"" + messageIdentityAttrs(msg) + "><div class=\"bg-slate-700/50 text-slate-400 text-xs px-3 py-1.5 rounded-full max-w-[85%] overflow-hidden truncate\"" + titleAttr + ">"
      + escapeHtml(msg.content) + "</div></div>";
  }
  if (msg.role === "task_delegated") {
    return "<div class=\"flex justify-start\"" + messageIdentityAttrs(msg) + "><div class=\"max-w-[90%] overflow-hidden bg-amber-900/30 border border-amber-700/30 rounded-2xl rounded-bl-md px-4 py-2.5 text-sm text-slate-300\">"
      + "<div class=\"flex items-center gap-2 text-amber-400 text-xs font-semibold mb-2\">"
      + "<svg class=\"w-3.5 h-3.5\" fill=\"none\" stroke=\"currentColor\" viewBox=\"0 0 24 24\"><path stroke-linecap=\"round\" stroke-linejoin=\"round\" stroke-width=\"2\" d=\"M13 5l7 7-7 7M5 5l7 7-7 7\"/></svg>"
      + "Delegated</div>"
      + "<div class=\"whitespace-pre-wrap\">" + escapeHtml(msg.content) + "</div>" + timeDiv() + "</div></div>";
  }
  if (msg.role === "worker_summary") {
    var escaped = escapeHtml(msg.full_content || "").replace(/"/g, "&quot;");
    return "<div class=\"flex justify-start\"" + messageIdentityAttrs(msg) + "><div class=\"max-w-[90%] overflow-hidden bg-emerald-900/40 border border-emerald-700/30 rounded-2xl rounded-bl-md px-4 py-2.5 text-sm text-slate-300 cursor-pointer\""
      + " data-full=\"" + escaped + "\""
      + " onclick=\"showTextModal(\x27Worker Result\x27, this.dataset.full)\">"
      + mdDiv(msg.content) + timeDiv("text-emerald-400/50") + "</div></div>";
  }
  if (msg.role === "thinking") {
    var thinkId = 'think-' + Math.random().toString(36).slice(2);
    return "<div class=\"flex justify-start\"" + messageIdentityAttrs(msg) + "><div class=\"max-w-[90%] overflow-hidden bg-slate-800/60 border border-slate-700/40 rounded-2xl rounded-bl-md px-4 py-2.5 text-sm\">"
      + "<button onclick=\"const el=document.getElementById(\x27" + thinkId + "\x27);el.style.display=el.style.display===\x27none\x27?\x27block\x27:\x27none\x27\" class=\"text-xs text-slate-500 hover:text-slate-400 italic\">Thinking…</button>"
      + "<div id=\"" + thinkId + "\" style=\"display:none\" class=\"mt-1 text-xs text-slate-500 whitespace-pre-wrap\">" + escapeHtml(String(msg.content || '')) + "</div>"
      + timeDiv() + "</div></div>";
  }
  if (msg.role === "plan") {
    return "<div class=\"flex justify-start\"" + messageIdentityAttrs(msg) + "><div class=\"max-w-[90%] overflow-hidden bg-slate-800 border border-blue-500/30 rounded-2xl px-4 py-3 text-sm\">"
      + "<div class=\"flex items-center gap-2 text-blue-400 text-xs font-semibold mb-2\">"
      + "<svg class=\"w-3.5 h-3.5\" fill=\"none\" stroke=\"currentColor\" viewBox=\"0 0 24 24\"><path stroke-linecap=\"round\" stroke-linejoin=\"round\" stroke-width=\"2\" d=\"M9 5H7a2 2 0 00-2 2v12a2 2 0 002 2h10a2 2 0 002-2V7a2 2 0 00-2-2h-2M9 5a2 2 0 002 2h2a2 2 0 002-2M9 5a2 2 0 012-2h2a2 2 0 012 2m-6 9l2 2 4-4\"/></svg>"
      + "Plan</div>"
      + mdDiv(msg.content) + timeDiv() + "</div></div>";
  }
  if (msg.role === "clone_start") {
    return "<div class=\"flex items-center gap-3 py-3 px-4\"" + messageIdentityAttrs(msg) + ">"
      + "<div class=\"flex-1 border-t border-purple-500/40\"></div>"
      + "<div class=\"flex items-center gap-2 text-purple-400 text-xs\">"
      + "<svg class=\"w-3.5 h-3.5\" fill=\"none\" stroke=\"currentColor\" viewBox=\"0 0 24 24\"><path stroke-linecap=\"round\" stroke-linejoin=\"round\" stroke-width=\"2\" d=\"M6 3v12M6 9h6m0 0V3m0 6v6m0 0h6\"/></svg>"
      + "<span>Cloned from <a href=\"/?session=" + encodeURIComponent(msg.parent_session_id || "")
      + "\" class=\"text-purple-300 hover:text-purple-200 underline\">"
      + escapeHtml(msg.content || "") + "</a></span></div>"
      + "<div class=\"flex-1 border-t border-purple-500/40\"></div></div>";
  }
  if (msg.role === "separator") {
    var timeStr = msg.thinking_seconds != null ? " &middot; " + msg.thinking_seconds + "s" : "";
    var buttons = "";
    if (sessionId) {
      if (msg.event_index != null) {
        buttons = "<button onclick=\"forkSession(\x27" + sessionId + "\x27, " + msg.event_index + ")\""
          + " class=\"p-0.5 text-slate-500 hover:text-green-400\" title=\"Clone to here\">"
          + "<svg class=\"w-3.5 h-3.5\" fill=\"none\" stroke=\"currentColor\" viewBox=\"0 0 24 24\"><path stroke-linecap=\"round\" stroke-linejoin=\"round\" stroke-width=\"2\" d=\"M6 3v12M6 9h6m0 0V3m0 6v6m0 0h6\"/></svg>"
          + "</button>"
          + "<button onclick=\"eloneSession(\x27" + sessionId + "\x27, " + msg.event_index + ")\""
          + " class=\"p-0.5 text-slate-500 hover:text-yellow-400\" title=\"Elon-e: retry with a fresh perspective\">"
          + "<svg class=\"w-3.5 h-3.5\" fill=\"none\" stroke=\"currentColor\" viewBox=\"0 0 24 24\"><path stroke-linecap=\"round\" stroke-linejoin=\"round\" stroke-width=\"2\" d=\"M13 10V3L4 14h7v7l9-11h-7z\"/></svg>"
          + "</button>"
          + "<button onclick=\"toggleRecapPanel(this, \x27" + sessionId + "\x27, " + msg.event_index + ")\""
          + " class=\"p-0.5 text-slate-500 hover:text-sky-400\" title=\"Recap: what this section covered\">"
          + "<svg class=\"w-3.5 h-3.5\" fill=\"none\" stroke=\"currentColor\" viewBox=\"0 0 24 24\"><path stroke-linecap=\"round\" stroke-linejoin=\"round\" stroke-width=\"2\" d=\"M3.75 6h16.5M3.75 12h16.5M3.75 18h10.5\"/></svg>"
          + "</button>";
      }
      if (msg.id != null) {
        buttons += renderRoundRatingButtons(sessionId, msg.id);
      }
    }
    return "<div class=\"flex items-center gap-3 py-2 px-4 separator-line group/sep\"" + messageIdentityAttrs(msg) + ">"
      + "<div class=\"flex-1 border-t border-slate-600/40\"></div>"
      + "<span class=\"text-xs text-slate-500 whitespace-nowrap\">response complete" + timeStr + "</span>"
      + buttons
      + "<div class=\"flex-1 border-t border-slate-600/40\"></div></div>";
  }
  return "";
}

function _appendRenderedMessage(html, forceScroll) {
  var container = document.getElementById("messages");
  var wasAtBottom = shouldAutoScroll(container);
  var wrapper = document.createElement("div");
  wrapper.innerHTML = html;
  postProcessRenderedMessages(wrapper);
  var el = wrapper.firstElementChild || wrapper;
  var streamEl = document.getElementById("streaming-msg");
  container.insertBefore(el, streamEl);
  if (forceScroll || wasAtBottom) {
    container.scrollTop = container.scrollHeight;
  } else {
    showScrollToBottom();
  }
}

function appendMessageObject(msg, sessionId) {
  _appendRenderedMessage(renderMessage(msg, sessionId || SESSION_ID), msg.role === "user");
}

async function sendMessage() {
  if (uploadsInFlight > 0) {
    showToast('Please wait for uploads to finish', true);
    return;
  }
  const input = document.getElementById('msg-input');
  const content = input.value.trim();
  const uploadedFilesForPayload = getUploadedFilesForPayload();
  if ((!content && !uploadedFilesForPayload.length) || !SESSION_ID) return;
  if (content.startsWith('/')) {
    const spaceIdx = content.indexOf(' ');
    const name = spaceIdx === -1 ? content.slice(1) : content.slice(1, spaceIdx);
    const args = spaceIdx === -1 ? '' : content.slice(spaceIdx + 1).trim();
    await executeSlashCommand(name, args, {displayText: content, uploadedFiles: uploadedFilesForPayload});
    return;
  }
  const contentWithCtx = applyWorkingContext(content);
  const payloadFiles = uploadedFilesForPayload.map((file) => ({
    filename: file.filename,
    path: file.path,
    size: file.size,
  }));
  clearSentUploadedFiles(uploadedFilesForPayload.map((file) => file.id));

  // Optimistic UI: append user message and bump session to top
  pendingUserMsg = true;
  appendMessage('user', content, false, new Date().toISOString(), payloadFiles);
  bumpCurrentSessionToTop();
  input.value = '';
  input.style.height = 'auto';
  if (DRAFT_KEY) localStorage.removeItem(DRAFT_KEY);

  // Start thinking indicator
  startThinking();

  try {
    const res = await fetch(`/api/chat/${SESSION_ID}/message`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ content: contentWithCtx, uploaded_files: payloadFiles }),
    });
    if (!res.ok) throw new Error(String(res.status));
  } catch (err) {
    console.error('Send failed:', err);
    pendingUserMsg = false;
    appendMessage('system', 'Failed to send message');
    stopThinking();
  }
}

function appendMessage(role, content, isVoice, timestamp, uploadedFiles) {
  var msg = {
    role: role,
    content: content || "",
    is_voice: !!isVoice,
    timestamp: timestamp || null,
    uploaded_files: uploadedFiles || null,
  };
  _appendRenderedMessage(renderMessage(msg, SESSION_ID), role === "user");
}

function appendSeparator(seconds, eventIndex) {
  var msg = {
    role: "separator",
    thinking_seconds: seconds,
    event_index: eventIndex,
  };
  _appendRenderedMessage(renderMessage(msg, SESSION_ID));
}

function appendCloneBanner(parentName, parentSessionId) {
  var msg = {
    role: "clone_start",
    content: parentName,
    parent_session_id: parentSessionId,
  };
  _appendRenderedMessage(renderMessage(msg, SESSION_ID));
}

function escapeHtml(str) {
  const d = document.createElement('div');
  d.textContent = str;
  return d.innerHTML;
}

// ---------------------------------------------------------------------------
// Scroll-to-bottom floating button
// ---------------------------------------------------------------------------
function showScrollToBottom() {
  const btn = document.getElementById('scroll-to-bottom');
  if (btn) btn.classList.remove('hidden');
}

function hideScrollToBottom() {
  const btn = document.getElementById('scroll-to-bottom');
  if (btn) btn.classList.add('hidden');
}

function scrollToBottom() {
  const container = document.getElementById('messages');
  if (container) container.scrollTop = container.scrollHeight;
  hideScrollToBottom();
}

// Hide the button when user scrolls back to bottom
document.addEventListener('DOMContentLoaded', () => {
  initializeRoundRatings();
  const container = document.getElementById('messages');
  if (container) {
    container.addEventListener('scroll', () => {
      if (shouldAutoScroll(container)) hideScrollToBottom();
    });
  }
});
