// ---------------------------------------------------------------------------
// Session status panel (chat column)
// ---------------------------------------------------------------------------
// The panel under the view toggle, above the message list, shows a web session's
// Goal, Now and Waiting on you, and in a parent session the Waiting on you items
// of its open descendant sessions. A web session is one that Chat.ForYou.isWebSession
// (for-you.js) accepts. A thread session and a worker session show no panel.
//
// The status is the session's web_status at open, then the web_status messages of
// this session, live or in history: the newest updated_at wins, so an older page
// of history never moves the panel back. The descendant rows come from
// GET /api/internal/web/status, fetched at open and on each web_status_subtree
// notice (websocket-handlers.js).
(function() {
  const Chat = globalThis.Chat;

  const PANEL_ID = 'chat-status-panel';
  // An item's link is text the agent wrote. Only web and host-absolute URLs become
  // anchors: any other scheme, javascript: above all, would run in this page.
  const SAFE_LINK = /^(https?:\/\/|\/)/;

  // The session the page shows the panel for, and what the panel draws from. A
  // null panelSessionId means the page shows no panel.
  let panelSessionId = null;
  let panelStatus = null;
  let panelDescendants = [];
  let panelChildrenOpen = false;
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

  function statusRow(kind) {
    const row = document.createElement('div');
    row.className = 'flex gap-3';
    row.dataset.statusRow = kind;
    return row;
  }

  function labelSpan(text) {
    return textSpan('shrink-0 w-28 text-slate-500', text);
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

  function waitingItem(item) {
    const el = document.createElement('div');
    el.className = 'flex gap-2 break-words text-amber-200';
    el.dataset.waitingItem = 'true';
    el.appendChild(textSpan('min-w-0', item.text));
    const href = safeLink(item.link);
    if (href) el.appendChild(openLink(href));
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
    el.className = 'flex gap-2 pl-6 break-words text-amber-200';
    el.dataset.childItem = 'true';
    el.appendChild(textSpan('min-w-0', prefix + item.text));
    el.appendChild(openLink('/?session=' + encodeURIComponent(sessionId)));
    return el;
  }

  // The row that counts the descendants' items and, opened, lists them. It starts folded.
  function childrenBlock() {
    const count = panelDescendants.reduce((total, row) => total + row.waiting.length, 0);
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

  function renderPanel() {
    const el = document.getElementById(PANEL_ID);
    if (!el) return;
    clearChildren(el);
    if (!panelSessionId) {
      el.classList.add('hidden');
      return;
    }
    el.classList.remove('hidden');
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
    absorbWebStatusMessages(messages);
    renderPanel();
    fetchDescendants(session.id);
  }

  function clearSessionStatusPanel() {
    panelSessionId = null;
    panelStatus = null;
    panelDescendants = [];
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

  Chat.wire({
    openSessionStatusPanel,
    clearSessionStatusPanel,
    observeWebStatusMessages,
    refreshSessionStatusPanel,
  });

})();
