// Marker `cbpanel` is the single URL-fragment marker shared with plan-panel.js.
// When the plan panel loads an artifact in its same-origin iframe, it appends
// `&cbpanel=1` to the #cbsession= fragment. This guard activates the comment
// tray inside that iframe. Keep this literal in sync with plan-panel.js.
var CB_PLAN_PANEL_MARKER = 'cbpanel';
var framed;
try { framed = window.parent !== window.self; } catch (e) { framed = true; }

function _hasPanelReviewMarker(hash) {
  var text = String(hash || '');
  return text.indexOf(CB_PLAN_PANEL_MARKER + '=1') !== -1;
}

if (!framed || _hasPanelReviewMarker(window.location.hash)) {
  (function() {
    var GLOBAL_PREFIX = '__cbc';
    var UI_CLASS = GLOBAL_PREFIX + '-ui';
    var HIDE_DELAY_MS = 300;
    var TRIGGER_SIZE = 34;
    var POPOVER_WIDTH = 460;
    var GUTTER_THRESHOLD = 900;
    // The column also needs vertical room. Gated on the window, never on how many
    // comments are pending: the action bar's own list is capped at 320px, so the bar
    // has a height ceiling (522px measured), and band = innerHeight - 72 - barHeight
    // stays positive at 600px even in that worst case; the usual bar leaves 326px.
    var GUTTER_MIN_HEIGHT = 600;
    // Properties that can make body or documentElement the containing block for the
    // injected layer's fixed boxes. Declared with the other constants: installBatchTray
    // runs during this same IIFE, long before the function that reads them is defined.
    var CB_PROPS = ['transform', 'translate', 'rotate', 'scale', 'perspective',
                    'filter', 'backdropFilter'];
    var GUTTER_GAP = 8;
    var COL_MAX = 300;
    var COL_MIN = 240;
    var AUTH_MESSAGE = 'log in to comment';
    var SECTION_SELECTOR = 'section';
    // Each shortcut owns a `kind`, which doubles as its dedup key and as the
    // persisted draft discriminator. Manual block comments keep kind 'block';
    // adding a button here needs no other change.
    var SHORTCUTS = [
      {
        kind: 'improve',
        label: 'Improve',
        prompt: 'Think from scratch, how to improve this?',
      },
      {
        kind: 'shorten',
        label: 'Shorten',
        prompt: 'This plan is too long. Rewrite it to state the key points concretely; ' +
          'delete anything that is not a key point or demote it into the Other Details list.',
      },
      {
        kind: 'verify',
        label: 'Verify',
        prompt: 'Delegate a fresh verify worker for this plan and report its findings.',
      },
    ];
    // Session identity has exactly one owner: the server injects the resolved id into
    // the page, and the URL path is never consulted. Only the #cbsession= hash (a
    // deliberate cross-session viewing link) overrides it.
    var serverSessionId = (typeof window.__cbcServerSessionId === 'string' &&
      window.__cbcServerSessionId.trim()) || null;
    var hashSessionId = extractSessionIdFromHash(window.location.hash);
    var sessionId = resolveSessionId(window.location.hash);
    var artifactPath = artifactPathFromPath(window.location.pathname);
    var sessionNameCache = {};
    var sessionNameRequests = {};
    var hovered = null;
    var hideTimer = null;
    var trigger = null;
    var dock = null;
    var popover = null;
    var toast = null;
    var pending = [];
    var tray = null;
    var trayHeader = null;
    var trayList = null;
    var traySendBtn = null;
    var trayClearBtn = null;
    var trayReason = null;
    var gutter = null;
    var active = false;
    var prevDockLeft = '';
    var prevDockRight = '';
    var prevDockWidth = '';
    var prevTrayWidth = '';
    var prevGutterLeft = '';
    var prevGutterWidth = '';
    var prevGutterBottom = '';
    var column = 0;
    var columnLeft = 0;
    var anchoredCards = [];
    var docTops = null;
    var bandHeight = 0;
    var refEl = null;
    var refDocTop = 0;
    var cardHeights = null;
    var reflowScheduled = false;

    window.__cbcResolveSessionId = resolveSessionId;
    window.__cbcBuildBatchMessage = buildBatchMessage;
    window.__cbcResolveEntryDraft = resolveEntryDraft;
    window.__cbcBuildTrayItem = buildTrayItem;
    window.__cbcFindBlock = findBlock;
    window.__cbcDraftKey = draftKey;
    window.__cbcSerializeDraft = serializeDraft;
    window.__cbcDeserializeDraft = deserializeDraft;
    window.__cbcSaveDraft = saveDraft;
    window.__cbcLoadDraft = loadDraft;
    window.__cbcClearDraft = clearDraft;
    window.__cbcStackCards = stackCards;
    window.__cbcFitColumn = fitColumn;
    window.__cbcChooseWidth = chooseWidth;
    window.__cbcGutterGap = GUTTER_GAP;
    window.__cbcReanchor = reanchorPending;

    installStyles();
    installListeners();
    installShortcuts();
    installBatchTray();

    function extractSessionIdFromHash(hash) {
      var text = String(hash || '');
      var prefix = '#cbsession=';
      if (text.indexOf(prefix) !== 0) return null;
      var rest = text.slice(prefix.length);
      var ampIdx = rest.indexOf('&');
      var value;
      try {
        value = decodeURIComponent(ampIdx === -1 ? rest : rest.slice(0, ampIdx)).trim();
      } catch (e) {
        console.warn('Invalid cbsession hash', hash, e);
        return null;
      }
      if (!value || value.indexOf('/') !== -1) return null;
      return value;
    }

    function resolveSessionId(hash) {
      return extractSessionIdFromHash(hash) || serverSessionId;
    }

    function artifactPathFromPath(pathname) {
      return decodeURIComponent(String(pathname || '').replace(/%2F/gi, '/'));
    }

    function draftKey(absPath) {
      return 'cbc-draft:' + String(absPath || '');
    }

    function isShortcutKind(kind) {
      for (var i = 0; i < SHORTCUTS.length; i++) {
        if (SHORTCUTS[i].kind === kind) return true;
      }
      return false;
    }

    function normalizeKind(kind) {
      return isShortcutKind(kind) ? kind : 'block';
    }

    function serializeDraft(entries) {
      var out = [];
      for (var i = 0; i < entries.length; i++) {
        var entry = entries[i] || {};
        out.push({
          kind: normalizeKind(entry.kind),
          quote: String(entry.quote == null ? '' : entry.quote),
          context: String(entry.context == null ? '' : entry.context),
          comment: String(entry.comment == null ? '' : entry.comment),
        });
      }
      return out;
    }

    function deserializeDraft(stored) {
      if (stored == null) return [];
      var list;
      try {
        list = JSON.parse(stored);
      } catch (e) {
        return [];
      }
      if (!list || typeof list.length !== 'number') return [];
      var out = [];
      for (var i = 0; i < list.length; i++) {
        var item = list[i];
        if (!item || typeof item !== 'object') continue;
        out.push({
          kind: normalizeKind(item.kind),
          el: null,
          quote: String(item.quote == null ? '' : item.quote),
          context: String(item.context == null ? '' : item.context),
          comment: String(item.comment == null ? '' : item.comment),
        });
      }
      return out;
    }

    function saveDraft(entries, absPath) {
      try {
        var data = JSON.stringify(serializeDraft(entries));
        sessionStorage.setItem(draftKey(absPath), data);
      } catch (e) {
        // sessionStorage unavailable or quota exceeded — degrade silently
      }
    }

    function loadDraft(absPath) {
      try {
        return deserializeDraft(sessionStorage.getItem(draftKey(absPath)));
      } catch (e) {
        return [];
      }
    }

    function clearDraft(absPath) {
      try {
        sessionStorage.removeItem(draftKey(absPath));
      } catch (e) {
        // sessionStorage unavailable — degrade silently
      }
    }

    function fallbackSessionLabel(id) {
      return String(id || '').slice(0, 8);
    }

    // When embedded in the plan panel (framed), prefer the live active session
    // from the parent window so the tray always targets the session currently
    // active in the app — not the session baked into the artifact URL when the
    // iframe was built. Degrades silently to the artifact-URL-derived session id
    // when not framed or when the parent accessor is unavailable or throws.
    function resolveTargetSessionId() {
      if (framed) {
        try {
          var live = window.parent && window.parent.planPanel &&
            window.parent.planPanel.currentSessionId();
          if (live) return live;
        } catch (e) {
          // Cross-window access unavailable — fall back below.
        }
      }
      return sessionId;
    }

    function targetSessionLabel() {
      var id = resolveTargetSessionId();
      if (!id) return '';
      return sessionNameCache[id] || fallbackSessionLabel(id);
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
      var targetId = resolveTargetSessionId();
      if (!targetId || sessionNameCache[targetId] || sessionNameRequests[targetId]) return;
      var requestedId = targetId;
      fetchSessionName(requestedId).then(function() {
        if (resolveTargetSessionId() === requestedId) refreshTray();
      }).catch(function(err) {
        // The 404-driven fallback reassignment (sessionId = serverSessionId) is
        // the base case for standalone mode and when the live accessor is
        // unavailable. When framed and the parent supplies a truthy live
        // session, requestedId is that live id (not the artifact-URL hash
        // session), so the `requestedId === hashSessionId` guard below keeps
        // this branch from clobbering the live session.
        if (err.status === 404 && hashSessionId && requestedId === hashSessionId && serverSessionId && serverSessionId !== requestedId) {
          console.warn('Artifact comment target session not found; falling back to server session:', requestedId);
          sessionId = serverSessionId;
          hashSessionId = null;
          ensureTargetSessionName();
          refreshTray();
          return;
        }
        console.error('Artifact comment session name fetch failed:', err);
        sessionNameCache[requestedId] = fallbackSessionLabel(requestedId);
        if (resolveTargetSessionId() === requestedId) refreshTray();
      });
    }

    function installStyles() {
      var style = document.createElement('style');
      style.textContent =
        '.' + GLOBAL_PREFIX + '-hover{outline:2px solid rgba(88,166,255,.95)!important;outline-offset:2px!important;box-shadow:0 0 0 4px rgba(88,166,255,.16)!important}' +
        '.' + GLOBAL_PREFIX + '-trigger{position:fixed;z-index:2147483645;width:34px;height:34px;border-radius:999px;border:1px solid rgba(88,166,255,.75);background:#0d1117;color:#e6edf3;display:none;align-items:center;justify-content:center;font-size:16px;line-height:1;box-shadow:0 8px 24px rgba(0,0,0,.35);cursor:pointer;padding:0}' +
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
        '.' + GLOBAL_PREFIX + '-dock{position:fixed;right:14px;bottom:64px;z-index:2147483646;display:flex;flex-direction:column-reverse;align-items:flex-end;gap:8px}' +
        '.' + GLOBAL_PREFIX + '-shortcuts{display:flex;flex-wrap:wrap;justify-content:flex-end;gap:6px;font:12px -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif}' +
        '.' + GLOBAL_PREFIX + '-shortcut{border:1px solid #2ea043;border-radius:7px;padding:7px 10px;background:#238636;color:#fff;font:12px -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;box-shadow:0 10px 30px rgba(0,0,0,.35);cursor:pointer}' +
        '.' + GLOBAL_PREFIX + '-shortcut:hover:not(:disabled){background:#2ea043}' +
        '.' + GLOBAL_PREFIX + '-shortcut:disabled{cursor:not-allowed;opacity:.64;background:#30363d;border-color:#484f58;color:#c9d1d9}' +
        '.' + GLOBAL_PREFIX + '-shortcut-reason{flex-basis:100%;max-width:220px;background:#5f2120;color:#ffe2df;border:1px solid rgba(248,81,73,.7);border-radius:6px;padding:5px 7px;line-height:1.3;box-shadow:0 10px 30px rgba(0,0,0,.35)}' +
        '.' + GLOBAL_PREFIX + '-marked{outline:2px solid rgba(88,166,255,.45)!important;outline-offset:2px!important;box-shadow:0 0 0 4px rgba(88,166,255,.08)!important}' +
        '.' + GLOBAL_PREFIX + '-card-hover{outline:3px solid rgba(255,196,0,.9)!important;outline-offset:3px!important;box-shadow:0 0 0 5px rgba(255,196,0,.18)!important}' +
        '.' + GLOBAL_PREFIX + '-tray{width:min(400px,calc(100vw - 28px));background:#161b22;color:#e6edf3;border:1px solid #2d3340;border-radius:8px;box-shadow:0 18px 50px rgba(0,0,0,.45);padding:10px;font:13px/1.4 -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;display:none;flex-direction:column;gap:8px}' +
        '.' + GLOBAL_PREFIX + '-tray-header{font-weight:600;font-size:12px;color:#8b949e}' +
        '.' + GLOBAL_PREFIX + '-tray-list{max-height:320px;overflow:auto;display:flex;flex-direction:column;gap:6px}' +
        '.' + GLOBAL_PREFIX + '-tray-item{background:#0d1117;border:1px solid #2d3340;border-radius:6px;padding:7px;min-height:0;flex-shrink:0}' +
        '.' + GLOBAL_PREFIX + '-tray-item-main{display:flex;align-items:flex-start;gap:7px;min-width:0}' +
        '.' + GLOBAL_PREFIX + '-tray-item-body{flex:1;min-width:0}' +
        '.' + GLOBAL_PREFIX + '-tray-item-controls{flex:0 0 auto;display:flex;flex-direction:column;align-items:flex-end;gap:5px}' +
        '.' + GLOBAL_PREFIX + '-tray-item-quote{color:#8b949e;font-size:11px;font-style:italic;overflow-wrap:anywhere}' +
        '.' + GLOBAL_PREFIX + '-tray-item-comment{color:#e6edf3;font-size:12px;margin-top:3px;white-space:pre-line;overflow-wrap:anywhere;cursor:default}' +
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
        '.' + GLOBAL_PREFIX + '-tray-clear:hover{background:#262d36}' +
        '.' + GLOBAL_PREFIX + '-gutter{position:fixed;top:0;left:0;width:300px;overflow-x:hidden;'
          + 'overflow-y:hidden;z-index:2147483644}' +
        '.' + GLOBAL_PREFIX + '-gutter .' + GLOBAL_PREFIX + '-tray-item{position:absolute;left:0;right:0;box-sizing:border-box}';
      document.head.appendChild(style);
    }

    function installListeners() {
      document.addEventListener('mouseover', function(event) {
        if (isLayerNode(event.target)) {
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
        if (isLayerNode(related)) {
          cancelHide();
          return;
        }
        if (hovered && (!related || !hovered.contains(related))) scheduleHide();
      });
      document.addEventListener('click', function(event) {
        if (isLayerNode(event.target) || isInteractiveElement(event.target) || hasActiveTextSelection()) return;
        var block = findBlock(event.target);
        if (block) openPopover(block);
      });
      document.addEventListener('mousemove', function() {
        if (hovered && !popover) positionTrigger(hovered);
      });
      window.addEventListener('scroll', reposition, true);
      window.addEventListener('resize', reposition);
      window.addEventListener('resize', scheduleReflow);
      document.addEventListener('toggle', scheduleReflow, true);
      document.addEventListener('load', scheduleReflow, true);
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

    function isLayerNode(el) {
      while (el) {
        if (el.classList && el.classList.contains(UI_CLASS)) {
          return true;
        }
        el = el.parentElement;
      }
      return false;
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

    // Unique path for the layer's root nodes. Marking happens here so anything
    // the layer inserts into document.body carries the ownership class, and any
    // root that forgets it must have bypassed this entry point.
    function injectRoot(node) {
      node.classList.add(UI_CLASS);
      document.body.appendChild(node);
      return node;
    }

    function findBlock(target) {
      if (isLayerNode(target)) return null;
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
      injectRoot(trigger);
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

    // Pure greedy placement for the anchored comment gutter (part 2 wires it
    // to the DOM). Sorts cards by anchor ascending, then places each at
    // max(anchor, prevBottom + gap). Returns tops in ascending-anchor order.
    // No DOM access, no module state.
    function stackCards(anchors, heights, gap) {
      var n = anchors.length;
      if (n === 0) return [];
      var order = [];
      for (var i = 0; i < n; i++) order.push(i);
      order.sort(function(a, b) { return anchors[a] - anchors[b]; });
      var tops = [];
      var prevBottom = -Infinity;
      for (var k = 0; k < n; k++) {
        var idx = order[k];
        var top = Math.max(anchors[idx], prevBottom + gap);
        tops.push(top);
        prevBottom = top + heights[idx];
      }
      return tops;
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
      injectRoot(node);
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

    function ensureDock() {
      if (dock) return dock;
      var node = document.createElement('div');
      node.className = GLOBAL_PREFIX + '-dock';
      injectRoot(node);
      dock = node;
      return dock;
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

      ensureDock().appendChild(container);
    }

    function addShortcutComment(shortcut) {
      for (var i = 0; i < pending.length; i++) {
        if (pending[i].kind === shortcut.kind) return;
      }
      pending.push({
        kind: shortcut.kind,
        el: null,
        quote: '',
        context: shortcut.label,
        comment: shortcut.prompt,
      });
      saveDraft(pending, artifactPath);
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

      injectRoot(node);
      popover = node;
      positionPopover(node, block);
      textarea.focus();
    }

    async function postChatMessage(content) {
      var targetId = resolveTargetSessionId();
      var response = await fetch('/api/chat/' + encodeURIComponent(targetId) + '/message', {
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
      saveDraft(pending, artifactPath);
      refreshTray();
    }

    function collectCommentableBlocks() {
      var blocks = [];
      function walk(el) {
        if (isLayerNode(el)) return;
        if (isCommentableBlock(el)) blocks.push(el);
        if (el.children) {
          for (var i = 0; i < el.children.length; i++) {
            walk(el.children[i]);
          }
        }
      }
      if (document.body && document.body.children) {
        for (var i = 0; i < document.body.children.length; i++) {
          walk(document.body.children[i]);
        }
      }
      return blocks;
    }

    function reanchorPending() {
      var blocks = collectCommentableBlocks();
      for (var i = 0; i < pending.length; i++) {
        var entry = pending[i];
        if (entry.el) continue;
        if (entry.quote === '') continue;
        for (var j = 0; j < blocks.length; j++) {
          if (cleanText(blocks[j], 400) === entry.quote) {
            entry.el = blocks[j];
            blocks[j].classList.add(GLOBAL_PREFIX + '-marked');
            break;
          }
        }
      }
    }

    function installBatchTray() {
      pending = loadDraft(artifactPath);
      reanchorPending();
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

      ensureDock().appendChild(container);
      tray = container;
      trayHeader = header;
      trayList = list;
      traySendBtn = sendBtn;
      trayClearBtn = clearBtn;
      trayReason = reason;

      placeColumn();
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
      item.addEventListener('mouseenter', function() {
        if (entry.el) entry.el.classList.add(GLOBAL_PREFIX + '-card-hover');
      });
      item.addEventListener('mouseleave', function() {
        if (entry.el) entry.el.classList.remove(GLOBAL_PREFIX + '-card-hover');
      });
      item.addEventListener('click', function(e) {
        if (e.target && e.target.closest && e.target.closest('.' + GLOBAL_PREFIX + '-tray-remove,.' + GLOBAL_PREFIX + '-tray-edit-btn')) return;
        if (entry.el && entry.el.scrollIntoView) entry.el.scrollIntoView({block: 'center'});
        e.stopPropagation();
        e.preventDefault();
      });
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
        saveDraft(pending, artifactPath);
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
      traySendBtn.textContent = 'Send ' + pending.length + targetSuffix;
      tray.style.display = pending.length > 0 ? 'flex' : 'none';
      if (!sessionId) {
        traySendBtn.disabled = true;
        trayReason.style.display = 'block';
      } else {
        traySendBtn.disabled = false;
        trayReason.style.display = 'none';
      }
      if (gutterActive()) {
        scheduleReflow();
      } else {
        presentCorner();
      }
    }

    // The column exists whenever the reserve does, so the action bar sits at its
    // bottom from first paint and never hops between the corner and the column.
    function gutterActive() {
      return column > 0;
    }

    function scheduleReflow() {
      if (reflowScheduled) return;
      reflowScheduled = true;
      window.requestAnimationFrame(reflowGutter);
    }

    function reflowGutter() {
      reflowScheduled = false;
      placeColumn();
      if (gutterActive()) {
        presentGutter();
      } else {
        presentCorner();
      }
    }

    function presentCorner() {
      if (gutter) {
        gutter.innerHTML = '';
        gutter.style.display = 'none';
      }
      trayList.innerHTML = '';
      for (var i = 0; i < pending.length; i++) {
        trayList.appendChild(buildTrayItem(i, pending[i]));
      }
    }

    function presentGutter() {
      if (!gutter) {
        gutter = document.createElement('div');
        gutter.className = GLOBAL_PREFIX + '-gutter';
        injectRoot(gutter);
        prevGutterLeft = gutter.style.left;
        prevGutterWidth = gutter.style.width;
        prevGutterBottom = gutter.style.bottom;
      }
      gutter.style.display = '';
      // The column exists now, so placeColumn() can read its geometry back. A refusal
      // here means the artifact is out of range: keep today's corner list instead.
      if (!placeColumn()) {
        presentCorner();
        return;
      }
      gutter.innerHTML = '';
      trayList.innerHTML = '';
      anchoredCards = [];
      for (var i = 0; i < pending.length; i++) {
        var entry = pending[i];
        var card = buildTrayItem(i, entry);
        if (entry.el) {
          gutter.appendChild(card);
          anchoredCards.push({card: card, el: entry.el});
        } else {
          trayList.appendChild(card);
        }
      }
      // Only now is the bar's height final: unanchored comments (every shortcut button
      // makes one) live in the bar's own list.
      setColumnBottom();
      measureCards();
      layoutColumn();
    }

    // Measured once per reflow: anchors in document space and card heights. Neither
    // changes with scrolling, so they are read once, here. The scroll path reads two
    // rects -- the reference anchor's and the column's own -- to measure displacement
    // rather than assume it, independent of card count.
    function measureCards() {
      var y = window.scrollY || 0;
      anchoredCards.sort(function (a, b) {
        return (a.el.getBoundingClientRect().top) - (b.el.getBoundingClientRect().top);
      });
      // Anchor order becomes DOM order, so the list regime reads top-to-bottom in the
      // order the anchored regime would have placed them.
      for (var d = 0; d < anchoredCards.length; d++) gutter.appendChild(anchoredCards[d].card);
      // The band belongs to the layout, not to the scroll position: read it here, once,
      // so the scroll path's layout reads stay at two rects, independent of card count.
      bandHeight = gutter.clientHeight;
      var anchors = [], heights = [];
      for (var i = 0; i < anchoredCards.length; i++) {
        anchors.push(anchoredCards[i].el.getBoundingClientRect().top + y);
        heights.push(anchoredCards[i].card.offsetHeight);
      }
      cardHeights = heights;
      // The displacement between document space and the column is measured from a live
      // anchor, not taken from window.scrollY: whichever element actually scrolls the
      // content -- documentElement, body, or a wrapper the artifact introduced -- this
      // reads the real offset. One rect read per scroll instead of none, which buys
      // correctness under any scroll container.
      refEl = anchoredCards.length ? anchoredCards[0].el : null;
      refDocTop = anchors.length ? anchors[0] : 0;
      // The downward pass is translation invariant, so it can be done once in document
      // space and shifted by the scroll position on every frame.
      docTops = stackCards(anchors, heights, GUTTER_GAP);
    }

    // Pure: shift the document-space stack into the column and cap it from below.
    // The last card's top is capped at band - its height, each earlier card at its
    // successor's top minus gap minus its own height; cascading upward keeps every
    // pair apart and puts the last card wholly inside the column. There is no floor:
    // an anchor scrolled above the viewport takes its card with it, as today. Pinning
    // the top card to 0 instead would push it into its neighbour.
    function fitColumn(docTops, heights, gap, band, scrolled) {
      var tops = [], i;
      for (i = 0; i < docTops.length; i++) tops.push(docTops[i] - scrolled);
      for (i = tops.length - 1; i >= 0; i--) {
        var ceiling = (i === tops.length - 1) ? band - heights[i]
                                             : tops[i + 1] - gap - heights[i];
        if (tops[i] > ceiling) tops[i] = ceiling;
      }
      return tops;
    }

    // Two regimes, chosen by whether the cards fit the column at all.
    function layoutColumn() {
      if (!column || !gutter || !docTops || docTops.length === 0) return;
      var L = bandHeight, total = 0, i;
      for (i = 0; i < cardHeights.length; i++) total += cardHeights[i] + (i ? GUTTER_GAP : 0);
      if (total > L) {
        // More comment than column: anchoring cannot fit, so the column becomes a plain
        // scrollable list in anchor order. Nothing is unreachable and the scroll path
        // has no work to do — a list does not move with the page.
        gutter.style.overflowY = 'auto';
        for (i = 0; i < anchoredCards.length; i++) {
          var st = anchoredCards[i].card.style;
          st.position = 'static';
          st.marginBottom = GUTTER_GAP + 'px';
          st.top = '';
        }
        return;
      }
      gutter.style.overflowY = 'hidden';
      // Displacement is measured between document space and THE COLUMN, not the viewport:
      // the reference anchor's offset from the column's own top edge. Whatever moves the
      // column -- the window scrolling, body scrolling itself, a wrapper the artifact
      // introduced, or the column riding a transformed containing block -- is accounted
      // for once and only once. Two rect reads per scroll, independent of card count.
      var displaced = window.scrollY || 0;
      if (refEl) {
        displaced = refDocTop - (refEl.getBoundingClientRect().top - gutter.getBoundingClientRect().top);
      }
      var tops = fitColumn(docTops, cardHeights, GUTTER_GAP, L, displaced);
      for (i = 0; i < anchoredCards.length; i++) {
        var s2 = anchoredCards[i].card.style;
        s2.position = 'absolute';
        s2.marginBottom = '';
        s2.top = tops[i] + 'px';
      }
    }

    // Right edge of everything the artifact itself renders. Every element we did not
    // inject is measured, nothing is filtered by position and scrollWidth is never
    // read, so out-of-flow panels, tables wider than their container and
    // display:contents subtrees are all covered.
    function measureContentRight() {
      var rightmost = 0, seen = false;
      var all = document.body.querySelectorAll('*');
      for (var i = 0; i < all.length; i++) {
        var el = all[i];
        if (el.className && String(el.className).indexOf(GLOBAL_PREFIX) === 0) continue;
        if (el.closest('.' + GLOBAL_PREFIX + '-dock,.' + GLOBAL_PREFIX + '-gutter')) continue;
        var r = el.getBoundingClientRect();
        if (r.width === 0 && r.height === 0) continue;
        seen = true;
        if (r.right > rightmost) rightmost = r.right;
      }
      return seen ? rightmost : null;
    }

    // Pure: the widest column that leaves a gap on both sides, or 0 when the free
    // space to the right of the content cannot hold the narrowest one.
    function chooseWidth(clientWidth, contentRight, gap, min, max) {
      var w = Math.min(max, clientWidth - contentRight - 2 * gap);
      return w < min ? 0 : Math.floor(w);
    }

    function chooseColumn() {
      if (window.innerWidth < GUTTER_THRESHOLD) return 0;
      if (window.innerHeight < GUTTER_MIN_HEIGHT) return 0;
      if (!viewportIsContainingBlock()) return 0;
      var right = measureContentRight();
      if (right === null) return 0;
      var w = chooseWidth(document.documentElement.clientWidth, right, GUTTER_GAP, COL_MIN, COL_MAX);
      if (w) columnLeft = right + GUTTER_GAP;
      return w;
    }

    // Owned by page layout, not by the comment list: decided at load and on the
    // existing reflow paths, never as a function of how many comments are pending.
    // Nothing outside the injected layer is written — not one artifact style.
    function placeColumn() {
      if (!active) {
        prevDockLeft = dock.style.left;
        prevDockRight = dock.style.right;
        prevDockWidth = dock.style.width;
        prevTrayWidth = tray.style.width;
        active = true;
      }
      var w = chooseColumn();
      if (!w) {
        restoreColumn();
        column = 0;
        return 0;
      }
      // Width only. The bar's height depends on its own content, so its top -- and
      // therefore this column's bottom -- can only be read after the bar is filled;
      // that is setColumnBottom(), which the caller runs once routing is done.
      dock.style.left = columnLeft + 'px';
      dock.style.right = 'auto';
      dock.style.width = w + 'px';
      tray.style.width = '100%';
      column = w;
      if (gutter) {
        gutter.style.left = columnLeft + 'px';
        gutter.style.width = w + 'px';
      }
      if (!resolvesAgainstViewport(w)) {
        restoreColumn();
        column = 0;
        return 0;
      }
      return w;
    }

    // The column's whole coordinate model -- margin measured against the viewport, cards
    // placed from the viewport's top edge, the bar's offset read from the viewport's
    // bottom -- holds only while the viewport is the containing block for the injected
    // layer's fixed boxes. The walk below follows the layer's real ancestor chain rather
    // than assuming it is exactly body and documentElement: an artifact can reparent the
    // layer. The property enumeration is a known set plus an empirical backstop
    // (resolvesAgainstViewport at placement time), not a complete test -- it can fall
    // behind browser implementations. Residual: a property the set misses lets the
    // column open anyway, so cards may stop tracking their paragraphs; non-overlap with
    // the bar stays structural regardless, because the column clips and the two boxes do
    // not intersect.
    function viewportIsContainingBlock() {
      // The chain is walked rather than assumed: if anything ever reparents the layer,
      // the hosts between it and the root are inspected too.
      var hosts = [], el = dock ? dock.parentElement : document.body;
      while (el) { hosts.push(el); el = el.parentElement; }
      if (document.documentElement && hosts.indexOf(document.documentElement) === -1) {
        hosts.push(document.documentElement);
      }
      for (var i = 0; i < hosts.length; i++) {
        if (!hosts[i]) continue;
        var cs = window.getComputedStyle(hosts[i]);
        for (var j = 0; j < CB_PROPS.length; j++) {
          var v = cs[CB_PROPS[j]];
          if (v && v !== 'none' && v !== 'normal') return false;
        }
        var contain = cs.contain || '';
        if (/paint|layout|strict|content/.test(contain)) return false;
        var willChange = cs.willChange || '';
        if (/transform|translate|rotate|scale|perspective|filter|contain|offset-path/.test(willChange)) {
          return false;
        }
        if (cs.transformStyle === 'preserve-3d') return false;
        if (cs.contentVisibility === 'auto') return false;
        if (cs.offsetPath && cs.offsetPath !== 'none') return false;
        var zoom = cs.zoom;
        if (zoom && zoom !== 'normal' && parseFloat(zoom) !== 1) return false;
        // Not a containing-block property, but it redefines which edge `left` counts
        // from, which the margin measurement assumes.
        if (cs.writingMode && cs.writingMode !== 'horizontal-tb') return false;
      }
      return true;
    }

    // Read back as well. The enumeration above is a known set, not a complete test for
    // the causes the spec defines; this is the backstop for whatever the enumeration
    // misses, at placement time.
    // The free margin is measured in viewport coordinates (getBoundingClientRect) but
    // written in containing-block coordinates (style.left), and the whole card layout
    // assumes the column's top edge is the viewport's. Those hold only while the layer's
    // own boxes resolve against the viewport. Rather than guess which artifact CSS breaks
    // that -- a transform, zoom, filter or contain:paint anywhere up the tree, on any
    // axis -- write the values and read them back: the column must land where it was put,
    // and the bar must sit its own offset above the viewport's bottom edge. Whatever
    // fails this is out of range by section 1 and keeps today's corner list.
    //
    // Both readings must be non-degenerate before they can disagree, so an environment
    // that reports no geometry at all (a test double) is not mistaken for a hostile one.
    function resolvesAgainstViewport(w) {
      if (!gutter) return true;
      var gr = gutter.getBoundingClientRect();
      if (gr.width > 0 && Math.abs(gr.left - columnLeft) > 1) return false;
      var dr = dock.getBoundingClientRect();
      var barBottom = parseFloat(window.getComputedStyle(dock).bottom);
      if (dr.height > 0 && barBottom === barBottom
          && Math.abs((window.innerHeight - dr.bottom) - barBottom) > 1) return false;
      return true;
    }

    // The column stops one gap above the action bar. Run after the bar holds its final
    // content, so the bar's own list can never grow into the column afterwards.
    //
    // Both boxes are fixed children of body, so they share one containing block, and
    // `bottom` is measured from the same edge for both. Stating the column's bottom as
    // "the bar's own bottom offset, plus the bar's height, plus a gap" therefore needs
    // no viewport constant, and holds whatever that containing block turns out to be --
    // including an artifact that makes itself one with a transform, filter or
    // contain:paint on body, where viewport arithmetic would be wrong.
    function setColumnBottom() {
      if (!gutter || !column) return;
      var barBottom = parseFloat(window.getComputedStyle(dock).bottom) || 0;
      gutter.style.bottom = (barBottom + dock.offsetHeight + GUTTER_GAP) + 'px';
    }

    function restoreColumn() {
      if (!active) return;
      dock.style.left = prevDockLeft;
      dock.style.right = prevDockRight;
      dock.style.width = prevDockWidth;
      tray.style.width = prevTrayWidth;
      if (gutter) {
        gutter.style.left = prevGutterLeft;
        gutter.style.width = prevGutterWidth;
        gutter.style.bottom = prevGutterBottom;
      }
      active = false;
      column = 0;
    }

    function removeEntry(idx) {
      if (idx < 0 || idx >= pending.length) return;
      var removed = pending.splice(idx, 1)[0];
      if (removed.kind === 'block' && removed.el) {
        var stillMarked = false;
        for (var i = 0; i < pending.length; i++) {
          if (pending[i].kind === 'block' && pending[i].el === removed.el) { stillMarked = true; break; }
        }
        if (!stillMarked) removed.el.classList.remove(GLOBAL_PREFIX + '-marked');
      }
      saveDraft(pending, artifactPath);
      refreshTray();
    }

    function clearAll() {
      for (var i = 0; i < pending.length; i++) {
        if (pending[i].kind === 'block' && pending[i].el) pending[i].el.classList.remove(GLOBAL_PREFIX + '-marked');
      }
      pending = [];
      clearDraft(artifactPath);
      refreshTray();
    }

    async function sendBatch() {
      if (!sessionId || pending.length === 0) return;
      var count = pending.length;
      var ordered = pending.slice();
      ordered.sort(function(a, b) {
        // Shortcut entries lead the batch; among themselves the sort is stable,
        // so they keep click order. Block comments follow in document order.
        var aShortcut = a.kind !== 'block';
        var bShortcut = b.kind !== 'block';
        if (aShortcut && !bShortcut) return -1;
        if (!aShortcut && bShortcut) return 1;
        if (aShortcut && bShortcut) return 0;
        if (!a.el || !b.el) return 0;
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
          if (pending[i].kind === 'block' && pending[i].el) pending[i].el.classList.remove(GLOBAL_PREFIX + '-marked');
        }
        pending = [];
        clearDraft(artifactPath);
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
      layoutColumn();
      if (!hovered) return;
      if (popover) positionPopover(popover, hovered);
      else positionTrigger(hovered);
    }
  })();
}
