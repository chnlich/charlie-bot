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
    // The strip the body leaves at its right edge for the comment trigger, which sits in it, never over text.
    var TRIGGER_STRIP = 40;
    // The rail: where every control of the layer sits. A window at least RAIL_SIDE_MIN_WIDTH wide gives it a
    // right-hand column of RAIL_WIDTH; a narrower one gives it the bottom strip.
    var RAIL_SIDE_MIN_WIDTH = 900;
    var RAIL_WIDTH = 320;
    var RAIL_PADDING = 12;
    var GUTTER_GAP = 8;
    // The least band the anchored column can use: one card and a gap. A rail whose controls leave less than
    // that keeps every entry in the tray list, which the rail's own scroll reaches.
    var GUTTER_MIN_BAND = 80;
    var NO_SESSION_MESSAGE = 'Cannot send: this page\'s address names no session. ' +
      'Open the page from its session\'s chat.';
    var SECTION_SELECTOR = 'section';
    // Characters of a fork's question text that the locator shows after the fork's
    // number. The session reads them to tell which item a commented line belongs
    // to without opening the page.
    var QUESTION_HEAD_CHARS = 30;
    // Characters of the page text that precede a row's first character. A draft on a row whose
    // quote repeats on the page stores them, so a reload finds that row again.
    var BEFORE_CHARS = 32;
    // The kind of one pending entry, which doubles as its dedup key and as the
    // persisted draft discriminator. A manual block comment has the BLOCK kind;
    // each shortcut below has its own; an option pick on a question has the PICK kind.
    var ENTRY_KIND = {
      BLOCK: 'block',
      IMPROVE: 'improve',
      SHORTEN: 'shorten',
      VERIFY: 'verify',
      PICK: 'pick',
    };
    // Adding a button here needs no other change: the entry's kind is its own.
    var SHORTCUTS = [
      {
        kind: ENTRY_KIND.IMPROVE,
        label: 'Improve',
        prompt: 'Keep this page\'s goal and resolved choices fixed; raise doubts about them as ' +
          'questions. Think from scratch for a better implementation: compare each candidate ' +
          'with the current design, adopt one that wins, otherwise keep the current design.',
      },
      {
        kind: ENTRY_KIND.SHORTEN,
        label: 'Shorten',
        prompt: 'This plan is too long. Rewrite it to state the key points concretely; ' +
          'delete anything that is not a key point or demote it into the Other Details list.',
      },
      {
        kind: ENTRY_KIND.VERIFY,
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
    var pending = [];
    var tray = null;
    var trayHeader = null;
    var trayList = null;
    var traySendBtn = null;
    var trayClearBtn = null;
    var trayStatus = null;
    var dockStack = null;
    var gutter = null;
    var anchoredCards = [];
    var docTops = null;
    var bandHeight = 0;
    var refEl = null;
    var refDocTop = 0;
    var cardHeights = null;
    var reflowScheduled = false;
    // Picks on the questions of plan, understanding and waiting pages only. A pick is an entry of `pending`,
    // so the one tray holds picks and comments and one Send sends both.
    var PICK_PAGE_PATTERN = /(^|\/)(plan|understanding|waiting)_[^/]*$/;
    var QUOTE_CLOSERS = {'「': '」', '“': '”', '"': '"'};
    // The tray stage names each step of a send. Editing changes the pending list. In countdown the undo window is
    // open. In sending the post is in flight, retried on a network error or 5xx. Entries leave the list when the
    // post lands.
    var trayStage = 'editing';
    // The session the send in flight posts to, fixed at the click, and the outcome of the last send
    // ({text, error}), which the tray shows until the next Send click or Clear.
    var sendTarget = null;
    var trayNotice = null;
    var sendHandle = null;
    var picks = [];
    var trayPickRow = null;
    var trayPickLabel = null;
    var trayRestButton = null;
    var trayUndoButton = null;

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
    window.__cbcGutterGap = GUTTER_GAP;
    window.__cbcReanchor = reanchorPending;

    installStyles();
    installListeners();
    installShortcuts();
    installPicks();
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
      return kind === ENTRY_KIND.PICK || isShortcutKind(kind) ? kind : ENTRY_KIND.BLOCK;
    }

    function serializeDraft(entries) {
      var out = [];
      for (var i = 0; i < entries.length; i++) {
        var entry = entries[i] || {};
        var quote = String(entry.quote == null ? '' : entry.quote);
        var item = {
          kind: normalizeKind(entry.kind),
          quote: quote,
          context: String(entry.context == null ? '' : entry.context),
          comment: String(entry.comment == null ? '' : entry.comment),
          occurrence: entry.el ? sameQuoteOccurrence(entry.el, quote) : entry.occurrence,
        };
        var before = beforeOfEntry(entry, quote);
        if (before !== undefined) item.before = before;
        out.push(item);
      }
      return out;
    }

    // The 0-based index of `el` among the page's rows with the same quote, top to
    // bottom. The restore re-finds rows by quote alone and takes this index, so a
    // draft on a repeated line re-anchors to its own row. A detached element reads
    // as 0, the first row.
    function sameQuoteOccurrence(el, quote) {
      if (!el) return 0;
      var rows = collectCommentableBlocks();
      var index = 0;
      for (var i = 0; i < rows.length; i++) {
        if (rows[i] === el) return index;
        if (cleanText(rows[i], 400) === quote) index++;
      }
      return 0;
    }

    // The rows among `rows` whose text equals `quote`, in page order.
    function rowsHolding(quote, rows) {
      var held = [];
      for (var i = 0; i < rows.length; i++) {
        if (cleanText(rows[i], 400) === quote) held.push(rows[i]);
      }
      return held;
    }

    function quoteRepeats(quote, row) {
      var rows = collectCommentableBlocks();
      return rows.indexOf(row) !== -1 && rowsHolding(quote, rows).length >= 2;
    }

    // The BEFORE_CHARS characters that precede the row's first character in the page text.
    // They come from the document's text nodes in source order, each whitespace run collapsed
    // to one space. Script content, style content and the comment layer are skipped. The walk
    // reads the source, not the layout, so a folded block gives the same value open or shut.
    function beforeOf(row) {
      var tail = '';
      var inSpace = false;
      var reached = false;
      function read(text, inRow) {
        for (var i = 0; i < text.length && !reached; i++) {
          var ch = text.charAt(i);
          if (/\s/.test(ch)) {
            if (!inSpace) {
              tail = (tail + ' ').slice(-BEFORE_CHARS);
              inSpace = true;
            }
          } else if (inRow) {
            reached = true;
          } else {
            tail = (tail + ch).slice(-BEFORE_CHARS);
            inSpace = false;
          }
        }
      }
      function walk(node, inRow) {
        if (reached) return;
        if (node.nodeType === 3) {
          read(node.textContent, inRow);
          return;
        }
        if (node.nodeType !== 1 || node.tagName === 'SCRIPT' || node.tagName === 'STYLE' ||
            node.classList.contains(UI_CLASS)) {
          return;
        }
        var within = inRow || node === row;
        for (var i = 0; i < node.childNodes.length && !reached; i++) {
          walk(node.childNodes[i], within);
        }
      }
      walk(document.body, false);
      if (!reached) throw new Error('the comment row is not in the page text');
      return tail;
    }

    // The `before` value a draft stores for one entry. An attached row whose quote repeats has the
    // words before it; an unattached draft keeps the value it stored; any other entry has none
    // (undefined).
    function beforeOfEntry(entry, quote) {
      if (!entry.el) return entry.before;
      return quoteRepeats(quote, entry.el) ? beforeOf(entry.el) : undefined;
    }

    // The row a pending draft belongs to, or null when the page does not place it. A draft with a
    // stored `before` takes the one row whose quote and `before` both match. With several such rows,
    // it takes the row at its stored occurrence among the rows holding the quote, when that row is
    // one of them. A draft without a stored `before` takes the only row that holds its quote.
    function rowForDraft(entry, rows) {
      var held = rowsHolding(entry.quote, rows);
      if (typeof entry.before !== 'string') return held.length === 1 ? held[0] : null;
      var candidates = [];
      for (var i = 0; i < held.length; i++) {
        if (beforeOf(held[i]) === entry.before) candidates.push(held[i]);
      }
      if (candidates.length <= 1) return candidates.length === 1 ? candidates[0] : null;
      var own = held[entry.occurrence];
      return candidates.indexOf(own) === -1 ? null : own;
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
        var entry = {
          kind: normalizeKind(item.kind),
          el: null,
          quote: String(item.quote == null ? '' : item.quote),
          context: String(item.context == null ? '' : item.context),
          comment: String(item.comment == null ? '' : item.comment),
          occurrence: item.occurrence == null ? 0 : item.occurrence,
        };
        // A draft stored before the before field existed has none, and restores by its quote alone.
        if (item.before != null) entry.before = String(item.before);
        out.push(entry);
      }
      return out;
    }

    function saveDraft(entries, absPath) {
      var data = JSON.stringify(serializeDraft(entries));
      try {
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
    // from the parent window. The tray then targets the session currently active
    // in the app, not the session baked into the artifact URL when the iframe was
    // built. Degrades silently to the artifact-URL-derived session id when not
    // framed, or when the parent accessor is unavailable or throws.
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

    function sessionLabelOf(id) {
      return sessionNameCache[id] || fallbackSessionLabel(id);
    }

    function targetSessionLabel() {
      var id = resolveTargetSessionId();
      if (!id) return '';
      return sessionLabelOf(id);
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
        // session, requestedId is that live id, not the artifact-URL hash
        // session. The `requestedId === hashSessionId` guard below then keeps
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
        '.' + GLOBAL_PREFIX + '-popover{background:#161b22;color:#e6edf3;border:1px solid #2d3340;border-radius:8px;box-shadow:0 18px 50px rgba(0,0,0,.45);padding:10px;font:13px/1.4 -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif}' +
        '.' + GLOBAL_PREFIX + '-popover textarea{box-sizing:border-box;width:100%;min-height:140px;resize:vertical;background:#0d1117;color:#e6edf3;border:1px solid #2d3340;border-radius:6px;padding:7px 8px;font:13px/1.4 -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;outline:none}' +
        '.' + GLOBAL_PREFIX + '-popover textarea:focus{border-color:#58a6ff;box-shadow:0 0 0 2px rgba(88,166,255,.18)}' +
        '.' + GLOBAL_PREFIX + '-actions{display:flex;justify-content:flex-end;gap:7px;margin-top:8px}' +
        '.' + GLOBAL_PREFIX + '-actions button{border:1px solid #2d3340;border-radius:6px;padding:4px 9px;background:#1c2230;color:#e6edf3;font:12px -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;cursor:pointer}' +
        '.' + GLOBAL_PREFIX + '-actions button:disabled{cursor:not-allowed;opacity:.58}' +
        '.' + GLOBAL_PREFIX + '-add{background:#238636!important;border-color:#2ea043!important;color:#fff!important}' +
        '.' + GLOBAL_PREFIX + '-error{display:none;margin-top:8px;color:#ffb4ab;font-size:12px}' +
        // The page's own body scrolls, so the rail is never over the content: the body's margin and height are
        // the two custom properties placeRail and setRailHeight write. The rail is outside the body's box.
        ':root{--cbc-rail-w:0px;--cbc-rail-h:0px}' +
        'html{height:100%!important;overflow:hidden!important}' +
        'body{box-sizing:border-box!important;height:calc(100% - var(--cbc-rail-h))!important;min-height:0!important;' +
          'overflow-y:auto!important;margin-right:calc(var(--cbc-rail-w) + ' + TRIGGER_STRIP + 'px)!important}' +
        // The stack takes the bottom with an auto top margin. Its overflow then runs down, where the rail scrolls
        // to it; a flex-end justification would push the overflow above the rail, out of reach.
        '.' + GLOBAL_PREFIX + '-dock{position:fixed;z-index:2147483646;display:flex;flex-direction:column;' +
          'box-sizing:border-box;overflow-y:auto}' +
        '.' + GLOBAL_PREFIX + '-dock-side{top:0;right:0;bottom:0;width:' + RAIL_WIDTH + 'px;padding:' + RAIL_PADDING + 'px}' +
        '.' + GLOBAL_PREFIX + '-dock-bottom{left:0;right:0;bottom:0;max-height:60vh;padding:8px}' +
        '.' + GLOBAL_PREFIX + '-stack{display:flex;flex-direction:column-reverse;gap:8px;min-width:0;margin-top:auto}' +
        '.' + GLOBAL_PREFIX + '-stack>*{width:100%;box-sizing:border-box}' +
        '.' + GLOBAL_PREFIX + '-shortcuts{display:flex;flex-wrap:wrap;justify-content:flex-end;gap:6px;font:12px -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif}' +
        '.' + GLOBAL_PREFIX + '-shortcut{border:1px solid #2ea043;border-radius:7px;padding:7px 10px;background:#238636;color:#fff;font:12px -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;box-shadow:0 10px 30px rgba(0,0,0,.35);cursor:pointer}' +
        '.' + GLOBAL_PREFIX + '-shortcut:hover:not(:disabled){background:#2ea043}' +
        '.' + GLOBAL_PREFIX + '-shortcut:disabled{cursor:not-allowed;opacity:.64;background:#30363d;border-color:#484f58;color:#c9d1d9}' +
        '.' + GLOBAL_PREFIX + '-shortcut-reason{flex-basis:100%;max-width:220px;background:#5f2120;color:#ffe2df;border:1px solid rgba(248,81,73,.7);border-radius:6px;padding:5px 7px;line-height:1.3;box-shadow:0 10px 30px rgba(0,0,0,.35)}' +
        '.' + GLOBAL_PREFIX + '-marked{outline:2px solid rgba(88,166,255,.45)!important;outline-offset:2px!important;box-shadow:0 0 0 4px rgba(88,166,255,.08)!important}' +
        '.' + GLOBAL_PREFIX + '-card-hover{outline:3px solid rgba(255,196,0,.9)!important;outline-offset:3px!important;box-shadow:0 0 0 5px rgba(255,196,0,.18)!important}' +
        '.' + GLOBAL_PREFIX + '-tray{background:#161b22;color:#e6edf3;border:1px solid #2d3340;border-radius:8px;box-shadow:0 18px 50px rgba(0,0,0,.45);padding:10px;font:13px/1.4 -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;display:none;flex-direction:column;gap:8px}' +
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
        '.' + GLOBAL_PREFIX + '-tray-status{color:#c9d1d9;font-size:11px;line-height:1.3}' +
        '.' + GLOBAL_PREFIX + '-tray-status-error{background:#5f2120;color:#ffe2df;border:1px solid rgba(248,81,73,.7);border-radius:6px;padding:5px 7px}' +
        '.' + GLOBAL_PREFIX + '-tray-actions{display:flex;justify-content:flex-end;gap:7px}' +
        '.' + GLOBAL_PREFIX + '-tray-send{border:1px solid #2ea043;border-radius:6px;padding:5px 10px;background:#238636;color:#fff;font:12px -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;cursor:pointer}' +
        '.' + GLOBAL_PREFIX + '-tray-send:hover:not(:disabled){background:#2ea043}' +
        '.' + GLOBAL_PREFIX + '-tray-send:disabled{cursor:not-allowed;opacity:.64;background:#30363d;border-color:#484f58;color:#c9d1d9}' +
        '.' + GLOBAL_PREFIX + '-tray-action{border:1px solid #2d3340;border-radius:6px;padding:5px 10px;background:#1c2230;color:#e6edf3;font:12px -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;cursor:pointer;white-space:nowrap}' +
        '.' + GLOBAL_PREFIX + '-tray-action:hover:not(:disabled){background:#262d36}' +
        '.' + GLOBAL_PREFIX + '-tray-action:disabled{cursor:not-allowed;opacity:.64}' +
        '.' + GLOBAL_PREFIX + '-tray-picks{display:flex;flex-wrap:wrap;align-items:center;justify-content:space-between;gap:6px 8px;color:#8b949e;font-size:12px}' +
        '.' + GLOBAL_PREFIX + '-tray-picks-label{white-space:nowrap}' +
        '.' + GLOBAL_PREFIX + '-icon{display:inline-flex;vertical-align:middle;margin-right:5px}' +
        '.' + GLOBAL_PREFIX + '-icon:empty{display:none}' +
        '.' + GLOBAL_PREFIX + '-picks{display:flex;flex-wrap:wrap;gap:6px;margin:8px 0 4px}' +
        '.' + GLOBAL_PREFIX + '-pick{border:1px solid #d0d7de;border-radius:6px;padding:4px 9px;background:#f6f8fa;color:#1b2430;font:12px -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;cursor:pointer}' +
        '.' + GLOBAL_PREFIX + '-pick[aria-pressed="true"]{background:#1163c0;border-color:#1163c0;color:#fff}' +
        '.' + GLOBAL_PREFIX + '-pick:disabled{cursor:not-allowed;opacity:.64}' +
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
      trigger.innerHTML = cbIcon('comment', 16);
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

    // The trigger is centred in the strip the body leaves at its right edge, level with the top of its block.
    // The strip is outside the body's box, so the trigger covers no page content.
    function positionTrigger(block) {
      var btn = ensureTrigger();
      var rect = block.getBoundingClientRect();
      var page = document.body.getBoundingClientRect();
      var left = page.right + (TRIGGER_STRIP - TRIGGER_SIZE) / 2;
      var top = clamp(rect.top + 4, page.top + 8, page.bottom - TRIGGER_SIZE - 8);
      btn.style.left = left + 'px';
      btn.style.top = top + 'px';
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

    // The question segment of the locator names the fork (item) a commented line
    // belongs to: `Q<n> "<question head>"`. It applies to a line inside a
    // div.fork whose question line (p.q) carries a span.fn number, except the
    // question line itself — its quote already carries the number.
    function forkQuestionLabel(block) {
      var fork = block.closest('.fork');
      if (!fork) return '';
      var question = fork.querySelector('.q');
      if (!question || question === block) return '';
      var fn = question.querySelector('.fn');
      var number = fn ? cleanText(fn, 20) : '';
      if (!number) return '';
      var head = cleanText(question, 200);
      if (head.indexOf(number) === 0) head = head.slice(number.length).trim();
      if (head.length > QUESTION_HEAD_CHARS) head = head.slice(0, QUESTION_HEAD_CHARS) + '\u2026';
      return 'Q' + number + ' "' + head + '"';
    }

    function contextFor(block) {
      var tag = (block.tagName || '').toLowerCase();
      var heading = tag === 'section' ? sectionTitle(block) || previousHeading(block)
                                      : previousHeading(block) || sectionTitle(block);
      var question = forkQuestionLabel(block);
      if (!question) return heading;
      return heading ? heading + ' \u203A ' + question : question;
    }

    function closePopover() {
      if (popover && popover.parentNode) popover.parentNode.removeChild(popover);
      popover = null;
    }

    function setPopoverError(node, message) {
      var error = node.querySelector('.' + GLOBAL_PREFIX + '-error');
      error.textContent = message;
      error.style.display = 'block';
    }

    function buildCommentEntry(quote, context, comment, suffix) {
      if (quote === '') {
        return {reference: context, answer: comment};
      }
      var reference = context
        ? context + ' \u203A "' + quote + '"'
        : '"' + quote + '"';
      // A quote spanning lines keeps its locator shape. The suffix rides the last
      // quote line, right of the closing quote.
      reference = reference.split('\n');
      reference[reference.length - 1] += suffix || '';
      return {reference: reference.join('\n'), answer: comment};
    }

    function resolveEntryDraft(entry, suffix) {
      return buildCommentEntry(entry.quote, entry.context, entry.comment, suffix);
    }

    // The occurrence suffix still to disambiguate. After the fork label, m rows on
    // the page share the locator's heading, question segment and quote. k counts
    // them top to bottom from 1. One match, or an entry the page no longer holds,
    // renders no suffix.
    function occurrenceSuffix(entry) {
      if (!entry.el || entry.quote === '') return '';
      var rows = collectCommentableBlocks();
      var k = 0;
      var m = 0;
      for (var i = 0; i < rows.length; i++) {
        if (cleanText(rows[i], 400) !== entry.quote) continue;
        if (contextFor(rows[i]) !== entry.context) continue;
        m++;
        if (rows[i] === entry.el) k = m;
      }
      if (m < 2 || !k) return '';
      return ' (' + k + ' of ' + m + ')';
    }

    function buildBatchMessage(entries) {
      var clickEntries = [];
      for (var i = 0; i < entries.length; i++) {
        var entry = entries[i];
        clickEntries.push(resolveEntryDraft(entry, occurrenceSuffix(entry)));
      }
      return buildClickMessage('[Artifact comments \u00B7 ' + artifactPath + '] (' + entries.length + ')', clickEntries);
    }

    // The rail: its root, the stack that holds the controls, and the gutter of anchored cards. The stack is
    // observed, so a change in its size re-measures the rail: the body's height, the cards and the trigger follow.
    function ensureDock() {
      if (dock) return dockStack;
      dock = document.createElement('div');
      dock.className = GLOBAL_PREFIX + '-dock';
      injectRoot(dock);
      dockStack = document.createElement('div');
      dockStack.className = GLOBAL_PREFIX + '-stack';
      dock.appendChild(dockStack);
      gutter = document.createElement('div');
      gutter.className = GLOBAL_PREFIX + '-gutter';
      gutter.style.display = 'none';
      injectRoot(gutter);
      new ResizeObserver(railSizeChanged).observe(dockStack);
      return dockStack;
    }

    // Every control of the layer goes into the rail's stack, marked as the layer's own.
    function injectInDock(node) {
      node.classList.add(UI_CLASS);
      ensureDock().appendChild(node);
      return node;
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
          if (!resolveTargetSessionId()) {
            button.disabled = true;
          } else {
            button.addEventListener('click', function() {
              addShortcutComment(shortcut);
            });
          }
          container.appendChild(button);
        })(SHORTCUTS[i]);
      }

      if (!resolveTargetSessionId()) {
        var reason = document.createElement('div');
        reason.className = GLOBAL_PREFIX + '-shortcut-reason';
        reason.textContent = NO_SESSION_MESSAGE;
        container.appendChild(reason);
      }

      injectInDock(container);
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
        occurrence: 0,
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

      if (!resolveTargetSessionId()) {
        addBtn.disabled = true;
        setPopoverError(node, NO_SESSION_MESSAGE);
      } else {
        addBtn.addEventListener('click', function() {
          submitComment(block, textarea);
        });
      }

      injectInDock(node);
      popover = node;
      textarea.focus();
    }

    function submitComment(block, textarea) {
      var comment = textarea.value.trim();
      if (!comment) {
        textarea.focus();
        return;
      }
      pending.push({
        kind: ENTRY_KIND.BLOCK,
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

    // The pick record a restored pick entry belongs to: the question it names, with the option it carries.
    function pickRecordFor(entry) {
      for (var i = 0; i < picks.length; i++) {
        if (entry.context === 'Q' + picks[i].n && optionIndexOf(picks[i], entry.comment) !== -1) return picks[i];
      }
      return null;
    }

    function reanchorPending() {
      var rows = collectCommentableBlocks();
      for (var i = 0; i < pending.length; i++) {
        var entry = pending[i];
        if (entry.el) continue;
        if (entry.kind === ENTRY_KIND.PICK) {
          var record = pickRecordFor(entry);
          if (record) entry.el = record.fork;
          continue;
        }
        if (entry.quote === '') continue;
        var row = rowForDraft(entry, rows);
        if (!row) continue;
        entry.el = row;
        row.classList.add(GLOBAL_PREFIX + '-marked');
      }
    }

    function installBatchTray() {
      pending = loadDraft(artifactPath);
      reanchorPending();
      var container = document.createElement('div');
      container.className = GLOBAL_PREFIX + '-tray';

      var header = document.createElement('div');
      header.className = GLOBAL_PREFIX + '-tray-header';
      container.appendChild(header);

      // The question count of a page with questions: how many still have no pick, and the action that picks
      // each of them as recommended.
      var pickRow = document.createElement('div');
      pickRow.className = GLOBAL_PREFIX + '-tray-picks';
      var pickLabelNode = document.createElement('span');
      pickLabelNode.className = GLOBAL_PREFIX + '-tray-picks-label';
      var restBtn = makeButton('Rest as recommended', restPicksAsRecommended, GLOBAL_PREFIX + '-tray-action', null);
      pickRow.appendChild(pickLabelNode);
      pickRow.appendChild(restBtn);
      container.appendChild(pickRow);

      var list = document.createElement('div');
      list.className = GLOBAL_PREFIX + '-tray-list';
      container.appendChild(list);

      var status = document.createElement('div');
      status.className = GLOBAL_PREFIX + '-tray-status';
      status.style.display = 'none';
      container.appendChild(status);

      var actions = document.createElement('div');
      actions.className = GLOBAL_PREFIX + '-tray-actions';
      var undoBtn = makeButton('Undo', undoSend, GLOBAL_PREFIX + '-tray-action', iconSlotFor('undo', 14));
      var clearBtn = document.createElement('button');
      clearBtn.type = 'button';
      clearBtn.textContent = 'Clear';
      clearBtn.className = GLOBAL_PREFIX + '-tray-action';
      clearBtn.addEventListener('click', resetPending);
      var sendBtn = document.createElement('button');
      sendBtn.type = 'button';
      sendBtn.textContent = 'Send';
      sendBtn.className = GLOBAL_PREFIX + '-tray-send';
      sendBtn.addEventListener('click', sendBatch);
      actions.appendChild(undoBtn);
      actions.appendChild(clearBtn);
      actions.appendChild(sendBtn);
      container.appendChild(actions);

      injectInDock(container);
      tray = container;
      trayHeader = header;
      trayPickRow = pickRow;
      trayPickLabel = pickLabelNode;
      trayRestButton = restBtn;
      trayUndoButton = undoBtn;
      trayList = list;
      traySendBtn = sendBtn;
      trayClearBtn = clearBtn;
      trayStatus = status;

      refreshTray();
      // The page's own scripts can rewrite its text after this script has run. The drafts
      // still unattached get a second look once the page has finished loading.
      window.addEventListener('load', function() {
        reanchorPending();
        refreshTray();
      });
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
      remove.innerHTML = cbIcon('close', 12);
      remove.title = 'Remove this entry';
      remove.setAttribute('aria-label', 'Remove this entry');
      remove.addEventListener('click', function() {
        removeEntry(idx);
      });
      // A send in flight holds the list still; an option pick's text is the option, so it has no edit.
      var locked = trayStage !== 'editing';
      remove.disabled = locked;
      editBtn.disabled = locked;

      body.appendChild(quote);
      body.appendChild(draft);
      controls.appendChild(remove);
      if (entry.kind !== ENTRY_KIND.PICK) controls.appendChild(editBtn);
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
      swapInInlineEditor(
          textarea, draftNode,
          function(value) {
            pending[idx].comment = value.trim();
            saveDraft(pending, artifactPath);
          },
          refreshTray);
    }

    // The tray's one status line: why Send cannot send and what to do, what the send in flight is doing, or what the
    // last send did. Null when Send is ready and no send has reported.
    function trayStatusOf() {
      if (!resolveTargetSessionId()) return {text: NO_SESSION_MESSAGE, error: true};
      if (trayStage === 'countdown') {
        return {
          text: 'Sends to ' + sessionLabelOf(sendTarget) + ' in ' + COMMENT_UNDO_MS / 1000 +
            ' seconds unless you press Undo.',
          error: false,
        };
      }
      if (trayStage === 'sending') {
        return {
          text: 'Sending to ' + sessionLabelOf(sendTarget) + '. A lost connection is retried until the server answers.',
          error: false,
        };
      }
      if (trayNotice) return trayNotice;
      if (pending.length === 0) {
        return {text: 'Nothing to send. Pick an option or write a comment, then press Send.', error: false};
      }
      return null;
    }

    function refreshTray() {
      if (!tray) return;
      if (pending.length > 0 && resolveTargetSessionId()) ensureTargetSessionName();
      var targetSuffix = targetSessionSuffix();
      var idle = trayStage === 'editing';
      var status = trayStatusOf();
      trayHeader.textContent = 'Pending (' + pending.length + ')' + targetSuffix;
      traySendBtn.textContent = idle ? 'Send ' + pending.length + targetSuffix : 'Sending';
      tray.style.display = pending.length > 0 || picks.length > 0 ? 'flex' : 'none';
      trayPickRow.style.display = picks.length > 0 ? 'flex' : 'none';
      trayPickLabel.textContent = 'No pick on ' + unpickedCount() + ' of ' + picks.length + ' questions';
      trayRestButton.disabled = !idle || unpickedRecommendedCount() === 0;
      trayUndoButton.style.display = trayStage === 'countdown' ? 'inline-block' : 'none';
      trayClearBtn.disabled = !idle;
      traySendBtn.disabled = !idle || !resolveTargetSessionId() || pending.length === 0;
      trayStatus.textContent = status ? status.text : '';
      trayStatus.style.display = status ? 'block' : 'none';
      trayStatus.classList.toggle(GLOBAL_PREFIX + '-tray-status-error', Boolean(status && status.error));
      renderPicks();
      scheduleReflow();
    }

    function scheduleReflow() {
      if (reflowScheduled) return;
      reflowScheduled = true;
      window.requestAnimationFrame(reflowGutter);
    }

    function reflowGutter() {
      reflowScheduled = false;
      var side = placeRail();
      if (side) presentGutter();
      else presentList();
      setRailHeight(side);
      // The trigger keeps its block's place once the rail has moved the page's edge.
      if (hovered && !popover) positionTrigger(hovered);
    }

    // The rail's size changes without a content change: the composer opens, a toast shows, the pick count relabels,
    // a textarea is dragged taller. Only the geometry follows here, so the controls and any open edit stay as they are.
    function railSizeChanged() {
      var side = railIsSide();
      setRailHeight(side);
      if (side) {
        setGutterBottom();
        layoutGutter();
      }
      if (hovered && !popover) positionTrigger(hovered);
    }

    // The rail's side for the window: a right-hand column when the window is wide enough, else the bottom strip.
    function railIsSide() {
      return window.innerWidth >= RAIL_SIDE_MIN_WIDTH;
    }

    // Places the rail for the window: its side class, the body's right margin (the --cbc-rail-w property), and the
    // gutter's column. Runs before the cards are built, because a card's height depends on its width.
    function placeRail() {
      var side = railIsSide();
      dock.classList.toggle(GLOBAL_PREFIX + '-dock-side', side);
      dock.classList.toggle(GLOBAL_PREFIX + '-dock-bottom', !side);
      document.documentElement.style.setProperty('--cbc-rail-w', (side ? RAIL_WIDTH : 0) + 'px');
      if (side) {
        gutter.style.left = (window.innerWidth - RAIL_WIDTH + RAIL_PADDING) + 'px';
        gutter.style.width = (RAIL_WIDTH - 2 * RAIL_PADDING) + 'px';
      }
      return side;
    }

    // The bottom strip's height (the --cbc-rail-h property) is its controls' height, read once they are placed.
    // The layout height, not offsetHeight: that rounds to whole pixels, and the body would then end a fraction of a
    // pixel under the strip's top edge. A side rail has no strip, so the body keeps its full height.
    function setRailHeight(side) {
      document.documentElement.style.setProperty('--cbc-rail-h',
        (side ? 0 : dock.getBoundingClientRect().height) + 'px');
    }

    // The bottom strip holds every pending entry in the tray's own list: it has no column for anchored cards.
    function presentList() {
      gutter.style.display = 'none';
      gutter.innerHTML = '';
      anchoredCards = [];
      docTops = null;
      trayList.innerHTML = '';
      for (var i = 0; i < pending.length; i++) {
        trayList.appendChild(buildTrayItem(i, pending[i]));
      }
    }

    function presentGutter() {
      gutter.style.display = '';
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
      // Only now is the column's bottom final: unanchored comments (every shortcut button makes one) live in the
      // stack's own list.
      setGutterBottom();
      layoutGutter();
    }

    // The column stops one gap above the controls. Read once the stack holds its final content.
    function setGutterBottom() {
      gutter.style.bottom = (window.innerHeight - dockStack.getBoundingClientRect().top + GUTTER_GAP) + 'px';
    }

    // The band the column has left above the controls, and the cards when it is tall enough. A stack that fills the
    // rail leaves no band, and a card in it would be clipped to a sliver, so every entry stays in the tray list,
    // which the rail's own scroll reaches. The mode flips only at that edge: a resize or a control that grows
    // without crossing it re-measures the column instead of rebuilding the list under the user.
    function layoutGutter() {
      if (dockStack.getBoundingClientRect().top - GUTTER_GAP < GUTTER_MIN_BAND) {
        if (gutter.style.display !== 'none') presentList();
        return;
      }
      if (gutter.style.display === 'none') {
        presentGutter();
        return;
      }
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
      // anchor, not taken from window.scrollY. Whichever element actually scrolls the
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
    // The last card's top is capped at band - its height. Each earlier card is capped
    // at its successor's top minus gap minus its own height. Cascading upward keeps
    // every pair apart and puts the last card wholly inside the column. There is no
    // floor: an anchor scrolled above the viewport takes its card with it. Pinning
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
      if (!docTops || docTops.length === 0) return;
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
      // the reference anchor's offset from the column's own top edge. The column can move
      // by the window scrolling, body scrolling itself, a wrapper the artifact introduced,
      // or a transformed containing block. Whatever moves it, this accounts for the move
      // once and only once. Two rect reads per scroll, independent of card count.
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

    function removeEntry(idx) {
      if (idx < 0 || idx >= pending.length) return;
      var removed = pending.splice(idx, 1)[0];
      if (removed.kind === ENTRY_KIND.BLOCK && removed.el) unmarkBlockIfUnused(removed.el);
      saveDraft(pending, artifactPath);
      refreshTray();
    }

    // The clear and send paths must leave the same final state: block markers
    // off, queue empty, draft gone, tray repainted.
    function resetPending() {
      for (var i = 0; i < pending.length; i++) {
        if (pending[i].kind === ENTRY_KIND.BLOCK && pending[i].el) {
          pending[i].el.classList.remove(GLOBAL_PREFIX + '-marked');
        }
      }
      pending = [];
      trayNotice = null;
      clearDraft(artifactPath);
      refreshTray();
    }

    // The batch order: shortcut entries lead, then the entries with no anchor, each group in click order (the sort
    // is stable). Anchored picks and block comments follow in document order, so each sits where the page puts it.
    function batchRank(entry) {
      if (isShortcutKind(entry.kind)) return 0;
      return entry.el ? 2 : 1;
    }

    function batchOrder(entries) {
      return entries.slice().sort(function(a, b) {
        var rank = batchRank(a) - batchRank(b);
        if (rank !== 0 || batchRank(a) !== 2) return rank;
        var mask = a.el.compareDocumentPosition(b.el);
        if (mask & Node.DOCUMENT_POSITION_FOLLOWING) return -1;
        if (mask & Node.DOCUMENT_POSITION_PRECEDING) return 1;
        return 0;
      });
    }

    // The one send of the tray: every pending entry, picks and comments, goes as one message to the comment target
    // after the undo window. The target is the one the click names. Entries that land leave the list; entries added
    // during the window stay pending. Every outcome sets the tray's status line.
    function sendBatch() {
      var target = resolveTargetSessionId();
      if (!target || pending.length === 0 || trayStage !== 'editing') return;
      var batch = batchOrder(pending);
      var message = buildBatchMessage(batch);
      var count = batch.length;
      var label = sessionLabelOf(target);
      sendTarget = target;
      trayNotice = null;
      trayStage = 'countdown';
      refreshTray();
      sendHandle = startUndoableSend(function() {
        trayStage = 'sending';
        refreshTray();
        return postCommentMessage(target, message);
      });
      sendHandle.sent.then(function(result) {
        sendHandle = null;
        trayStage = 'editing';
        if (result === null) {
          trayNotice = {text: 'Undone. Nothing was sent; the entries stay pending.', error: false};
        } else {
          dropSent(batch);
          if (result.dispatch_error) {
            trayNotice = {
              text: 'Sent ' + count + ' to ' + label + ', but the session did not start a run: ' +
                result.dispatch_error,
              error: true,
            };
          } else {
            trayNotice = {text: 'Sent ' + count + ' to ' + label + '.', error: false};
          }
          if (batch.some(function(entry) { return entry.kind === ENTRY_KIND.PICK; })) refreshStatusViewAfterPick();
        }
        refreshTray();
      }, function(err) {
        sendHandle = null;
        trayStage = 'editing';
        console.error('Batch send failed:', err);
        trayNotice = {
          text: 'Not sent: ' + err.message + '. The entries are still pending; press Send to try again.',
          error: true,
        };
        refreshTray();
      });
    }

    function undoSend() {
      if (sendHandle) sendHandle.cancel();
    }

    // Takes the sent entries out of the pending list and records each sent pick on its question, which then holds
    // its answer until a reload. Entries not in the batch stay.
    function dropSent(batch) {
      pending = pending.filter(function(entry) { return batch.indexOf(entry) === -1; });
      for (var i = 0; i < batch.length; i++) {
        var entry = batch[i];
        if (entry.kind === ENTRY_KIND.PICK && entry.el) {
          for (var j = 0; j < picks.length; j++) {
            if (picks[j].fork === entry.el) picks[j].answered = entry.comment;
          }
        }
        if (entry.kind === ENTRY_KIND.BLOCK && entry.el) unmarkBlockIfUnused(entry.el);
      }
      if (pending.length === 0) clearDraft(artifactPath);
      else saveDraft(pending, artifactPath);
    }

    // The block marker goes once no pending block comment still anchors to the block.
    function unmarkBlockIfUnused(el) {
      for (var i = 0; i < pending.length; i++) {
        if (pending[i].kind === ENTRY_KIND.BLOCK && pending[i].el === el) return;
      }
      el.classList.remove(GLOBAL_PREFIX + '-marked');
    }

    // ---- Option picks on the page's open questions ----
    //
    // The parser reads the grammar of the Rust checker's page_forks (artifact_check.rs).
    // An open div.fork holds no p.resolved, its question number is span.fn in a p.q, and its
    // options are the li lines outside every closed details and every div.demo. Each option
    // opens with its name in 「」.
    // The shared fixture plan_template_forks.json pins this parser and the Rust one to one output.
    function isPickMarker(el) {
      return el.classList.contains('demo') || el.classList.contains('no-demo');
    }

    function collapseText(text) {
      return String(text || '').replace(/\s+/g, ' ').trim();
    }

    // The option line's text without the demo and reason it may carry: they draw or explain the
    // option and name none.
    function optionLineText(el) {
      var text = '';
      for (var i = 0; i < el.childNodes.length; i++) {
        var node = el.childNodes[i];
        if (node.nodeType === 3) text += node.textContent;
        else if (node.nodeType === 1 && !isPickMarker(node)) text += optionLineText(node);
      }
      return text;
    }

    function optionNameOf(text) {
      var rest = text.replace(/^\s+/, '');
      if (rest.charAt(0) !== '「') return null;
      var close = rest.indexOf('」');
      if (close === -1) return null;
      var name = rest.slice(1, close).trim();
      return name && rest.slice(close + 1).trim() ? name : null;
    }

    function quotedSpansOf(text) {
      var spans = [];
      var i = 0;
      while (i < text.length) {
        var closer = QUOTE_CLOSERS[text.charAt(i)];
        if (!closer) {
          i += 1;
          continue;
        }
        var end = text.indexOf(closer, i + 1);
        if (end === -1) break;
        spans.push(text.slice(i + 1, end).trim());
        i = end + 1;
      }
      return spans;
    }

    // True when a closed details lies between el and top (top null: among all ancestors).
    function isFoldedBelow(el, top) {
      for (var node = el.parentElement; node && node !== top; node = node.parentElement) {
        if (node.tagName === 'DETAILS' && !node.open) return true;
      }
      return false;
    }

    function insideDemoBelow(el, top) {
      for (var node = el.parentElement; node && node !== top; node = node.parentElement) {
        if (node.classList.contains('demo')) return true;
      }
      return false;
    }

    function forkNumber(fork) {
      var questions = fork.querySelectorAll('.q');
      for (var i = 0; i < questions.length; i++) {
        if (questions[i].tagName !== 'P') continue;
        var spans = questions[i].querySelectorAll('.fn');
        for (var j = 0; j < spans.length; j++) {
          if (spans[j].tagName !== 'SPAN') continue;
          var number = collapseText(spans[j].textContent);
          if (number) return number;
        }
      }
      return '';
    }

    function forkIsResolved(fork) {
      var marks = fork.querySelectorAll('.resolved');
      for (var i = 0; i < marks.length; i++) {
        if (marks[i].tagName === 'P') return true;
      }
      return false;
    }

    // One record per pickable open fork. The record holds the fork's number and its named options.
    // Each option carries the line text and, once rendered, its button. The record also holds the
    // index of the recommended option or -1.
    function pageForkRecords() {
      var records = [];
      var blocks = document.querySelectorAll('.fork');
      for (var i = 0; i < blocks.length; i++) {
        var fork = blocks[i];
        if (fork.tagName !== 'DIV' || forkIsResolved(fork) || isFoldedBelow(fork, null)) continue;
        var number = forkNumber(fork);
        if (!number) continue;
        var options = [];
        var lines = fork.querySelectorAll('li');
        for (var j = 0; j < lines.length; j++) {
          if (isFoldedBelow(lines[j], fork) || insideDemoBelow(lines[j], fork)) continue;
          var text = collapseText(optionLineText(lines[j]));
          var name = optionNameOf(text);
          if (name) options.push({name: name, text: text, line: lines[j], button: null});
        }
        if (options.length < 2) continue;
        var rec = -1;
        var recLines = fork.querySelectorAll('.rec');
        var recLine = null;
        for (var k = 0; k < recLines.length; k++) {
          if (recLines[k].tagName === 'P') {
            recLine = recLines[k];
            break;
          }
        }
        if (recLine) {
          var spans = quotedSpansOf(collapseText(recLine.textContent));
          for (var s = 0; s < spans.length && rec === -1; s++) {
            for (var o = 0; o < options.length; o++) {
              if (options[o].name === spans[s]) {
                rec = o;
                break;
              }
            }
          }
        }
        // answered: the option text of the pick that landed, or null until one does.
        records.push({fork: fork, n: number, options: options, rec: rec, answered: null});
      }
      return records;
    }

    // A question holds at most one pick, and the pick is a pending entry: anchored to the question's block and
    // carrying the option's text. A pick leaves the pending list when its message lands; its question then holds
    // the answer until a reload.
    function pickEntryOf(record) {
      for (var i = 0; i < pending.length; i++) {
        if (pending[i].kind === ENTRY_KIND.PICK && pending[i].el === record.fork) return pending[i];
      }
      return null;
    }

    function optionIndexOf(record, text) {
      for (var i = 0; i < record.options.length; i++) {
        if (record.options[i].text === text) return i;
      }
      return -1;
    }

    function pickEntry(record, option) {
      return {
        kind: ENTRY_KIND.PICK,
        el: record.fork,
        quote: '',
        context: 'Q' + record.n,
        comment: option.text,
        occurrence: 0,
      };
    }

    // The questions with neither a pending pick nor an answer that landed.
    function unpickedRecords() {
      return picks.filter(function(record) { return record.answered === null && !pickEntryOf(record); });
    }

    function unpickedCount() {
      return unpickedRecords().length;
    }

    function unpickedRecommendedCount() {
      return unpickedRecords().filter(function(record) { return record.rec !== -1; }).length;
    }

    // Clicking an option picks it; clicking the picked option again takes the pick back.
    function togglePick(record, index) {
      var option = record.options[index];
      var entry = pickEntryOf(record);
      if (!entry) {
        pending.push(pickEntry(record, option));
      } else if (entry.comment === option.text) {
        pending.splice(pending.indexOf(entry), 1);
      } else {
        entry.comment = option.text;
      }
      saveDraft(pending, artifactPath);
      refreshTray();
    }

    // Each question that has no pick yet takes its recommended option.
    function restPicksAsRecommended() {
      var rested = unpickedRecords();
      for (var i = 0; i < rested.length; i++) {
        if (rested[i].rec !== -1) pending.push(pickEntry(rested[i], rested[i].options[rested[i].rec]));
      }
      saveDraft(pending, artifactPath);
      refreshTray();
    }

    // Each option button shows its question's pick: the pending entry's option, or the answer that landed. The
    // buttons are off outside the editing stage, and on a question that has been answered.
    function renderPicks() {
      var idle = trayStage === 'editing';
      for (var i = 0; i < picks.length; i++) {
        var record = picks[i];
        var entry = pickEntryOf(record);
        var picked = optionIndexOf(record, entry ? entry.comment : record.answered);
        for (var j = 0; j < record.options.length; j++) {
          var option = record.options[j];
          option.button.disabled = !idle || record.answered !== null;
          option.button.setAttribute('aria-pressed', picked === j ? 'true' : 'false');
          setIcon(option.mark, picked === j ? 'done' : null, 12);
        }
      }
    }

    // A pick that landed refreshes the Status view of the chat page this artifact sits in. The chat
    // page's refresh does nothing when no Status view is open.
    function refreshStatusViewAfterPick() {
      if (!framed) return;
      var refresh = window.parent.refreshSessionStatusPanel;
      if (typeof refresh === 'function') refresh();
    }

    // An icon slot holds one cbIcon drawing. setIcon fills it; an empty slot draws nothing.
    function makeIconSlot() {
      var slot = document.createElement('span');
      slot.className = GLOBAL_PREFIX + '-icon';
      return slot;
    }

    // cbIcon is the icon module's function. It throws on a meaning the table lacks, so no control draws without one.
    function setIcon(slot, meaning, size) {
      slot.innerHTML = meaning ? window.cbIcon(meaning, size) : '';
    }

    function iconSlotFor(meaning, size) {
      var slot = makeIconSlot();
      setIcon(slot, meaning, size);
      return slot;
    }

    // A button with an optional leading icon slot. The label sits in its own span, so the button's text is the label.
    function makeButton(label, onClick, className, leading) {
      var button = document.createElement('button');
      button.type = 'button';
      button.className = className;
      if (leading) button.appendChild(leading);
      var text = document.createElement('span');
      text.textContent = label;
      button.appendChild(text);
      button.addEventListener('click', onClick);
      return button;
    }

    // One row of buttons under the question's option lines. The row carries the layer marker, so the
    // comment layer neither quotes it nor anchors a comment to it.
    function installPickRow(record) {
      var row = document.createElement('div');
      row.className = GLOBAL_PREFIX + '-picks';
      row.classList.add(UI_CLASS);
      record.options.forEach(function(option, index) {
        option.mark = makeIconSlot();
        var button = makeButton(option.name, function() { togglePick(record, index); }, GLOBAL_PREFIX + '-pick',
          option.mark);
        option.button = button;
        row.appendChild(button);
      });
      var last = record.options[record.options.length - 1].line;
      var anchor = last.parentElement === record.fork ? last : last.parentElement;
      anchor.parentNode.insertBefore(row, anchor.nextSibling);
    }

    function installPicks() {
      if (!PICK_PAGE_PATTERN.test(artifactPath)) return;
      picks = pageForkRecords();
      picks.forEach(installPickRow);
    }

    // The page's pickable forks in the shape its fixture holds: the number, the named options as
    // the page writes them, and the recommended name.
    window.__cbcPageForks = function() {
      return pageForkRecords().map(function(record) {
        return {
          n: record.n,
          options: record.options.map(function(option) { return {name: option.name, text: option.text}; }),
          rec: record.rec === -1 ? null : record.options[record.rec].name,
        };
      });
    };

    function reposition() {
      layoutColumn();
      if (hovered && !popover) positionTrigger(hovered);
    }
  })();
}
