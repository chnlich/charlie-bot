
(function() {
  const Chat = globalThis.Chat;

// ---------------------------------------------------------------------------
// HTML artifact rendering — linked artifacts/*.html render as compact cards.
// The sandboxed iframe (a whole live document with its own scripts) exists only
// while the user has the card expanded, and both the fetch cache and the number
// of simultaneously expanded frames are bounded.
// ---------------------------------------------------------------------------
var HTML_ARTIFACT_LINK_RE = /(^|\/)artifacts\/[^/]+\.html$/;
var HTML_ARTIFACT_CACHE_MAX = 8;
var MAX_EXPANDED_ARTIFACTS = 3;
var htmlArtifactFetchCache = new Map();
var expandedArtifactCards = [];

// The two prefixes the file server answers on (server.py mounts one router under both):
// "/absolute_filepath/" is the form written into chat text, "/files/" is what this UI builds
// and what older messages carry. Every link below is normalized to the absolute path it names
// before anything else looks at it, so cards, dedupe keys, cache keys and the plan version
// badge see one path per file whichever prefix it arrived under.
var FILE_SERVER_PREFIXES = ['/files', '/absolute_filepath'];
var FILE_SERVER_PREFIX_GROUP = '(?:' + FILE_SERVER_PREFIXES.join('|') + ')';

function absolutePathFromServedPathname(pathname) {
  for (var i = 0; i < FILE_SERVER_PREFIXES.length; i++) {
    var prefix = FILE_SERVER_PREFIXES[i] + '/';
    if (pathname.indexOf(prefix) !== 0) continue;
    try {
      return '/' + decodeURIComponent(pathname.slice(prefix.length));
    } catch (e) {
      console.warn('Invalid file-server path', pathname, e);
      return null;
    }
  }
  return null;
}

function parseLinkUrl(href) {
  try {
    return new URL(href, window.location.href);
  } catch (e) {
    console.warn('Invalid link', href, e);
    return null;
  }
}

// A link naming this page's hostname with a different scheme or port names this same server:
// the base URL is written from memory, and a wrong scheme or port there is still unambiguous,
// so the link is pulled back to the page origin. Another hostname is another server, and is
// returned as written.
function normalizedToPageOrigin(url) {
  var page = new URL(window.location.href);
  if (url.hostname !== page.hostname || url.origin === page.origin) return url;
  return new URL(url.pathname + url.search + url.hash, page.origin);
}

function resolveFileServerUrl(href) {
  var url = parseLinkUrl(href);
  return url ? normalizedToPageOrigin(url) : null;
}

function basename(path) {
  var parts = String(path || '').split('/');
  return parts[parts.length - 1];
}

function resolveArtifactAbsolutePath(filePath) {
  if (filePath.charAt(0) === '/') return filePath;
  var root = (typeof window !== 'undefined' && window.SESSIONS_ROOT) ? window.SESSIONS_ROOT : '';
  if (!root) {
    throw new Error('window.SESSIONS_ROOT not injected');
  }
  return root + '/' + SESSION_ID + '/' + filePath;
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

function injectLinkBehavior(html, absPath) {
  var baseHref = '/files' + absPath;
  var src = String(html || '');
  var hasBase = /<base\b/i.test(src);
  var baseTag = '<base href="' + escapeHtml(baseHref) + '">';
  var withBase;
  if (hasBase) {
    withBase = src;
  } else {
    var headMatch = src.match(/<head\b[^>]*>/i);
    if (headMatch) {
      var headEnd = headMatch.index + headMatch[0].length;
      withBase = src.slice(0, headEnd) + baseTag + src.slice(headEnd);
    } else {
      withBase = baseTag + src;
    }
  }
  var interceptor = '<script>(function(){'
    + 'document.addEventListener("click", function(e){'
    + 'if(e.defaultPrevented) return;'
    + 'if(e.ctrlKey||e.metaKey||e.shiftKey||e.altKey) return;'
    + 'if(e.button!==0) return;'
    + 'var a=e.target.closest("a[href]");'
    + 'if(!a) return;'
    + 'var raw=a.getAttribute("href");'
    + 'if(raw.charAt(0)==="#"){'
    + 'e.preventDefault();'
    + 'if(raw==="#") return;'
    + 'var frag;'
    + 'try{frag=decodeURIComponent(raw.slice(1));}catch(_e){frag=raw.slice(1);}'
    + 'var el=document.getElementById(frag)||document.getElementsByName(frag)[0];'
    + 'if(el) el.scrollIntoView();'
    + 'return;'
    + '}'
    + 'e.preventDefault();'
    + 'var url;'
    + 'try{url=new URL(raw, document.baseURI);}catch(_e){console.warn("Invalid href in artifact", raw);return;}'
    + 'window.open(url.href, "_blank", "noopener");'
    + '});'
    + '})();<\/script>';
  var bodyIdx = withBase.lastIndexOf('</body>');
  if (bodyIdx === -1) return withBase + interceptor;
  return withBase.slice(0, bodyIdx) + interceptor + withBase.slice(bodyIdx);
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

function stampViewingSessionFragment(href) {
  if (typeof SESSION_ID === 'undefined' || !SESSION_ID) return href;
  var raw = String(href || '');
  var hashIdx = raw.indexOf('#');
  var base = hashIdx === -1 ? raw : raw.slice(0, hashIdx);
  return base + '#cbsession=' + encodeURIComponent(String(SESSION_ID));
}

function saveHtmlArtifactSavedSize(filePath, size) {
  var key = htmlArtifactSizeStorageKey(filePath);
  if (!key) return;
  try {
    localStorage.setItem(key, JSON.stringify(size));
  } catch (e) {}
}

// Inner markup of an expanded card: the live iframe plus its toolbar, resize
// handles and source view. Built on expand only, discarded on collapse.
function buildHtmlArtifactFrameHtml(opts) {
  var filePath = opts.filePath || opts.absPath || '';
  var absPath = opts.absPath || resolveArtifactAbsolutePath(filePath);
  var rawHtml = opts.html || '';
  var frameId = 'hf-' + Math.random().toString(36).slice(2);
  var withScript = injectResizeScript(injectLinkBehavior(rawHtml, absPath), frameId);
  var srcdoc = escapeForSrcdoc(withScript);
  var openUrl = stampViewingSessionFragment('/files' + absPath);
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
  return '<div class="html-artifact-toolbar" style="' + widthStyle + '">'
    + '<span class="filename">' + escapeHtml(basename(filePath)) + '</span>'
    + '<button type="button" onclick="toggleHtmlArtifactEmbed(this)">Collapse</button>'
    + '<button type="button" onclick="expandHtmlArtifact(this)">Full screen</button>'
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
    + sourceHighlighted + '</code></pre></div>';
}

function resolveHtmlArtifactLink(source) {
  var href = '';
  if (typeof source === 'string') {
    href = source;
  } else if (source && source.getAttribute) {
    href = source.getAttribute('href') || '';
  }
  if (!href) return null;
  var url = resolveFileServerUrl(href);
  if (!url) return null;
  var pathname = url.pathname || '';
  if (!HTML_ARTIFACT_LINK_RE.test(pathname)) return null;
  var absPath = absolutePathFromServedPathname(pathname);
  if (absPath === null) return null;
  return {fetchUrl: pathname, absPath: absPath};
}

var HTML_ARTIFACT_CODE_RE = new RegExp(
  '(?:https?:\\/\\/[^\\s]+?)?' + FILE_SERVER_PREFIX_GROUP + '\\/[^\\s]*\\/artifacts\\/[^/\\s]+\\.html', 'g');

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

// Map iteration order is insertion order, so re-inserting on read makes the
// first key the least recently used one.
function touchHtmlArtifactCache(absPath, promise) {
  htmlArtifactFetchCache.delete(absPath);
  htmlArtifactFetchCache.set(absPath, promise);
  while (htmlArtifactFetchCache.size > HTML_ARTIFACT_CACHE_MAX) {
    htmlArtifactFetchCache.delete(htmlArtifactFetchCache.keys().next().value);
  }
}

function fetchHtmlArtifact(absPath, fetchUrl) {
  var cached = htmlArtifactFetchCache.get(absPath);
  if (cached) {
    touchHtmlArtifactCache(absPath, cached);
    return cached;
  }
  var promise = fetch(fetchUrl).then(function(resp) {
    if (!resp.ok) {
      // The status rides along on the error: it is what tells an expand that the artifact is
      // gone rather than that the fetch went wrong some other way.
      var failure = new Error('HTTP ' + resp.status + ' fetching ' + fetchUrl);
      failure.status = resp.status;
      throw failure;
    }
    return resp.text();
  }).catch(function(e) {
    htmlArtifactFetchCache.delete(absPath);
    throw e;
  });
  touchHtmlArtifactCache(absPath, promise);
  return promise;
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

// ---------------------------------------------------------------------------
// Missing file links — a file link whose target the server has nothing at is
// marked where it appears, so a wrong path shows up on the render rather than
// on the click that fails. Only a 404 marks: any other status, and any network
// error, leaves the link alone, so the marker keeps meaning "the server looked
// and the file is not there". HTML artifact links are decided by the fetch that
// builds their card, so they cost no request of their own here.
// ---------------------------------------------------------------------------
var MISSING_FILE_MARKER_TEXT = '\u26A0 missing';
var MISSING_FILE_MARKER_TITLE = 'The file server found nothing at this path';
// A run of text naming a path under either prefix, with or without a scheme and host in front.
// The class ends the run at whitespace, at the closers markdown and prose put after a URL, and
// at everything outside printable ASCII: a chat URL is a maximal printable-ASCII run
// [\x21-\x7E]+, so a glued CJK character, full-width mark or emoji is prose, not the tail of
// the link. \x7F-\uFFFF also covers astral characters, whose UTF-16 surrogate code units fall
// inside that range. markdown-renderer.js's tokenizer.url override cuts bare URLs at the same
// boundary — two copies of one definition, kept in sync by the comments in both files; the
// modules load independently, so sharing a constant would couple their load order.
var FILE_SERVER_LINK_SOURCE =
  '(?:https?:\\/\\/[^\\s]+?)?' + FILE_SERVER_PREFIX_GROUP + '\\/[^\\s\\)\\]"\'`<>\\x7F-\\uFFFF]+';
var LINK_TRAILING_PUNCTUATION_RE = /[.,;:!?*]+$/;

function buildMissingMarker() {
  var marker = document.createElement('span');
  marker.className = 'file-link-missing';
  marker.title = MISSING_FILE_MARKER_TITLE;
  marker.textContent = MISSING_FILE_MARKER_TEXT;
  return marker;
}

function missingMarkerHtml() {
  return '<span class="file-link-missing" title="' + escapeHtml(MISSING_FILE_MARKER_TITLE) + '">'
    + escapeHtml(MISSING_FILE_MARKER_TEXT) + '</span>';
}

// One occurrence of a file link: the absolute path it names, the URL a probe would ask for,
// and where the marker goes. `end` is the offset just past the link inside a text node, which
// is where that node has to be split; element carriers take the marker as a sibling instead.
function fileServerOccurrence(href, node, kind, end) {
  var url = resolveFileServerUrl(href);
  if (!url) return null;
  var pathname = url.pathname || '';
  var absPath = absolutePathFromServedPathname(pathname);
  if (absPath === null) return null;
  // An HTML artifact link already has an answer coming from the fetch that builds its card;
  // probing it here would be a second request for the same thing.
  if (HTML_ARTIFACT_LINK_RE.test(pathname)) return null;
  return {absPath: absPath, probeUrl: url.href, node: node, kind: kind, end: end};
}

function collectFileServerOccurrences(out, text, node, kind) {
  if (!text) return;
  var pattern = new RegExp(FILE_SERVER_LINK_SOURCE, 'g');
  var match;
  while ((match = pattern.exec(text)) !== null) {
    var href = match[0].replace(LINK_TRAILING_PUNCTUATION_RE, '');
    var occurrence = fileServerOccurrence(href, node, kind, match.index + href.length);
    if (occurrence) out.push(occurrence);
  }
}

function probeFileMissing(probeUrl) {
  return fetch(probeUrl, {method: 'HEAD'}).then(function(resp) {
    return resp.status === 404;
  }).catch(function(e) {
    console.warn('file link probe failed', probeUrl, e);
    return false;
  });
}

function markMissingFileLinks(occurrences) {
  if (!occurrences.length) return Promise.resolve();
  var byPath = new Map();
  occurrences.forEach(function(occurrence) {
    var group = byPath.get(occurrence.absPath);
    if (!group) {
      group = [];
      byPath.set(occurrence.absPath, group);
    }
    group.push(occurrence);
  });
  var groups = Array.from(byPath.values());
  // One HEAD per unique path, and marking waits for all of them: a text node is split once
  // with every missing occurrence in it at hand, since splitting it moves the later offsets.
  return Promise.all(groups.map(function(group) {
    return probeFileMissing(group[0].probeUrl);
  })).then(function(missing) {
    var marked = [];
    groups.forEach(function(group, index) {
      if (missing[index]) marked = marked.concat(group);
    });
    markOccurrencesMissing(marked);
  });
}

function markOccurrencesMissing(occurrences) {
  var textNodes = new Map();
  occurrences.forEach(function(occurrence) {
    if (occurrence.kind === 'text') {
      var group = textNodes.get(occurrence.node);
      if (!group) {
        group = [];
        textNodes.set(occurrence.node, group);
      }
      group.push(occurrence);
      return;
    }
    insertMissingMarkerAfter(occurrence.node);
  });
  textNodes.forEach(splitTextNodeAroundMarkers);
}

// The chat can be rebuilt (SPA session switch) while a probe is in flight, which leaves the
// carrier detached; a marker on a detached node would never be seen, so there is nothing to do.
function insertMissingMarkerAfter(node) {
  var parent = node.parentNode;
  if (!parent) return;
  parent.insertBefore(buildMissingMarker(), node.nextSibling);
}

// A text node carries no attribute to hang a marker off, so it is split just past each missing
// link and the marker goes between the halves. Offsets ascend, so one pass rebuilds the node.
function splitTextNodeAroundMarkers(occurrences, node) {
  var parent = node.parentNode;
  if (!parent) return;
  var text = node.nodeValue || '';
  var pieces = document.createDocumentFragment();
  var cursor = 0;
  occurrences.forEach(function(occurrence) {
    pieces.appendChild(document.createTextNode(text.slice(cursor, occurrence.end)));
    pieces.appendChild(buildMissingMarker());
    cursor = occurrence.end;
  });
  pieces.appendChild(document.createTextNode(text.slice(cursor)));
  parent.replaceChild(pieces, node);
}

function markArtifactCardMissing(card) {
  card.querySelector('.html-artifact-toolbar').insertAdjacentHTML('beforeend', missingMarkerHtml());
}

// ---------------------------------------------------------------------------
// Plan compact cards — registered plan-version links render as native cards.
// Pure helpers (exported for testing) live below; the DOM render path is in
// embedLinkedHtmlArtifacts. The registry snapshot is owned by the plan panel
// module (single source of truth, no client-side state derivation).
// ---------------------------------------------------------------------------

function buildSessionDir(sessionId, sessionsRoot) {
  var root = sessionsRoot;
  if (root == null && typeof window !== 'undefined' && window.SESSIONS_ROOT) root = window.SESSIONS_ROOT;
  if (!root) return '';
  return root + '/' + sessionId;
}

function _planStateLabel(plan) {
  if (typeof planPanel !== 'undefined' && planPanel && typeof planPanel.formatPlanStateLabel === 'function') {
    return planPanel.formatPlanStateLabel(plan);
  }
  if (plan && plan.takeoff != null && plan.state === 'approved') {
    return 'approved \u00B7 v' + plan.takeoff.v;
  }
  return plan ? plan.state : '';
}

// Both plan-version lookups return this one record shape; compact-card
// renders and badge updates read the same fields.
function _planVersionRecord(plan, ver) {
  return {planId: plan.id, v: ver.v, title: plan.title, state: _planStateLabel(plan), file: ver.file};
}

function lookupRegisteredPlanVersion(snapshot, absPath, sessionId, sessionsRoot) {
  var plans = (snapshot && snapshot.plans) || [];
  var sessionDir = buildSessionDir(sessionId, sessionsRoot);
  if (!sessionDir) return null;
  for (var i = 0; i < plans.length; i++) {
    var plan = plans[i];
    var versions = (plan && plan.versions) || [];
    for (var j = 0; j < versions.length; j++) {
      var ver = versions[j];
      if (!ver || !ver.file) continue;
      var expected = sessionDir + '/' + ver.file;
      if (absPath === expected) {
        return _planVersionRecord(plan, ver);
      }
    }
  }
  return null;
}

function decidePlanCardRender(snapshot, absPath, sessionId, sessionsRoot) {
  return lookupRegisteredPlanVersion(snapshot, absPath, sessionId, sessionsRoot) ? 'compact' : 'legacy';
}

function lookupPlanVersionState(snapshot, planId, v) {
  var plans = (snapshot && snapshot.plans) || [];
  for (var i = 0; i < plans.length; i++) {
    var plan = plans[i];
    if (String(plan && plan.id) !== String(planId)) continue;
    var versions = (plan && plan.versions) || [];
    for (var j = 0; j < versions.length; j++) {
      var ver = versions[j];
      if (Number(ver && ver.v) === Number(v)) {
        return _planVersionRecord(plan, ver);
      }
    }
  }
  return null;
}

var ARTIFACT_EXPAND_CONTROL = '<button type="button" onclick="toggleHtmlArtifactEmbed(this)">Expand</button>';

function buildCompactToolbarHtml(title, absPath, controls) {
  var openInTabUrl = stampViewingSessionFragment('/files' + absPath);
  return '<div class="html-artifact-toolbar">'
    + '<span class="filename">' + escapeHtml(title || '(untitled)') + '</span>'
    + controls
    + '<a href="' + escapeHtml(openInTabUrl) + '" target="_blank" rel="noopener noreferrer">Open in tab</a>'
    + '</div>';
}

// One compact card shape for both registered plan versions (which open in the
// plan panel) and any other HTML artifact (which expands into an iframe here).
// `opts.fetchUrl` is required for the non-plan variant.
function buildPlanCompactCardHtml(planId, v, title, state, absPath, opts) {
  if (planId == null) {
    return '<div class="artifact-compact-card html-artifact" data-artifact-path="' + escapeHtml(absPath) + '"'
      + ' data-artifact-fetch-url="' + escapeHtml((opts && opts.fetchUrl) || '') + '">'
      + buildCompactToolbarHtml(title, absPath, ARTIFACT_EXPAND_CONTROL)
      + '</div>';
  }
  var planControls = '<span class="plan-compact-version">v' + escapeHtml(v) + '</span>'
    + '<span class="plan-compact-state">' + escapeHtml(state || '') + '</span>'
    + '<button type="button" onclick="openPlanFromCard(this)">Open panel</button>';
  return '<div class="plan-compact-card html-artifact" data-artifact-path="' + escapeHtml(absPath) + '"'
    + ' data-plan-card-plan="' + escapeHtml(planId) + '"'
    + ' data-plan-card-version="' + escapeHtml(v) + '"'
    + ' data-plan-card-abs-path="' + escapeHtml(absPath) + '">'
    + buildCompactToolbarHtml(title, absPath, planControls)
    + '</div>';
}

function collapseArtifactCard(card) {
  var index = expandedArtifactCards.indexOf(card);
  if (index !== -1) expandedArtifactCards.splice(index, 1);
  var absPath = card.dataset.artifactPath;
  card.innerHTML = buildCompactToolbarHtml(basename(absPath), absPath, ARTIFACT_EXPAND_CONTROL);
  delete card.dataset.artifactExpanded;
}

function expandArtifactCard(card) {
  var absPath = card.dataset.artifactPath;
  var fetchUrl = card.dataset.artifactFetchUrl;
  return fetchHtmlArtifact(absPath, fetchUrl).then(function(html) {
    // The chat can be rebuilt (SPA session switch) while the fetch is in
    // flight; never revive a detached card into the expanded set.
    if (!card.isConnected) return;
    if (card.dataset.artifactExpanded === '1') return;
    card.innerHTML = buildHtmlArtifactFrameHtml({absPath: absPath, filePath: absPath, html: html});
    card.dataset.artifactExpanded = '1';
    expandedArtifactCards.push(card);
    // Oldest expansion wins the eviction: at most MAX_EXPANDED_ARTIFACTS live
    // documents exist in the chat at any time.
    while (expandedArtifactCards.length > MAX_EXPANDED_ARTIFACTS) {
      collapseArtifactCard(expandedArtifactCards[0]);
    }
  }).catch(function(e) {
    console.warn('Failed to expand linked HTML artifact', fetchUrl, e);
    // This fetch is the probe for an HTML artifact link: a 404 here is the answer a HEAD would
    // have brought back, so the card carries the marker and no extra request is made.
    if (e.status === 404) markArtifactCardMissing(card);
  });
}

function toggleHtmlArtifactEmbed(btn) {
  var card = btn.closest('.html-artifact');
  if (!card) return;
  if (card.dataset.artifactExpanded === '1') collapseArtifactCard(card);
  else expandArtifactCard(card);
}

function openPlanFromCard(btn) {
  var card = btn.closest('.plan-compact-card');
  if (!card) return;
  var planId = card.dataset.planCardPlan;
  var v = Number(card.dataset.planCardVersion);
  if (typeof planPanel !== 'undefined' && planPanel && typeof planPanel.openPlan === 'function') {
    planPanel.openPlan(planId, v);
  }
}

function updatePlanCardBadges(snapshot) {
  var cards = document.querySelectorAll('.plan-compact-card');
  if (!cards.length) return;
  cards.forEach(function(card) {
    var planId = card.dataset.planCardPlan;
    var v = Number(card.dataset.planCardVersion);
    var info = lookupPlanVersionState(snapshot, planId, v);
    if (!info) return;
    var badge = card.querySelector('.plan-compact-state');
    if (badge) badge.textContent = info.state;
  });
}

function _planRegistryReady() {
  if (typeof planPanel === 'undefined' || !planPanel) return null;
  if (typeof planPanel.ready !== 'function') return null;
  return planPanel.ready();
}

function _planRegistrySnapshot() {
  if (typeof planPanel === 'undefined' || !planPanel) return null;
  if (typeof planPanel.getRegistrySnapshot !== 'function') return null;
  return planPanel.getRegistrySnapshot();
}

// `reg` is the registered plan version for this path, or null for any other
// HTML artifact — both render through the same compact card.
function renderPlanCompactCard(link, ordinal, prose, reg) {
  var template = document.createElement('template');
  template.innerHTML = reg
    ? buildPlanCompactCardHtml(reg.planId, reg.v, reg.title, reg.state, link.absPath)
    : buildPlanCompactCardHtml(null, null, basename(link.absPath), null, link.absPath, {fetchUrl: link.fetchUrl});
  var card = template.content.firstElementChild;
  if (!card) {
    throw new Error('artifact compact card render produced no element');
  }
  var mc = document.getElementById('messages');
  var atBottom = mc ? shouldAutoScroll(mc) : false;
  insertHtmlArtifactCard(prose, card, ordinal);
  link.el.dataset.embedded = '1';
  if (atBottom && mc) mc.scrollTop = mc.scrollHeight;
}

function renderArtifactLink(link, ordinal, prose) {
  var ready = _planRegistryReady();
  if (!ready) {
    renderPlanCompactCard(link, ordinal, prose, null);
    return;
  }
  var isReadyFragment = isTurnEngineReadyFragment(prose);
  ready.then(function() {
    if (!link.el.isConnected && !isReadyFragment) return;
    if (link.el.dataset.embedded === '1') return;
    var currentProse = link.el.closest('.prose-msg');
    if (!currentProse || currentProse.closest('#streaming-msg')) return;
    if (findEmbeddedHtmlArtifactCard(currentProse, link.absPath)) {
      link.el.dataset.embedded = '1';
      return;
    }
    var snapshot = _planRegistrySnapshot();
    var reg = snapshot ? lookupRegisteredPlanVersion(snapshot, link.absPath, SESSION_ID, window.SESSIONS_ROOT) : null;
    renderPlanCompactCard(link, ordinal, currentProse, reg);
  }).catch(function(e) {
    console.warn('plan registry readiness failed; rendering the artifact card', e);
    renderPlanCompactCard(link, ordinal, prose, null);
  });
}

function makePlainTextArtifactHandle(prose) {
  return {
    dataset: {},
    get isConnected() { return prose.isConnected; },
    closest: function(sel) { return sel === '.prose-msg' ? prose : null; },
  };
}

function isTurnEngineReadyFragment(prose) {
  for (var node = prose; node; node = node.parentNode) {
    if (node.__turnEngineReadyFragment) return true;
  }
  return false;
}

function embedLinkedHtmlArtifacts(root) {
  root.querySelectorAll('.prose-msg').forEach(function(prose) {
    if (prose.id === 'streaming-msg' || prose.closest('#streaming-msg')) return;
    var links = [];
    var occurrences = [];
    var seen = new Set();
    Array.from(prose.querySelectorAll('a[href]')).forEach(function(anchor) {
      var href = anchor.getAttribute('href') || '';
      var url = parseLinkUrl(href);
      var normalized = url ? normalizedToPageOrigin(url) : null;
      // Writing the normalized href back is what makes the click land: a wrong scheme or port
      // on this host would otherwise be carried into the navigation exactly as written.
      // normalizedToPageOrigin hands back the same object when the link needs no rewrite.
      if (normalized && normalized !== url) {
        anchor.setAttribute('href', normalized.href);
        href = normalized.href;
      }
      var occurrence = fileServerOccurrence(href, anchor, 'element', null);
      if (occurrence) occurrences.push(occurrence);
      if (anchor.dataset.embedded === '1') return;
      var resolved = resolveHtmlArtifactLink(anchor);
      if (!resolved) return;
      anchor.setAttribute('href', stampViewingSessionFragment(anchor.getAttribute('href') || ''));
      if (seen.has(resolved.absPath)) return;
      seen.add(resolved.absPath);
      links.push({
        el: anchor,
        absPath: resolved.absPath,
        fetchUrl: resolved.fetchUrl,
      });
    });
    Array.from(prose.querySelectorAll('code')).forEach(function(code) {
      collectFileServerOccurrences(occurrences, code.textContent || '', code, 'element');
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
    (function scanPlainText(node) {
      if (!node) return;
      var type = node.nodeType;
      if (type === Node.TEXT_NODE) {
        var text = node.nodeValue || '';
        if (text) {
          collectFileServerOccurrences(occurrences, text, node, 'text');
          var matches = text.match(HTML_ARTIFACT_CODE_RE);
          if (matches) {
            for (var i = 0; i < matches.length; i++) {
              var ptResolved = resolveHtmlArtifactLink(matches[i]);
              if (!ptResolved) continue;
              if (seen.has(ptResolved.absPath)) continue;
              seen.add(ptResolved.absPath);
              links.push({
                el: makePlainTextArtifactHandle(prose),
                absPath: ptResolved.absPath,
                fetchUrl: ptResolved.fetchUrl,
              });
            }
          }
        }
        return;
      }
      if (type === Node.ELEMENT_NODE) {
        var tag = node.tagName;
        if (tag === 'A' || tag === 'CODE') return;
      }
      var children = node.childNodes;
      if (children) {
        for (var ci = 0; ci < children.length; ci++) scanPlainText(children[ci]);
      }
    })(prose);
    links.forEach(function(link, ordinal) {
      if (findEmbeddedHtmlArtifactCard(prose, link.absPath)) {
        link.el.dataset.embedded = '1';
        return;
      }
      renderArtifactLink(link, ordinal, prose);
    });
    markMissingFileLinks(occurrences);
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
    var container = document.getElementById('messages');
    var wasAtBottom = container ? shouldAutoScroll(container) : false;
    var cap = Math.floor(window.innerHeight * 0.8);
    frame.style.height = Math.min(Number(data.height) + 2, cap) + 'px';
    if (wasAtBottom && container) container.scrollTop = container.scrollHeight;
  });
}
installHtmlArtifactListener();

Chat.embedLinkedHtmlArtifacts = embedLinkedHtmlArtifacts;
Chat.resolveHtmlArtifactLink = resolveHtmlArtifactLink;
Chat.findArtifactLinkInCode = findArtifactLinkInCode;
Chat.toggleHtmlArtifactSource = toggleHtmlArtifactSource;
Chat.startHtmlArtifactResize = startHtmlArtifactResize;
Chat.expandHtmlArtifact = expandHtmlArtifact;
Chat.toggleHtmlArtifactEmbed = toggleHtmlArtifactEmbed;
Chat.expandArtifactCard = expandArtifactCard;
Chat.collapseArtifactCard = collapseArtifactCard;
Chat.fetchHtmlArtifact = fetchHtmlArtifact;
Chat.htmlArtifactFetchCache = htmlArtifactFetchCache;
Chat.expandedArtifactCards = expandedArtifactCards;
Chat.injectLinkBehavior = injectLinkBehavior;
Chat.lookupRegisteredPlanVersion = lookupRegisteredPlanVersion;
Chat.decidePlanCardRender = decidePlanCardRender;
Chat.lookupPlanVersionState = lookupPlanVersionState;
Chat.buildPlanCompactCardHtml = buildPlanCompactCardHtml;
Chat.updatePlanCardBadges = updatePlanCardBadges;
Chat.openPlanFromCard = openPlanFromCard;
Chat._planStateLabel = _planStateLabel;
Chat.expose([
  'resolveHtmlArtifactLink',
  'findArtifactLinkInCode',
  'toggleHtmlArtifactSource',
  'startHtmlArtifactResize',
  'expandHtmlArtifact',
  'toggleHtmlArtifactEmbed',
  'injectLinkBehavior',
  'lookupRegisteredPlanVersion',
  'decidePlanCardRender',
  'lookupPlanVersionState',
  'buildPlanCompactCardHtml',
  'updatePlanCardBadges',
  'openPlanFromCard',
  '_planStateLabel',
]);

})();
