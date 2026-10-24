// ---------------------------------------------------------------------------
// Session status panel (chat column)
// ---------------------------------------------------------------------------
// The panel under the view toggle, above the message list, shows a web session's
// Goal, Now and Waiting on you, how long each waiting item has waited, and in a
// parent session the Waiting on you items of its open descendant sessions and a
// folded block of the Now of its open direct children. A web session is one that
// Chat.ForYou.isWebSession (for-you.js) accepts.
// A thread session and a worker session show no panel. Both platforms draw the
// toggle row; the phone starts folded and desktop state persists in localStorage.
//
// The status is the session's web_status at open, then the web_status messages of
// this session, live or in history: the newest updated_at wins, so an older page
// of history never moves the panel back. The descendant and child rows come from
// GET /api/internal/web/status, fetched at open and on each web_status_subtree
// notice (websocket-handlers.js).
(function() {
  const Chat = globalThis.Chat;

  const PANEL_ID = 'chat-status-panel';
  const PANEL_EXPANDED_STORAGE_KEY = 'chat-status-panel-expanded';
  // An item's link is text the agent wrote. Only web and host-absolute URLs become
  // anchors: any other scheme, javascript: above all, would run in this page.
  const SAFE_LINK = /^(https?:\/\/|\/)/;

  // The session the page shows the panel for, and what the panel draws from. A
  // null panelSessionId means the page shows no panel. panelExpanded holds the
  // phone's tap state until reload; desktop reads its choice from localStorage.
  let panelSessionId = null;
  let panelStatus = null;
  let panelDescendants = [];
  let panelChildrenOpen = false;
  let panelChildren = [];
  let panelChildrenNowOpen = false;
  let panelExpanded = false;
  let panelFetchGen = 0;

  function textSpan(className, text) {
    const span = document.createElement('span');
    span.className = className;
    span.textContent = text;
    return span;
  }

  function clearChildren(el) {
    while (el.firstChild) el.removeChild(el.firstChild);
  }

  function safeLink(link) {
    return link && SAFE_LINK.test(link) ? link : null;
  }

  function openLink(href) {
    const a = document.createElement('a');
    a.className = 'shrink-0 whitespace-nowrap text-blue-400 hover:text-blue-300';
    a.href = href;
    a.dataset.openLink = 'true';
    a.textContent = 'Open ›';
    return a;
  }

  // A link run inside an item's context: a web URL or a host path that starts a word (so
  // "and/or" is prose) and does not end in punctuation. safeLink still decides whether it
  // becomes an anchor, so the context cannot introduce a scheme the item's link could not.
  const CONTEXT_LINK = /(?:https?:\/\/|(?<=^|[\s(])\/)[^\s<>"']*[^\s<>"'.,;:!?)\]]/g;

  // The context text, with each link run rendered through safeLink.
  function appendContextText(parent, text) {
    let cursor = 0;
    for (const match of text.matchAll(CONTEXT_LINK)) {
      if (match.index > cursor) parent.appendChild(textSpan('', text.slice(cursor, match.index)));
      const anchor = document.createElement('a');
      anchor.className = 'break-all text-blue-400 hover:text-blue-300';
      anchor.href = safeLink(match[0]);
      anchor.target = '_blank';
      anchor.rel = 'noopener noreferrer';
      anchor.textContent = match[0];
      parent.appendChild(anchor);
      cursor = match.index + match[0].length;
    }
    if (cursor < text.length) parent.appendChild(textSpan('', text.slice(cursor)));
  }

  // An item's context: one fold under the item's line, closed until the operator clicks it.
  function contextFold(context) {
    const block = document.createElement('div');
    block.className = 'mt-1';
    block.dataset.context = 'true';
    const toggle = document.createElement('button');
    toggle.type = 'button';
    toggle.className = 'flex items-center gap-1 text-xs text-slate-400 hover:text-slate-200 transition-colors';
    toggle.dataset.contextToggle = 'true';
    toggle.setAttribute('aria-expanded', 'false');
    toggle.appendChild(textSpan('w-3 text-center', '▸'));
    toggle.appendChild(textSpan('', 'Context'));
    const body = document.createElement('div');
    body.className = 'hidden mt-1 whitespace-pre-wrap text-xs text-slate-300';
    body.dataset.contextBody = 'true';
    appendContextText(body, context);
    toggle.onclick = () => {
      const open = body.classList.contains('hidden');
      body.classList.toggle('hidden', !open);
      toggle.setAttribute('aria-expanded', String(open));
      toggle.children[0].textContent = open ? '▾' : '▸';
    };
    block.appendChild(toggle);
    block.appendChild(body);
    return block;
  }

  // A phone stacks each label above its value at full width; the desktop keeps a label column.
  function statusRow(kind) {
    const row = document.createElement('div');
    row.className = platform.isMobile ? 'flex flex-col gap-1' : 'flex gap-3';
    row.dataset.statusRow = kind;
    return row;
  }

  function labelSpan(text) {
    return textSpan(platform.isMobile ? 'text-slate-500' : 'shrink-0 w-28 text-slate-500', text);
  }

  function goalRow(goal) {
    const row = statusRow('goal');
    row.appendChild(labelSpan('Goal'));
    row.appendChild(textSpan('min-w-0 break-words text-slate-200', goal || ''));
    return row;
  }

  function nowRow(status) {
    const row = statusRow('now');
    row.appendChild(labelSpan('Now'));
    const value = document.createElement('div');
    value.className = 'min-w-0 break-words text-slate-200';
    value.appendChild(textSpan('', status.now || ''));
    const updated = textSpan('ml-2 whitespace-nowrap text-slate-500', 'updated ' + relativeTime(status.updated_at));
    updated.dataset.statusUpdated = status.updated_at;
    value.appendChild(updated);
    row.appendChild(value);
    return row;
  }

  // How long an item has waited, in hours or days. An item whose start time is a
  // lower bound (since_exact false) reads with the bound mark in front.
  const HOUR_MS = 60 * 60 * 1000;
  function waitedAge(item) {
    if (!item.since) return null;
    const hours = Math.floor(Math.max(0, Date.now() - Date.parse(item.since)) / HOUR_MS);
    const value = hours >= 24 ? Math.floor(hours / 24) + 'd' : hours + 'h';
    return (item.since_exact === false ? '至少 ' : '') + value;
  }

  function waitedAgeSpan(item) {
    const age = waitedAge(item);
    if (!age) return null;
    const span = textSpan('whitespace-nowrap text-slate-500', age);
    span.dataset.waitingAge = 'true';
    return span;
  }

  function waitingItem(item) {
    const el = document.createElement('div');
    el.className = 'break-words text-amber-200';
    el.dataset.waitingItem = 'true';
    const line = document.createElement('div');
    line.className = 'flex gap-2';
    line.appendChild(textSpan('min-w-0', item.text));
    const age = waitedAgeSpan(item);
    if (age) line.appendChild(age);
    const href = safeLink(item.link);
    if (href) line.appendChild(openLink(href));
    el.appendChild(line);
    if (item.context) el.appendChild(contextFold(item.context));
    return el;
  }

  function waitingRow(items) {
    const row = statusRow('waiting');
    row.appendChild(labelSpan('Waiting on you'));
    const column = document.createElement('div');
    column.className = 'min-w-0 flex-1 space-y-1';
    if (!items.length) {
      const nothing = textSpan('text-slate-400', 'Nothing');
      nothing.dataset.waitingEmpty = 'true';
      column.appendChild(nothing);
    }
    for (const item of items) column.appendChild(waitingItem(item));
    row.appendChild(column);
    return row;
  }

  function childItem(prefix, item, sessionId) {
    const el = document.createElement('div');
    el.className = 'pl-6 break-words text-amber-200';
    el.dataset.childItem = 'true';
    const line = document.createElement('div');
    line.className = 'flex gap-2';
    line.appendChild(textSpan('min-w-0', prefix + item.text));
    const age = waitedAgeSpan(item);
    if (age) line.appendChild(age);
    line.appendChild(openLink('/?session=' + encodeURIComponent(sessionId)));
    el.appendChild(line);
    if (item.context) el.appendChild(contextFold(item.context));
    return el;
  }

  function descendantItemCount() {
    return panelDescendants.reduce((total, row) => total + row.waiting.length, 0);
  }

  // The shared toggle row counts Waiting on you items and any descendant items.
  function toggleRow(expanded) {
    const row = document.createElement('button');
    row.type = 'button';
    row.className = 'sticky -top-2 z-10 bg-slate-900 block w-full text-left text-slate-200 hover:text-slate-100 transition-colors';
    row.dataset.statusRow = 'toggle';
    row.setAttribute('aria-expanded', String(expanded));
    row.onclick = () => {
      const nextExpanded = !panelIsExpanded();
      if (platform.isMobile) {
        panelExpanded = nextExpanded;
      } else {
        localStorage.setItem(PANEL_EXPANDED_STORAGE_KEY, nextExpanded ? '1' : '0');
      }
      renderPanel();
    };
    let summary = 'No status yet';
    if (panelStatus) {
      const waiting = panelStatus.waiting.length;
      row.dataset.waitingCount = String(waiting);
      summary = 'Waiting on you · ' + (waiting || 'Nothing');
    }
    const childCount = descendantItemCount();
    row.dataset.childCount = String(childCount);
    if (childCount) summary += ' · In child sessions · ' + childCount;
    row.textContent = (expanded ? '▾' : '▸') + ' ' + summary;
    return row;
  }

  function panelIsExpanded() {
    if (platform.isMobile) return panelExpanded;
    const stored = localStorage.getItem(PANEL_EXPANDED_STORAGE_KEY);
    if (stored === null || stored === '1') return true;
    if (stored === '0') return false;
    throw new Error('Invalid desktop status panel state: ' + stored);
  }

  // The row that counts the descendants' items and, opened, lists them. It starts folded.
  function childrenBlock() {
    const count = descendantItemCount();
    const block = document.createElement('div');
    block.className = 'space-y-1 pt-1';
    block.dataset.statusRow = 'children';
    const toggle = document.createElement('button');
    toggle.type = 'button';
    toggle.className = 'flex items-center gap-2 w-full text-left text-slate-400 hover:text-slate-200 transition-colors';
    toggle.dataset.childCount = String(count);
    toggle.setAttribute('aria-expanded', String(panelChildrenOpen));
    toggle.onclick = () => {
      panelChildrenOpen = !panelChildrenOpen;
      renderPanel();
    };
    toggle.appendChild(textSpan('w-3 text-center', panelChildrenOpen ? '▾' : '▸'));
    toggle.appendChild(textSpan('', 'In child sessions · ' + count));
    block.appendChild(toggle);
    if (panelChildrenOpen) {
      for (const row of panelDescendants) {
        const prefix = row.name + ' · ' + row.session_id.slice(0, 8) + ': ';
        for (const item of row.waiting) block.appendChild(childItem(prefix, item, row.session_id));
      }
    }
    return block;
  }

  function childNowItem(row) {
    const el = document.createElement('div');
    el.className = 'flex gap-2 pl-6 break-words text-slate-200';
    el.dataset.childNowItem = 'true';
    el.appendChild(textSpan('min-w-0', row.name + ' · ' + row.session_id.slice(0, 8) + ': ' + row.now));
    const updated = textSpan('whitespace-nowrap text-slate-500', 'updated ' + relativeTime(row.updated_at));
    updated.dataset.statusUpdated = row.updated_at;
    el.appendChild(updated);
    el.appendChild(openLink('/?session=' + encodeURIComponent(row.session_id)));
    return el;
  }

  // The folded block of the open direct children's Now, one row each when opened. It starts folded.
  function childrenNowBlock() {
    const block = document.createElement('div');
    block.className = 'space-y-1 pt-1';
    block.dataset.statusRow = 'children-now';
    const toggle = document.createElement('button');
    toggle.type = 'button';
    toggle.className = 'flex items-center gap-2 w-full text-left text-slate-400 hover:text-slate-200 transition-colors';
    toggle.dataset.childCount = String(panelChildren.length);
    toggle.setAttribute('aria-expanded', String(panelChildrenNowOpen));
    toggle.onclick = () => {
      panelChildrenNowOpen = !panelChildrenNowOpen;
      renderPanel();
    };
    toggle.appendChild(textSpan('w-3 text-center', panelChildrenNowOpen ? '▾' : '▸'));
    toggle.appendChild(textSpan('', "Child sessions' Now · " + panelChildren.length));
    block.appendChild(toggle);
    if (panelChildrenNowOpen) {
      for (const row of panelChildren) block.appendChild(childNowItem(row));
    }
    return block;
  }

  function renderPanel() {
    const el = document.getElementById(PANEL_ID);
    if (!el) return;
    clearChildren(el);
    if (!panelSessionId) {
      el.classList.add('hidden');
      return;
    }
    el.classList.remove('hidden');
    // Desktop scrolls within its height cap; an expanded phone panel uses its full height.
    el.classList.toggle('max-h-48', !platform.isMobile);
    const expanded = panelIsExpanded();
    el.appendChild(toggleRow(expanded));
    if (!expanded) return;
    if (panelStatus) {
      el.appendChild(goalRow(panelStatus.goal));
      el.appendChild(nowRow(panelStatus));
      el.appendChild(waitingRow(panelStatus.waiting));
    } else {
      const row = statusRow('none');
      row.appendChild(textSpan('text-slate-400', 'No status yet'));
      el.appendChild(row);
    }
    if (panelDescendants.length) el.appendChild(childrenBlock());
    if (panelChildren.length) el.appendChild(childrenNowBlock());
  }

  // A web_status message's status replaces the panel's, unless the panel already
  // holds a newer one.
  function absorbWebStatusMessages(messages) {
    for (const msg of messages) {
      if (msg.role !== 'web_status') continue;
      const next = msg.web_status;
      if (panelStatus && Date.parse(next.updated_at) < Date.parse(panelStatus.updated_at)) continue;
      panelStatus = next;
    }
  }

  // The descendant rows: one GET per open and per subtree notice. A response for a
  // session the page no longer shows, or one overtaken by a later GET, is dropped.
  async function fetchDescendants(sessionId) {
    const gen = ++panelFetchGen;
    let body;
    try {
      const res = await fetch('/api/internal/web/status?session_id=' + encodeURIComponent(sessionId));
      if (!res.ok) throw new Error('HTTP ' + res.status);
      body = await res.json();
    } catch (err) {
      console.error('web status fetch failed:', err);
      return;
    }
    if (gen !== panelFetchGen || sessionId !== panelSessionId) return;
    panelDescendants = body.descendants;
    panelChildren = body.children;
    renderPanel();
  }

  // Session open (first load and SPA switch): the session's own status, then the
  // history's web_status messages, then the descendant rows.
  function openSessionStatusPanel(session, messages) {
    if (!Chat.ForYou.isWebSession(session)) {
      clearSessionStatusPanel();
      return;
    }
    panelSessionId = session.id;
    panelStatus = session.web_status || null;
    panelDescendants = [];
    panelChildrenOpen = false;
    panelChildren = [];
    panelChildrenNowOpen = false;
    absorbWebStatusMessages(messages);
    renderPanel();
    fetchDescendants(session.id);
  }

  function clearSessionStatusPanel() {
    panelSessionId = null;
    panelStatus = null;
    panelDescendants = [];
    panelChildren = [];
    renderPanel();
  }

  // A live or catch-up message of this session: the panel redraws only when a
  // web_status message changes the status.
  function observeWebStatusMessages(messages) {
    if (!panelSessionId) return;
    const before = panelStatus;
    absorbWebStatusMessages(messages);
    if (panelStatus !== before) renderPanel();
  }

  // A descendant session set its status: the page re-reads the child rows.
  function refreshSessionStatusPanel() {
    if (panelSessionId) fetchDescendants(panelSessionId);
  }

  // A fold-out or rotation changes the platform layout, so redraw the panel.
  platform.onChange(renderPanel);

  Chat.wire({
    openSessionStatusPanel,
    clearSessionStatusPanel,
    observeWebStatusMessages,
    refreshSessionStatusPanel,
  });

})();
