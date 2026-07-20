var framed;
try { framed = window.parent !== window.self; } catch (e) { framed = true; }

if (!framed) {
  (function() {
    var GLOBAL_PREFIX = '__cbc';
    var HIDE_DELAY_MS = 300;
    var TRIGGER_SIZE = 34;
    var POPOVER_WIDTH = 460;
    var AUTH_MESSAGE = 'log in to comment';
    var SECTION_SELECTOR = 'section';
    var SHORTCUTS = [{label: 'Improve', prompt: 'Think from scratch, how to improve this?'}];
    var pathSessionId = extractSessionIdFromPath(window.location.pathname);
    var hashSessionId = extractSessionIdFromHash(window.location.hash);
    var sessionId = resolveSessionIdFromLocation(window.location.pathname, window.location.hash);
    var artifactPath = artifactPathFromPath(window.location.pathname);
    var sessionNameCache = {};
    var sessionNameRequests = {};
    var hovered = null;
    var hideTimer = null;
    var trigger = null;
    var popover = null;
    var toast = null;
    var pending = [];
    var tray = null;
    var trayHeader = null;
    var trayList = null;
    var traySendBtn = null;
    var trayClearBtn = null;
    var trayReason = null;

    window.__cbcExtractSessionIdFromPath = extractSessionIdFromPath;
    window.__cbcResolveSessionId = resolveSessionIdFromLocation;
    window.__cbcBuildBatchMessage = buildBatchMessage;
    window.__cbcResolveEntryDraft = resolveEntryDraft;
    window.__cbcBuildTrayItem = buildTrayItem;
    window.__cbcFindBlock = findBlock;

    installStyles();
    installListeners();
    installShortcuts();
    installBatchTray();

    function extractSessionIdFromPath(pathname) {
      var normalized = String(pathname || '').replace(/%2F/gi, '/');
      var match = normalized.match(/(?:^|\/)sessions\/([^/]+)\/artifacts(?:\/|$)/);
      if (!match) return null;
      var value = decodeURIComponent(match[1]).trim();
      return value || null;
    }

    function extractSessionIdFromHash(hash) {
      var text = String(hash || '');
      var prefix = '#cbsession=';
      if (text.indexOf(prefix) !== 0) return null;
      var value;
      try {
        value = decodeURIComponent(text.slice(prefix.length)).trim();
      } catch (e) {
        console.warn('Invalid cbsession hash', hash, e);
        return null;
      }
      if (!value || value.indexOf('/') !== -1) return null;
      return value;
    }

    function resolveSessionIdFromLocation(pathname, hash) {
      return extractSessionIdFromHash(hash) || extractSessionIdFromPath(pathname);
    }

    function artifactPathFromPath(pathname) {
      return decodeURIComponent(String(pathname || '').replace(/%2F/gi, '/'));
    }

    function fallbackSessionLabel(id) {
      return String(id || '').slice(0, 8);
    }

    function targetSessionLabel() {
      if (!sessionId) return '';
      return sessionNameCache[sessionId] || fallbackSessionLabel(sessionId);
    }

    function targetSessionSuffix() {
      var label = targetSessionLabel();
      return label ? ' \u2192 ' + label : '';
    }

    function fetchSessionName(id) {
      if (sessionNameCache[id]) return Promise.resolve(sessionNameCache[id]);
      if (sessionNameRequests[id]) return sessionNameRequests[id];

      var request = fetch('/api/sessions/' + encodeURIComponent(id), {
        credentials: 'same-origin',
      }).then(function(response) {
        if (response.status === 404) {
          var notFound = new Error('Session not found: ' + id);
          notFound.status = 404;
          throw notFound;
        }
        if (!response.ok) {
          throw new Error('Session name fetch failed: HTTP ' + response.status);
        }
        return response.json();
      }).then(function(data) {
        var name = data && data.name ? String(data.name).trim() : '';
        sessionNameCache[id] = name || fallbackSessionLabel(id);
        delete sessionNameRequests[id];
        return sessionNameCache[id];
      }).catch(function(err) {
        delete sessionNameRequests[id];
        throw err;
      });

      sessionNameRequests[id] = request;
      return request;
    }

    function ensureTargetSessionName() {
      if (!sessionId || sessionNameCache[sessionId] || sessionNameRequests[sessionId]) return;
      var requestedId = sessionId;
      fetchSessionName(requestedId).then(function() {
        if (sessionId === requestedId) refreshTray();
      }).catch(function(err) {
        if (err.status === 404 && hashSessionId && requestedId === hashSessionId && pathSessionId && pathSessionId !== requestedId) {
          console.warn('Artifact comment target session not found; falling back to path session:', requestedId);
          sessionId = pathSessionId;
          hashSessionId = null;
          ensureTargetSessionName();
          refreshTray();
          return;
        }
        console.error('Artifact comment session name fetch failed:', err);
        sessionNameCache[requestedId] = fallbackSessionLabel(requestedId);
        if (sessionId === requestedId) refreshTray();
      });
    }

    function installStyles() {
      var style = document.createElement('style');
      style.textContent =
        '.' + GLOBAL_PREFIX + '-hover{outline:2px solid rgba(88,166,255,.95)!important;outline-offset:2px!important;box-shadow:0 0 0 4px rgba(88,166,255,.16)!important}' +
        '.' + GLOBAL_PREFIX + '-trigger{position:fixed;z-index:2147483646;width:34px;height:34px;border-radius:999px;border:1px solid rgba(88,166,255,.75);background:#0d1117;color:#e6edf3;display:none;align-items:center;justify-content:center;font-size:16px;line-height:1;box-shadow:0 8px 24px rgba(0,0,0,.35);cursor:pointer;padding:0}' +
        '.' + GLOBAL_PREFIX + '-trigger:hover{background:#1c2230;border-color:#58a6ff}' +
        '.' + GLOBAL_PREFIX + '-popover{position:fixed;z-index:2147483647;width:min(' + POPOVER_WIDTH + 'px,calc(100vw - 16px));background:#161b22;color:#e6edf3;border:1px solid #2d3340;border-radius:8px;box-shadow:0 18px 50px rgba(0,0,0,.45);padding:10px;font:13px/1.4 -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif}' +
        '.' + GLOBAL_PREFIX + '-popover textarea{box-sizing:border-box;width:100%;min-height:140px;resize:vertical;background:#0d1117;color:#e6edf3;border:1px solid #2d3340;border-radius:6px;padding:7px 8px;font:13px/1.4 -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;outline:none}' +
        '.' + GLOBAL_PREFIX + '-popover textarea:focus{border-color:#58a6ff;box-shadow:0 0 0 2px rgba(88,166,255,.18)}' +
        '.' + GLOBAL_PREFIX + '-actions{display:flex;justify-content:flex-end;gap:7px;margin-top:8px}' +
        '.' + GLOBAL_PREFIX + '-actions button{border:1px solid #2d3340;border-radius:6px;padding:4px 9px;background:#1c2230;color:#e6edf3;font:12px -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;cursor:pointer}' +
        '.' + GLOBAL_PREFIX + '-actions button:disabled{cursor:not-allowed;opacity:.58}' +
        '.' + GLOBAL_PREFIX + '-add{background:#238636!important;border-color:#2ea043!important;color:#fff!important}' +
        '.' + GLOBAL_PREFIX + '-error{display:none;margin-top:8px;color:#ffb4ab;font-size:12px}' +
        '.' + GLOBAL_PREFIX + '-toast{position:fixed;z-index:2147483647;max-width:min(360px,calc(100vw - 16px));left:50%;bottom:18px;transform:translateX(-50%);background:#1f6f3a;color:#dfffe5;border:1px solid rgba(63,185,80,.65);border-radius:999px;padding:7px 12px;font:12px -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;box-shadow:0 10px 30px rgba(0,0,0,.35)}' +
        '.' + GLOBAL_PREFIX + '-toast-error{background:#5f2120;color:#ffe2df;border-color:rgba(248,81,73,.7);border-radius:8px}' +
        '.' + GLOBAL_PREFIX + '-shortcuts{position:fixed;right:14px;bottom:64px;z-index:2147483646;display:flex;flex-direction:column;align-items:flex-end;gap:6px;font:12px -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif}' +
        '.' + GLOBAL_PREFIX + '-shortcut{border:1px solid #2ea043;border-radius:7px;padding:7px 10px;background:#238636;color:#fff;font:12px -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;box-shadow:0 10px 30px rgba(0,0,0,.35);cursor:pointer}' +
        '.' + GLOBAL_PREFIX + '-shortcut:hover:not(:disabled){background:#2ea043}' +
        '.' + GLOBAL_PREFIX + '-shortcut:disabled{cursor:not-allowed;opacity:.64;background:#30363d;border-color:#484f58;color:#c9d1d9}' +
        '.' + GLOBAL_PREFIX + '-shortcut-reason{max-width:220px;background:#5f2120;color:#ffe2df;border:1px solid rgba(248,81,73,.7);border-radius:6px;padding:5px 7px;line-height:1.3;box-shadow:0 10px 30px rgba(0,0,0,.35)}' +
        '.' + GLOBAL_PREFIX + '-marked{outline:2px solid rgba(88,166,255,.45)!important;outline-offset:2px!important;box-shadow:0 0 0 4px rgba(88,166,255,.08)!important}' +
        '.' + GLOBAL_PREFIX + '-tray{position:fixed;right:14px;bottom:110px;z-index:2147483646;width:min(400px,calc(100vw - 28px));background:#161b22;color:#e6edf3;border:1px solid #2d3340;border-radius:8px;box-shadow:0 18px 50px rgba(0,0,0,.45);padding:10px;font:13px/1.4 -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;display:none;flex-direction:column;gap:8px}' +
        '.' + GLOBAL_PREFIX + '-tray-header{font-weight:600;font-size:12px;color:#8b949e}' +
        '.' + GLOBAL_PREFIX + '-tray-list{max-height:320px;overflow:auto;display:flex;flex-direction:column;gap:6px}' +
        '.' + GLOBAL_PREFIX + '-tray-item{background:#0d1117;border:1px solid #2d3340;border-radius:6px;padding:7px;min-height:0}' +
        '.' + GLOBAL_PREFIX + '-tray-item-main{display:flex;align-items:flex-start;gap:7px;min-width:0}' +
        '.' + GLOBAL_PREFIX + '-tray-item-body{flex:1;min-width:0}' +
        '.' + GLOBAL_PREFIX + '-tray-item-controls{flex:0 0 auto;display:flex;flex-direction:column;align-items:flex-end;gap:5px}' +
        '.' + GLOBAL_PREFIX + '-tray-item-quote{color:#8b949e;font-size:11px;font-style:italic;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}' +
        '.' + GLOBAL_PREFIX + '-tray-item-comment{color:#e6edf3;font-size:12px;margin-top:3px;overflow:hidden;text-overflow:ellipsis;white-space:pre-line;overflow-wrap:anywhere;display:-webkit-box;-webkit-line-clamp:2;-webkit-box-orient:vertical;max-height:3.4em;cursor:default}' +
        '.' + GLOBAL_PREFIX + '-tray-edit{box-sizing:border-box;width:100%;min-height:130px;resize:vertical;margin-top:3px;background:#0d1117;color:#e6edf3;border:1px solid #2d3340;border-radius:6px;padding:6px 7px;font:12px/1.4 -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;outline:none}' +
        '.' + GLOBAL_PREFIX + '-tray-edit:focus{border-color:#58a6ff;box-shadow:0 0 0 2px rgba(88,166,255,.18)}' +
        '.' + GLOBAL_PREFIX + '-tray-edit-btn{border:1px solid #2d3340;border-radius:4px;padding:2px 6px;background:#1c2230;color:#e6edf3;font:10px -apple-system,BlinkMacSystemFont,sans-serif;cursor:pointer}' +
        '.' + GLOBAL_PREFIX + '-tray-edit-btn:hover{background:#262d36}' +
        '.' + GLOBAL_PREFIX + '-tray-remove{width:18px;height:18px;border:none;border-radius:4px;background:transparent;color:#8b949e;font:14px/1 -apple-system,BlinkMacSystemFont,sans-serif;cursor:pointer;padding:0;display:flex;align-items:center;justify-content:center}' +
        '.' + GLOBAL_PREFIX + '-tray-remove:hover{background:#21262d;color:#e6edf3}' +
        '.' + GLOBAL_PREFIX + '-tray-reason{background:#5f2120;color:#ffe2df;border:1px solid rgba(248,81,73,.7);border-radius:6px;padding:5px 7px;font-size:11px;line-height:1.3}' +
        '.' + GLOBAL_PREFIX + '-tray-actions{display:flex;justify-content:flex-end;gap:7px}' +
        '.' + GLOBAL_PREFIX + '-tray-send{border:1px solid #2ea043;border-radius:6px;padding:5px 10px;background:#238636;color:#fff;font:12px -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;cursor:pointer}' +
        '.' + GLOBAL_PREFIX + '-tray-send:hover:not(:disabled){background:#2ea043}' +
        '.' + GLOBAL_PREFIX + '-tray-send:disabled{cursor:not-allowed;opacity:.64;background:#30363d;border-color:#484f58;color:#c9d1d9}' +
        '.' + GLOBAL_PREFIX + '-tray-clear{border:1px solid #2d3340;border-radius:6px;padding:5px 10px;background:#1c2230;color:#e6edf3;font:12px -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;cursor:pointer}' +
        '.' + GLOBAL_PREFIX + '-tray-clear:hover{background:#262d36}';
      document.head.appendChild(style);
    }

    function installListeners() {
      document.addEventListener('mouseover', function(event) {
        if (isCommentUi(event.target)) {
          cancelHide();
          return;
        }
        if (popover) return;
        var block = findBlock(event.target);
        if (block) {
          cancelHide();
          setHovered(block);
        }
      });
      document.addEventListener('mouseout', function(event) {
        if (popover) return;
        var related = event.relatedTarget;
        if (isCommentUi(related)) {
          cancelHide();
          return;
        }
        if (hovered && (!related || !hovered.contains(related))) scheduleHide();
      });
      document.addEventListener('click', function(event) {
        if (isCommentUi(event.target) || isInteractiveElement(event.target) || hasActiveTextSelection()) return;
        var block = findBlock(event.target);
        if (block) openPopover(block);
      });
      document.addEventListener('mousemove', function() {
        if (hovered && !popover) positionTrigger(hovered);
      });
      window.addEventListener('scroll', reposition, true);
      window.addEventListener('resize', reposition);
    }

    function textFor(el, cap) {
      return (el.innerText || el.textContent || '').trim().slice(0, cap);
    }

    function cleanText(el, cap) {
      return textFor(el, cap).replace(/\s+/g, ' ');
    }

    function isTextBearing(el) {
      return Boolean(el && textFor(el, 1));
    }

    function isCommentUi(target) {
      return Boolean(target && target.closest && target.closest(
        '.' + GLOBAL_PREFIX + '-trigger,.' + GLOBAL_PREFIX + '-popover,.' + GLOBAL_PREFIX + '-toast,.' + GLOBAL_PREFIX + '-tray'
      ));
    }

    function isInteractiveElement(target) {
      return Boolean(target && target.closest && target.closest('a,button,input,textarea,select,label,summary'));
    }

    function hasActiveTextSelection() {
      var selection = window.getSelection && window.getSelection();
      return Boolean(selection && !selection.isCollapsed);
    }

    function isBlockLevel(el) {
      var display = window.getComputedStyle(el).display;
      return display !== 'none' && display !== 'contents' && display.indexOf('inline') !== 0;
    }

    function hasOwnText(el) {
      var nodes = el.childNodes;
      for (var i = 0; i < nodes.length; i++) {
        var node = nodes[i];
        if (node.nodeType === 3 && /\S/.test(node.textContent)) return true;
      }
      return false;
    }

    function isCommentableBlock(el) {
      if (el.nodeType !== 1 || !isBlockLevel(el)) return false;
      if (hasOwnText(el)) return true;
      if (!isTextBearing(el)) return false;
      var tagName = el.tagName;
      return tagName === 'PRE' || tagName === 'TD' || tagName === 'TH';
    }

    function findBlock(target) {
      var el = target;
      while (el && el !== document.body) {
        if (isCommentableBlock(el)) return el;
        el = el.parentElement;
      }

      el = target && target.closest ? target.closest(SECTION_SELECTOR) : null;
      while (el && !isTextBearing(el)) {
        el = el.parentElement && el.parentElement.closest ? el.parentElement.closest(SECTION_SELECTOR) : null;
      }
      return el;
    }

    function ensureTrigger() {
      if (trigger) return trigger;
      trigger = document.createElement('button');
      trigger.type = 'button';
      trigger.className = GLOBAL_PREFIX + '-trigger';
      trigger.textContent = '\uD83D\uDCAC';
      trigger.title = 'Comment on this block';
      trigger.setAttribute('aria-label', 'Comment on this block');
      trigger.addEventListener('mousedown', function(event) {
        event.preventDefault();
        event.stopPropagation();
      });
      trigger.addEventListener('click', function(event) {
        event.preventDefault();
        event.stopPropagation();
        if (hovered) openPopover(hovered);
      });
      trigger.addEventListener('mouseenter', cancelHide);
      trigger.addEventListener('mouseleave', function() {
        if (!popover) scheduleHide();
      });
      document.body.appendChild(trigger);
      return trigger;
    }

    function positionTrigger(block) {
      var btn = ensureTrigger();
      var rect = block.getBoundingClientRect();
      var left = clamp(rect.right - TRIGGER_SIZE, 8, window.innerWidth - TRIGGER_SIZE - 8);
      var top = clamp(rect.top + 4, 8, window.innerHeight - TRIGGER_SIZE - 8);
      var position = avoidShortcutOverlap(left, top);
      btn.style.left = position.left + 'px';
      btn.style.top = position.top + 'px';
      btn.style.display = 'flex';
    }

    function clamp(value, min, max) {
      return Math.max(min, Math.min(value, max));
    }

    function rectForTrigger(left, top) {
      return {
        left: left,
        top: top,
        right: left + TRIGGER_SIZE,
        bottom: top + TRIGGER_SIZE,
      };
    }

    function rectsIntersect(a, b) {
      return a.left < b.right && a.right > b.left && a.top < b.bottom && a.bottom > b.top;
    }

    function intersectsAnyShortcut(left, top, shortcuts) {
      var triggerRect = rectForTrigger(left, top);
      for (var i = 0; i < shortcuts.length; i++) {
        if (rectsIntersect(triggerRect, shortcuts[i])) return true;
      }
      return false;
    }

    function addCandidate(values, value, min, max) {
      var clamped = clamp(value, min, max);
      if (values.indexOf(clamped) === -1) values.push(clamped);
    }

    function avoidShortcutOverlap(left, top) {
      var shortcutNodes = Array.prototype.slice.call(document.querySelectorAll('.' + GLOBAL_PREFIX + '-shortcuts'));
      var shortcuts = shortcutNodes.map(function(node) { return node.getBoundingClientRect(); });
      if (!intersectsAnyShortcut(left, top, shortcuts)) return {left: left, top: top};

      var minLeft = 8;
      var minTop = 8;
      var maxLeft = window.innerWidth - TRIGGER_SIZE - 8;
      var maxTop = window.innerHeight - TRIGGER_SIZE - 8;
      var lefts = [];
      var tops = [];
      addCandidate(lefts, left, minLeft, maxLeft);
      addCandidate(lefts, minLeft, minLeft, maxLeft);
      addCandidate(lefts, maxLeft, minLeft, maxLeft);
      addCandidate(tops, top, minTop, maxTop);
      addCandidate(tops, minTop, minTop, maxTop);
      addCandidate(tops, maxTop, minTop, maxTop);

      for (var i = 0; i < shortcuts.length; i++) {
        var shortcut = shortcuts[i];
        addCandidate(lefts, shortcut.left - TRIGGER_SIZE - 8, minLeft, maxLeft);
        addCandidate(lefts, shortcut.left - TRIGGER_SIZE, minLeft, maxLeft);
        addCandidate(lefts, shortcut.right, minLeft, maxLeft);
        addCandidate(lefts, shortcut.right + 8, minLeft, maxLeft);
        addCandidate(tops, shortcut.top - TRIGGER_SIZE - 8, minTop, maxTop);
        addCandidate(tops, shortcut.top - TRIGGER_SIZE, minTop, maxTop);
        addCandidate(tops, shortcut.bottom, minTop, maxTop);
        addCandidate(tops, shortcut.bottom + 8, minTop, maxTop);
      }

      var best = null;
      var bestDistance = Infinity;
      for (var x = 0; x < lefts.length; x++) {
        for (var y = 0; y < tops.length; y++) {
          var candidateLeft = lefts[x];
          var candidateTop = tops[y];
          if (intersectsAnyShortcut(candidateLeft, candidateTop, shortcuts)) continue;
          var distance = Math.pow(candidateLeft - left, 2) + Math.pow(candidateTop - top, 2);
          if (distance < bestDistance) {
            bestDistance = distance;
            best = {left: candidateLeft, top: candidateTop};
          }
        }
      }

      if (!best) throw new Error('Cannot position comment trigger without overlapping artifact shortcuts');
      return best;
    }

    function setHovered(block) {
      cancelHide();
      if (hovered === block) {
        positionTrigger(block);
        return;
      }
      clearHover();
      hovered = block;
      hovered.classList.add(GLOBAL_PREFIX + '-hover');
      positionTrigger(block);
    }

    function scheduleHide() {
      cancelHide();
      hideTimer = window.setTimeout(function() {
        hideTimer = null;
        clearHover();
      }, HIDE_DELAY_MS);
    }

    function cancelHide() {
      if (!hideTimer) return;
      window.clearTimeout(hideTimer);
      hideTimer = null;
    }

    function clearHover() {
      cancelHide();
      if (hovered) hovered.classList.remove(GLOBAL_PREFIX + '-hover');
      hovered = null;
      if (trigger) trigger.style.display = 'none';
    }

    function previousHeading(block) {
      var headings = Array.prototype.slice.call(document.querySelectorAll('h1,h2,h3,h4,h5,h6'));
      var last = '';
      for (var i = 0; i < headings.length; i++) {
        var heading = headings[i];
        if (heading === block) break;
        if (heading.compareDocumentPosition(block) & Node.DOCUMENT_POSITION_FOLLOWING) {
          last = cleanText(heading, 200);
        }
      }
      return last;
    }

    function sectionTitle(block) {
      var tag = (block.tagName || '').toLowerCase();
      var section = tag === 'section' ? block : block.closest('section');
      if (!section) return '';
      var title = section.querySelector('h1,h2,h3,h4,h5,h6');
      if (!title || title === block) return '';
      if (block === section || (title.compareDocumentPosition(block) & Node.DOCUMENT_POSITION_FOLLOWING)) {
        return cleanText(title, 200);
      }
      return '';
    }

    function contextFor(block) {
      var tag = (block.tagName || '').toLowerCase();
      if (tag === 'section') return sectionTitle(block) || previousHeading(block);
      return previousHeading(block) || sectionTitle(block);
    }

    function closePopover() {
      if (popover && popover.parentNode) popover.parentNode.removeChild(popover);
      popover = null;
    }

    function positionPopover(node, block) {
      var rect = block.getBoundingClientRect();
      var width = Math.min(POPOVER_WIDTH, window.innerWidth - 16);
      var left = Math.max(8, Math.min(rect.left, window.innerWidth - width - 8));
      var top = Math.max(8, Math.min(rect.bottom + 8, window.innerHeight - node.offsetHeight - 8));
      node.style.left = left + 'px';
      node.style.top = top + 'px';
    }

    function setPopoverError(node, message) {
      var error = node.querySelector('.' + GLOBAL_PREFIX + '-error');
      error.textContent = message;
      error.style.display = 'block';
    }

    function showToast(message, isError) {
      if (toast && toast.parentNode) toast.parentNode.removeChild(toast);
      var node = document.createElement('div');
      toast = node;
      node.className = GLOBAL_PREFIX + '-toast' + (isError ? ' ' + GLOBAL_PREFIX + '-toast-error' : '');
      node.textContent = message;
      document.body.appendChild(node);
      window.setTimeout(function() {
        if (node.parentNode) node.parentNode.removeChild(node);
        if (toast === node) toast = null;
      }, 2200);
    }

    function buildCommentEntry(quote, context, comment) {
      if (quote === '') {
        return ('\u25B8 ' + context).split('\n').concat(('\u21B3 ' + comment).split('\n'));
      }
      var quoteLine = context
        ? '\u25B8 ' + context + ' \u203A "' + quote + '"'
        : '\u25B8 "' + quote + '"';
      return quoteLine.split('\n').concat(('\u21B3 ' + comment).split('\n'));
    }

    function resolveEntryDraft(entry) {
      return buildCommentEntry(entry.quote, entry.context, entry.comment);
    }

    function buildBatchMessage(entries) {
      var lines = [];
      lines.push('[Artifact comments \u00B7 ' + artifactPath + '] (' + entries.length + ')');
      lines.push('');
      for (var i = 0; i < entries.length; i++) {
        var entry = entries[i];
        var entryLines = resolveEntryDraft(entry);
        lines.push((i + 1) + '. ' + entryLines[0]);
        for (var j = 1; j < entryLines.length; j++) {
          lines.push('   ' + entryLines[j]);
        }
        if (i < entries.length - 1) lines.push('');
      }
      return lines.join('\n');
    }

    function installShortcuts() {
      var container = document.createElement('div');
      container.className = GLOBAL_PREFIX + '-shortcuts';

      for (var i = 0; i < SHORTCUTS.length; i++) {
        (function(shortcut) {
          var button = document.createElement('button');
          button.type = 'button';
          button.className = GLOBAL_PREFIX + '-shortcut';
          button.textContent = shortcut.label;
          button.title = shortcut.prompt;
          button.setAttribute('aria-label', shortcut.prompt);
          if (!sessionId) {
            button.disabled = true;
          } else {
            button.addEventListener('click', function() {
              addShortcutComment(shortcut);
            });
          }
          container.appendChild(button);
        })(SHORTCUTS[i]);
      }

      if (!sessionId) {
        var reason = document.createElement('div');
        reason.className = GLOBAL_PREFIX + '-shortcut-reason';
        reason.textContent = 'Cannot parse session id from this artifact URL.';
        container.appendChild(reason);
      }

      document.body.appendChild(container);
    }

    function addShortcutComment(shortcut) {
      for (var i = 0; i < pending.length; i++) {
        if (pending[i].kind === 'improve') return;
      }
      pending.push({
        kind: 'improve',
        el: null,
        quote: '',
        context: shortcut.label,
        comment: shortcut.prompt,
      });
      refreshTray();
    }

    function openPopover(block) {
      cancelHide();
      closePopover();
      if (hovered && hovered !== block) clearHover();
      if (trigger) trigger.style.display = 'none';
      hovered = block;
      hovered.classList.add(GLOBAL_PREFIX + '-hover');

      var node = document.createElement('div');
      node.className = GLOBAL_PREFIX + '-popover';
      var textarea = document.createElement('textarea');
      textarea.placeholder = 'Add a comment (Ctrl+Enter)';
      var error = document.createElement('div');
      error.className = GLOBAL_PREFIX + '-error';
      var actions = document.createElement('div');
      actions.className = GLOBAL_PREFIX + '-actions';
      var addBtn = document.createElement('button');
      addBtn.type = 'button';
      addBtn.className = GLOBAL_PREFIX + '-add';
      addBtn.textContent = 'Add';
      var cancelBtn = document.createElement('button');
      cancelBtn.type = 'button';
      cancelBtn.textContent = 'Cancel';

      actions.appendChild(cancelBtn);
      actions.appendChild(addBtn);
      node.appendChild(textarea);
      node.appendChild(error);
      node.appendChild(actions);
      node.addEventListener('mousedown', function(event) { event.stopPropagation(); });
      node.addEventListener('mouseenter', cancelHide);
      textarea.addEventListener('keydown', function(event) {
        if ((event.ctrlKey || event.metaKey) && event.key === 'Enter' && !addBtn.disabled) {
          event.preventDefault();
          submitComment(block, textarea);
        }
      });
      cancelBtn.addEventListener('click', function() {
        closePopover();
        clearHover();
      });

      if (!sessionId) {
        addBtn.disabled = true;
        setPopoverError(node, 'Cannot parse session id from this artifact URL.');
      } else {
        addBtn.addEventListener('click', function() {
          submitComment(block, textarea);
        });
      }

      document.body.appendChild(node);
      popover = node;
      positionPopover(node, block);
      textarea.focus();
    }

    async function postChatMessage(content) {
      var response = await fetch('/api/chat/' + encodeURIComponent(sessionId) + '/message', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        credentials: 'same-origin',
        body: JSON.stringify({content: content, uploaded_files: []}),
      });
      if (response.status === 401) {
        throw new Error(AUTH_MESSAGE);
      }
      if (!response.ok) {
        throw new Error('Comment post failed: HTTP ' + response.status);
      }
    }

    function submitComment(block, textarea) {
      var comment = textarea.value.trim();
      if (!comment) {
        textarea.focus();
        return;
      }
      pending.push({
        kind: 'block',
        el: block,
        quote: cleanText(block, 400),
        context: contextFor(block),
        comment: comment,
      });
      block.classList.add(GLOBAL_PREFIX + '-marked');
      closePopover();
      clearHover();
      refreshTray();
    }

    function installBatchTray() {
      var container = document.createElement('div');
      container.className = GLOBAL_PREFIX + '-tray';

      var header = document.createElement('div');
      header.className = GLOBAL_PREFIX + '-tray-header';
      header.textContent = 'Pending comments (0)';
      container.appendChild(header);

      var list = document.createElement('div');
      list.className = GLOBAL_PREFIX + '-tray-list';
      container.appendChild(list);

      var reason = document.createElement('div');
      reason.className = GLOBAL_PREFIX + '-tray-reason';
      reason.textContent = 'Cannot parse session id from this artifact URL.';
      container.appendChild(reason);

      var actions = document.createElement('div');
      actions.className = GLOBAL_PREFIX + '-tray-actions';
      var clearBtn = document.createElement('button');
      clearBtn.type = 'button';
      clearBtn.textContent = 'Clear';
      clearBtn.className = GLOBAL_PREFIX + '-tray-clear';
      clearBtn.addEventListener('click', clearAll);
      var sendBtn = document.createElement('button');
      sendBtn.type = 'button';
      sendBtn.textContent = 'Send';
      sendBtn.className = GLOBAL_PREFIX + '-tray-send';
      sendBtn.addEventListener('click', sendBatch);
      actions.appendChild(clearBtn);
      actions.appendChild(sendBtn);
      container.appendChild(actions);

      document.body.appendChild(container);
      tray = container;
      trayHeader = header;
      trayList = list;
      traySendBtn = sendBtn;
      trayClearBtn = clearBtn;
      trayReason = reason;

      refreshTray();
    }

    function buildTrayItem(idx, entry) {
      var item = document.createElement('div');
      item.className = GLOBAL_PREFIX + '-tray-item';

      var main = document.createElement('div');
      main.className = GLOBAL_PREFIX + '-tray-item-main';

      var body = document.createElement('div');
      body.className = GLOBAL_PREFIX + '-tray-item-body';

      var controls = document.createElement('div');
      controls.className = GLOBAL_PREFIX + '-tray-item-controls';

      var quote = document.createElement('div');
      quote.className = GLOBAL_PREFIX + '-tray-item-quote';
      quote.textContent = entry.quote === '' ? entry.context : '\u201C' + entry.quote + '\u201D';

      var draft = document.createElement('div');
      draft.className = GLOBAL_PREFIX + '-tray-item-comment';
      draft.textContent = entry.comment;
      draft.title = 'Comment text';

      var editBtn = document.createElement('button');
      editBtn.type = 'button';
      editBtn.className = GLOBAL_PREFIX + '-tray-edit-btn';
      editBtn.textContent = 'Edit';
      editBtn.title = 'Edit comment';
      editBtn.setAttribute('aria-label', 'Edit comment for this entry');
      editBtn.addEventListener('click', function() {
        editTrayEntry(idx, item);
      });

      var remove = document.createElement('button');
      remove.type = 'button';
      remove.className = GLOBAL_PREFIX + '-tray-remove';
      remove.textContent = '\u00D7';
      remove.title = 'Remove this comment';
      remove.setAttribute('aria-label', 'Remove this comment');
      remove.addEventListener('click', function() {
        removeEntry(idx);
      });

      body.appendChild(quote);
      body.appendChild(draft);
      controls.appendChild(remove);
      controls.appendChild(editBtn);
      main.appendChild(body);
      main.appendChild(controls);
      item.appendChild(main);
      return item;
    }

    function editTrayEntry(idx, itemNode) {
      var entry = pending[idx];
      var draftNode = itemNode.querySelector('.' + GLOBAL_PREFIX + '-tray-item-comment');
      if (!draftNode) return;
      var textarea = document.createElement('textarea');
      textarea.className = GLOBAL_PREFIX + '-tray-edit';
      textarea.value = entry.comment;
      textarea.setAttribute('aria-label', 'Edit comment');
      var done = false;

      function cancel() {
        if (done) return;
        done = true;
        refreshTray();
      }

      function save() {
        if (done) return;
        var next = textarea.value.trim();
        if (!next) {
          cancel();
          return;
        }
        done = true;
        pending[idx].comment = next;
        refreshTray();
      }

      textarea.addEventListener('keydown', function(event) {
        if (event.key === 'Escape') {
          event.preventDefault();
          cancel();
          return;
        }
        if ((event.ctrlKey || event.metaKey) && event.key === 'Enter') {
          event.preventDefault();
          save();
        }
      });
      textarea.addEventListener('blur', save);
      draftNode.parentNode.replaceChild(textarea, draftNode);
      textarea.focus();
      textarea.select();
    }

    function refreshTray() {
      if (!tray) return;
      if (pending.length > 0 && sessionId) ensureTargetSessionName();
      var targetSuffix = targetSessionSuffix();
      trayHeader.textContent = 'Pending comments (' + pending.length + ')' + targetSuffix;
      trayList.innerHTML = '';
      for (var i = 0; i < pending.length; i++) {
        trayList.appendChild(buildTrayItem(i, pending[i]));
      }
      traySendBtn.textContent = 'Send ' + pending.length + targetSuffix;
      tray.style.display = pending.length > 0 ? 'flex' : 'none';
      if (!sessionId) {
        traySendBtn.disabled = true;
        trayReason.style.display = 'block';
      } else {
        traySendBtn.disabled = false;
        trayReason.style.display = 'none';
      }
    }

    function removeEntry(idx) {
      if (idx < 0 || idx >= pending.length) return;
      var removed = pending.splice(idx, 1)[0];
      if (removed.kind === 'block') {
        var stillMarked = false;
        for (var i = 0; i < pending.length; i++) {
          if (pending[i].kind === 'block' && pending[i].el === removed.el) { stillMarked = true; break; }
        }
        if (!stillMarked) removed.el.classList.remove(GLOBAL_PREFIX + '-marked');
      }
      refreshTray();
    }

    function clearAll() {
      for (var i = 0; i < pending.length; i++) {
        if (pending[i].kind === 'block') pending[i].el.classList.remove(GLOBAL_PREFIX + '-marked');
      }
      pending = [];
      refreshTray();
    }

    async function sendBatch() {
      if (!sessionId || pending.length === 0) return;
      var count = pending.length;
      var ordered = pending.slice();
      ordered.sort(function(a, b) {
        if (a.kind === 'improve' && b.kind === 'block') return -1;
        if (a.kind === 'block' && b.kind === 'improve') return 1;
        if (a.kind === 'improve' && b.kind === 'improve') return 0;
        var mask = a.el.compareDocumentPosition(b.el);
        if (mask & Node.DOCUMENT_POSITION_FOLLOWING) return -1;
        if (mask & Node.DOCUMENT_POSITION_PRECEDING) return 1;
        return 0;
      });
      var sendBtn = traySendBtn;
      var prevLabel = sendBtn.textContent;
      sendBtn.disabled = true;
      sendBtn.textContent = 'Sending';
      try {
        await postChatMessage(buildBatchMessage(ordered));
        for (var i = 0; i < pending.length; i++) {
          if (pending[i].kind === 'block') pending[i].el.classList.remove(GLOBAL_PREFIX + '-marked');
        }
        pending = [];
        refreshTray();
        showToast(count + ' comments sent to chat', false);
      } catch (err) {
        console.error('Batch comment send failed:', err);
        showToast(err.message, true);
        sendBtn.disabled = false;
        sendBtn.textContent = prevLabel;
      }
    }

    function reposition() {
      if (!hovered) return;
      if (popover) positionPopover(popover, hovered);
      else positionTrigger(hovered);
    }
  })();
}
