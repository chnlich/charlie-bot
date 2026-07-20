
(function() {
  const Chat = globalThis.Chat;

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

function buildHtmlArtifactCard(opts) {
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

// ---------------------------------------------------------------------------
// Plan compact cards — registered plan-version links render as native cards.
// Pure helpers (exported for testing) live below; the DOM render path is in
// embedLinkedHtmlArtifacts. The registry snapshot is owned by the plan panel
// module (single source of truth, no client-side state derivation).
// ---------------------------------------------------------------------------

function buildSessionDir(sessionId, userHome) {
  var home = userHome;
  if (home == null && typeof window !== 'undefined' && window.USER_HOME) home = window.USER_HOME;
  if (!home) return '';
  return home + '/.charliebot/sessions/' + sessionId;
}

function lookupRegisteredPlanVersion(snapshot, absPath, sessionId, userHome) {
  var plans = (snapshot && snapshot.plans) || [];
  var sessionDir = buildSessionDir(sessionId, userHome);
  if (!sessionDir) return null;
  for (var i = 0; i < plans.length; i++) {
    var plan = plans[i];
    var versions = (plan && plan.versions) || [];
    for (var j = 0; j < versions.length; j++) {
      var ver = versions[j];
      if (!ver || !ver.file) continue;
      var expected = sessionDir + '/' + ver.file;
      if (absPath === expected) {
        return {planId: plan.id, v: ver.v, title: plan.title, state: plan.state, file: ver.file};
      }
    }
  }
  return null;
}

function decidePlanCardRender(snapshot, absPath, sessionId, userHome) {
  return lookupRegisteredPlanVersion(snapshot, absPath, sessionId, userHome) ? 'compact' : 'legacy';
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
        return {planId: plan.id, v: ver.v, title: plan.title, state: plan.state, file: ver.file};
      }
    }
  }
  return null;
}

function buildPlanCompactCardHtml(planId, v, title, state, absPath) {
  return '<div class="plan-compact-card html-artifact" data-artifact-path="' + escapeHtml(absPath) + '"'
    + ' data-plan-card-plan="' + escapeHtml(planId) + '"'
    + ' data-plan-card-version="' + escapeHtml(v) + '"'
    + ' data-plan-card-abs-path="' + escapeHtml(absPath) + '">'
    + '<div class="html-artifact-toolbar">'
    + '<span class="filename">' + escapeHtml(title || '(untitled)') + '</span>'
    + '<span class="plan-compact-version">v' + escapeHtml(v) + '</span>'
    + '<span class="plan-compact-state">' + escapeHtml(state || '') + '</span>'
    + '<button type="button" onclick="openPlanFromCard(this)">Open panel</button>'
    + '</div>'
    + '</div>';
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

function renderPlanCompactCard(link, ordinal, prose, reg) {
  var template = document.createElement('template');
  template.innerHTML = buildPlanCompactCardHtml(reg.planId, reg.v, reg.title, reg.state, link.absPath);
  var card = template.content.firstElementChild;
  if (!card) {
    throw new Error('plan compact card render produced no element');
  }
  var mc = document.getElementById('messages');
  var atBottom = mc ? shouldAutoScroll(mc) : false;
  insertHtmlArtifactCard(prose, card, ordinal);
  link.el.dataset.embedded = '1';
  if (atBottom && mc) mc.scrollTop = mc.scrollHeight;
}

function renderLegacyArtifactEmbed(link, ordinal) {
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
    var mc = document.getElementById('messages');
    var atBottom = mc ? shouldAutoScroll(mc) : false;
    insertHtmlArtifactCard(currentProse, card, ordinal);
    link.el.dataset.embedded = '1';
    if (atBottom && mc) mc.scrollTop = mc.scrollHeight;
  }).catch(function(e) {
    console.warn('Failed to render linked HTML artifact', link.fetchUrl, e);
  });
}

function renderArtifactLink(link, ordinal, prose) {
  var ready = _planRegistryReady();
  if (!ready) {
    renderLegacyArtifactEmbed(link, ordinal);
    return;
  }
  ready.then(function() {
    if (!link.el.isConnected) return;
    if (link.el.dataset.embedded === '1') return;
    var currentProse = link.el.closest('.prose-msg');
    if (!currentProse || currentProse.closest('#streaming-msg')) return;
    if (findEmbeddedHtmlArtifactCard(currentProse, link.absPath)) {
      link.el.dataset.embedded = '1';
      return;
    }
    var snapshot = _planRegistrySnapshot();
    var reg = snapshot ? lookupRegisteredPlanVersion(snapshot, link.absPath, SESSION_ID, window.USER_HOME) : null;
    if (reg) {
      renderPlanCompactCard(link, ordinal, currentProse, reg);
    } else {
      renderLegacyArtifactEmbed(link, ordinal);
    }
  }).catch(function(e) {
    console.warn('plan registry readiness failed; using legacy embed', e);
    renderLegacyArtifactEmbed(link, ordinal);
  });
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
      renderArtifactLink(link, ordinal, prose);
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
Chat.injectLinkBehavior = injectLinkBehavior;
Chat.lookupRegisteredPlanVersion = lookupRegisteredPlanVersion;
Chat.decidePlanCardRender = decidePlanCardRender;
Chat.lookupPlanVersionState = lookupPlanVersionState;
Chat.buildPlanCompactCardHtml = buildPlanCompactCardHtml;
Chat.buildSessionDir = buildSessionDir;
Chat.updatePlanCardBadges = updatePlanCardBadges;
Chat.openPlanFromCard = openPlanFromCard;
Chat.expose([
  'resolveHtmlArtifactLink',
  'findArtifactLinkInCode',
  'toggleHtmlArtifactSource',
  'startHtmlArtifactResize',
  'expandHtmlArtifact',
  'injectLinkBehavior',
  'lookupRegisteredPlanVersion',
  'decidePlanCardRender',
  'lookupPlanVersionState',
  'buildPlanCompactCardHtml',
  'buildSessionDir',
  'updatePlanCardBadges',
  'openPlanFromCard',
]);

})();
