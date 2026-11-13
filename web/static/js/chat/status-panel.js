// ---------------------------------------------------------------------------
// Session status panel (the Status view)
// ---------------------------------------------------------------------------
// A web session's status shows in the Status view (#status-panel). That panel
// takes the message list's place in the Chat column while Status is selected in
// the view toggle. The selection and the panel's visibility belong to
// chat/for-you.js's applyChatView; this file renders the panel's content and
// the waiting count on the toggle option's badge. A thread session and a worker
// session show neither the option nor the badge. A web session is one that
// Chat.ForYou.isWebSession (for-you.js) accepts.
//
// The status is the session's web_status at open, then the web_status messages
// of this session, live or in history. The newest updated_at wins, so an older
// page of history never moves the view back. The descendant and child rows
// come from GET /api/internal/web/status, fetched at open and on each
// web_status_subtree notice (websocket-handlers.js).
(function() {
  const Chat = globalThis.Chat;

  const VIEW_ID = 'status-panel';
  const BADGE_ID = 'status-waiting-count';
  // An item's link is text the agent wrote. Only web and host-absolute URLs become
  // anchors: any other scheme, javascript: above all, would run in this page.
  const SAFE_LINK = /^(https?:\/\/|\/)/;

  // The session the page shows the status for, and what it draws from. A null
  // panelSessionId means the page shows none of the surfaces.
  let panelSessionId = null;
  let panelStatus = null;
  let panelDescendants = [];
  let panelChildren = [];
  let panelDerived = [];
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

  // A fold's mark: the closed and open glyphs every fold in the panel shows.
  function foldGlyph(open) {
    return open ? '▾' : '▸';
  }

  // A fold toggle: the button, its glyph span and its label span are the same
  // in every fold. A caller names the style, the state, the label and the click.
  function foldToggle(className, open, label, onToggle) {
    const toggle = document.createElement('button');
    toggle.type = 'button';
    toggle.className = className;
    toggle.setAttribute('aria-expanded', String(open));
    toggle.appendChild(textSpan('w-3 text-center', foldGlyph(open)));
    toggle.appendChild(textSpan('', label));
    toggle.onclick = onToggle;
    return toggle;
  }

  // A fold's state as its toggle shows it: aria-expanded for assistive tech, the glyph
  // for the eye.
  function setFoldOpen(toggle, open) {
    toggle.setAttribute('aria-expanded', String(open));
    toggle.children[0].textContent = foldGlyph(open);
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

  // An item's context: one fold under the item's line. Own items open with their
  // context shown; descendant items keep it folded until the operator clicks.
  function contextFold(context, open) {
    const block = document.createElement('div');
    block.className = 'mt-1';
    block.dataset.context = 'true';
    const body = document.createElement('div');
    body.className = (open ? '' : 'hidden ') + 'mt-1 whitespace-pre-wrap text-xs text-slate-300';
    body.dataset.contextBody = 'true';
    appendContextText(body, context);
    const toggle = foldToggle(
      'flex items-center gap-1 text-xs text-slate-400 hover:text-slate-200 transition-colors',
      open,
      'Context',
      () => {
        const nextOpen = body.classList.contains('hidden');
        body.classList.toggle('hidden', !nextOpen);
        setFoldOpen(toggle, nextOpen);
      });
    toggle.dataset.contextToggle = 'true';
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
    return (item.since_exact === false ? 'at least ' : '') + value;
  }

  function waitedAgeSpan(item) {
    const age = waitedAge(item);
    if (!age) return null;
    const span = textSpan('shrink-0 whitespace-nowrap text-slate-500', age);
    span.dataset.waitingAge = 'true';
    return span;
  }

  // A Waiting on you item. contextOpen: the Status view shows an own item's
  // context open; the descendant rows keep it folded.
  function waitingItem(item, contextOpen) {
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
    if (item.context) el.appendChild(contextFold(item.context, contextOpen));
    return el;
  }

  function childItem(item, sessionId) {
    const el = document.createElement('div');
    el.className = 'break-words text-amber-200';
    el.dataset.childItem = 'true';
    const line = document.createElement('div');
    line.className = 'flex gap-2';
    line.appendChild(textSpan('min-w-0', item.text));
    const age = waitedAgeSpan(item);
    if (age) line.appendChild(age);
    line.appendChild(openLink('/?session=' + encodeURIComponent(sessionId)));
    el.appendChild(line);
    if (item.context) el.appendChild(contextFold(item.context, false));
    return el;
  }

  // The derived Waiting on you items the server holds for one session of the tree, such as a host login it awaits.
  function derivedFor(sessionId) {
    return panelDerived.filter((item) => item.session_id === sessionId);
  }

  // ---- Status panel --------------------------------------------------------

  // The descendants' items, grouped by session: one header per session, its
  // items under it with their contexts folded. A session with no items shows no group.
  function childrenRow() {
    const row = statusRow('children');
    row.appendChild(labelSpan('In child sessions'));
    const column = document.createElement('div');
    column.className = 'min-w-0 flex-1 space-y-2';
    for (const child of panelDescendants) {
      const group = document.createElement('div');
      group.className = 'space-y-1';
      const header = textSpan('text-slate-400', child.name + ' · ' + child.session_id.slice(0, 8));
      header.dataset.childGroup = child.session_id;
      group.appendChild(header);
      const items = child.waiting.concat(derivedFor(child.session_id));
      for (const item of items) group.appendChild(childItem(item, child.session_id));
      if (items.length) column.appendChild(group);
    }
    row.appendChild(column);
    return row;
  }

  function childNowItem(row) {
    const el = document.createElement('div');
    el.className = 'flex gap-2 break-words text-slate-200';
    el.dataset.childNowItem = 'true';
    el.appendChild(textSpan('min-w-0', row.name + ' · ' + row.session_id.slice(0, 8) + ': ' + row.now));
    const updated = textSpan('whitespace-nowrap text-slate-500', 'updated ' + relativeTime(row.updated_at));
    updated.dataset.statusUpdated = row.updated_at;
    el.appendChild(updated);
    el.appendChild(openLink('/?session=' + encodeURIComponent(row.session_id)));
    return el;
  }

  // The open direct children's Now, one row each.
  function childrenNowRow() {
    const row = statusRow('children-now');
    row.appendChild(labelSpan("Child sessions' Now"));
    const column = document.createElement('div');
    column.className = 'min-w-0 flex-1 space-y-1';
    for (const child of panelChildren) column.appendChild(childNowItem(child));
    row.appendChild(column);
    return row;
  }

  // The Waiting on you row: the session's items, each with its context open.
  function waitingRow(items) {
    const waiting = statusRow('waiting');
    waiting.appendChild(labelSpan('Waiting on you'));
    const column = document.createElement('div');
    column.className = 'min-w-0 flex-1 space-y-1';
    if (!items.length) {
      const nothing = textSpan('text-slate-400', 'Nothing');
      nothing.dataset.waitingEmpty = 'true';
      column.appendChild(nothing);
    }
    for (const item of items) column.appendChild(waitingItem(item, true));
    waiting.appendChild(column);
    return waiting;
  }

  function statusPanelBody(el) {
    if (!panelStatus) {
      const none = statusRow('none');
      none.appendChild(textSpan('text-slate-400', 'No status yet'));
      el.appendChild(none);
      const held = derivedFor(panelSessionId);
      if (held.length) el.appendChild(waitingRow(held));
    } else {
      el.appendChild(goalRow(panelStatus.goal));
      el.appendChild(nowRow(panelStatus));
      el.appendChild(waitingRow(panelStatus.waiting.concat(derivedFor(panelSessionId))));
    }
    if (panelDescendants.length) el.appendChild(childrenRow());
    if (panelChildren.length) el.appendChild(childrenNowRow());
  }

  // The panel redraws on every status update, and the badge follows the
  // same state. The panel may be hidden while Status is not selected; its
  // content stays current, so selecting it shows the newest state.
  function renderPanel() {
    const view = document.getElementById(VIEW_ID);
    if (view) {
      clearChildren(view);
      if (panelSessionId) statusPanelBody(view);
    }
    renderWaitingBadge();
  }

  // The toggle option's waiting count holds this session's own stored Waiting on you
  // items. A child session's waiting items show in the Status view's child rows. Hidden at 0 and when
  // the page shows no web session.
  function renderWaitingBadge() {
    const badge = document.getElementById(BADGE_ID);
    if (!badge) return;
    const count = panelSessionId && panelStatus ? panelStatus.waiting.length : 0;
    badge.textContent = count ? String(count) : '';
    badge.classList.toggle('hidden', !count);
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
      const res = await fetch('/api/internal/web/status?session_id=' + encodeURIComponent(sessionId),
          {signal: viewRequestSignal()});
      if (!res.ok) throw new Error('HTTP ' + res.status);
      body = await res.json();
    } catch (err) {
      console.error('web status fetch failed:', err);
      return;
    }
    if (gen !== panelFetchGen || sessionId !== panelSessionId) return;
    if (body.web_status && (!panelStatus || Date.parse(body.web_status.updated_at) >= Date.parse(panelStatus.updated_at))) {
      panelStatus = body.web_status;
    }
    panelDescendants = body.descendants;
    panelChildren = body.children;
    panelDerived = body.derived_waiting;
    renderPanel();
  }

  // Session open (first load and SPA switch): the session's own status, then the
  // history's web_status messages, then the descendant rows. Whether the panel
  // shows comes from the session's remembered view choice (chat/for-you.js
  // applyChatView, called by the same open path).
  function openSessionStatusPanel(session, messages) {
    if (!Chat.ForYou.isWebSession(session)) {
      clearSessionStatusPanel();
      return;
    }
    panelSessionId = session.id;
    panelStatus = session.web_status || null;
    panelDescendants = [];
    panelChildren = [];
    panelDerived = [];
    absorbWebStatusMessages(messages);
    renderPanel();
    fetchDescendants(session.id);
  }

  function clearSessionStatusPanel() {
    panelSessionId = null;
    panelStatus = null;
    panelDescendants = [];
    panelChildren = [];
    panelDerived = [];
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
