// ---------------------------------------------------------------------------
// Session status panel (chat column)
// ---------------------------------------------------------------------------
// A web session's status draws on two surfaces from one state. The summary
// strip (#chat-status-panel) sits above the message list in the For-you and
// Everything views: one line of counts and the Now text, then the session's
// own Waiting on you items, at most four lines on desktop and one on a phone.
// The full status (Status view, #chat-status-view) covers the message list
// area while the Status view is selected; the list stays laid out underneath
// so the turn engine keeps reading its real height. Chat.ForYou owns the
// toggle that selects the view (for-you.js); this file owns both surfaces.
// A thread session and a worker session show neither surface. A web session
// is one that Chat.ForYou.isWebSession (for-you.js) accepts.
//
// The status is the session's web_status at open, then the web_status messages of
// this session, live or in history: the newest updated_at wins, so an older page
// of history never moves the panel back. The descendant and child rows come from
// GET /api/internal/web/status, fetched at open and on each web_status_subtree
// notice (websocket-handlers.js).
(function() {
  const Chat = globalThis.Chat;

  const PANEL_ID = 'chat-status-panel';
  const VIEW_ID = 'chat-status-view';
  const SCROLL_BUTTON_ID = 'scroll-to-bottom';
  // An item's link is text the agent wrote. Only web and host-absolute URLs become
  // anchors: any other scheme, javascript: above all, would run in this page.
  const SAFE_LINK = /^(https?:\/\/|\/)/;
  // The view name for-yous's toggle carries for the full status.
  const STATUS_VIEW = 'status';
  // The desktop strip lists its own Waiting on you items up to this many before
  // the "+K more" line keeps the strip at four lines.
  const STRIP_ITEM_LINES = 3;

  // The session the page shows the status for, and what it draws from. A null
  // panelSessionId means the page shows neither surface. statusViewShown holds
  // the Status view's visibility; the strip hides while it is on.
  let panelSessionId = null;
  let panelStatus = null;
  let panelDescendants = [];
  let panelChildren = [];
  let statusViewShown = false;
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

  // A fold toggle: the button, its glyph span and its label span are the same in every
  // fold, so a caller names the style, the state, the label and the click.
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

  // The milestones between the session and its Goal: how many remain and when the
  // list was last set, then the open items in written order and the done items in
  // one fold. The text is agent-written, so it goes in through textContent like
  // the Goal.
  function remainingRow(status) {
    const row = statusRow('remaining');
    row.appendChild(labelSpan('Remaining'));
    const value = document.createElement('div');
    value.className = 'min-w-0 break-words text-slate-200 space-y-1';
    const open = status.remaining.filter((item) => !item.done).length;
    const count = textSpan('', open + ' of ' + status.remaining.length + ' left');
    count.dataset.remainingCount = String(open);
    value.appendChild(count);
    const updated = textSpan('ml-2 whitespace-nowrap text-slate-500',
      'updated ' + relativeTime(status.remaining_updated_at));
    updated.dataset.statusUpdated = status.remaining_updated_at;
    value.appendChild(updated);
    for (const item of status.remaining) {
      if (item.done) continue;
      const line = document.createElement('div');
      line.className = 'break-words';
      line.dataset.remainingItem = 'open';
      line.appendChild(textSpan('mr-2 text-slate-400', '○'));
      line.appendChild(textSpan('', item.text));
      value.appendChild(line);
    }
    const done = status.remaining.filter((item) => item.done);
    if (done.length) {
      const body = document.createElement('div');
      body.className = 'hidden space-y-1';
      body.dataset.remainingDone = 'true';
      for (const item of done) {
        const line = document.createElement('div');
        line.className = 'break-words';
        line.dataset.remainingItem = 'done';
        line.appendChild(textSpan('mr-2 text-slate-400', '✓'));
        line.appendChild(textSpan('', item.text));
        body.appendChild(line);
      }
      const toggle = foldToggle(
        'flex items-center gap-1 text-xs text-slate-400 hover:text-slate-200 transition-colors',
        false,
        done.length + ' done',
        () => {
          const nextOpen = body.classList.contains('hidden');
          body.classList.toggle('hidden', !nextOpen);
          setFoldOpen(toggle, nextOpen);
        });
      toggle.dataset.remainingDoneToggle = 'true';
      value.appendChild(toggle);
      value.appendChild(body);
    }
    row.appendChild(value);
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
    const span = textSpan('shrink-0 whitespace-nowrap text-slate-500', age);
    span.dataset.waitingAge = 'true';
    return span;
  }

  // A Waiting on you item. contextOpen: the Status view shows an own item's
  // context open; the strip and the descendant rows keep it folded.
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

  function descendantItemCount() {
    return panelDescendants.reduce((total, row) => total + row.waiting.length, 0);
  }

  // ---- Summary strip -------------------------------------------------------

  // The dot separator between the strip line's segments.
  function stripDot() {
    return textSpan('shrink-0 text-slate-600', '·');
  }

  // The strip's one clickable link to the Status view: line 1's "Status ›" and
  // the "+K more" line of a long own-item list.
  function statusViewLink(label, className) {
    const link = document.createElement('button');
    link.type = 'button';
    link.className = className;
    link.textContent = label;
    link.dataset.statusViewLink = 'true';
    link.onclick = () => Chat.setChatView(STATUS_VIEW);
    return link;
  }

  // Line 1: the Waiting on you count, the descendant count, the Remaining count,
  // the Now text cut to one line, and the link to the Status view. A phone keeps
  // the counts and the link only, under the short labels the narrow line holds.
  function stripLine1() {
    const phone = platform.isMobile;
    const line = document.createElement('div');
    line.className = 'flex items-center gap-2 min-w-0 overflow-hidden whitespace-nowrap';
    line.dataset.statusRow = 'summary';
    const waiting = panelStatus.waiting.length;
    const waitingSpan = textSpan('shrink-0 whitespace-nowrap text-slate-200',
      (phone ? 'Waiting · ' : 'Waiting on you · ') + (waiting || 'Nothing'));
    waitingSpan.dataset.waitingCount = String(waiting);
    line.appendChild(waitingSpan);
    const childCount = descendantItemCount();
    if (childCount) {
      line.appendChild(stripDot());
      const childSpan = textSpan('shrink-0 whitespace-nowrap text-slate-200',
        (phone ? 'Child · ' : 'In child sessions · ') + childCount);
      childSpan.dataset.childCount = String(childCount);
      line.appendChild(childSpan);
    }
    if (panelStatus.remaining && panelStatus.remaining.length) {
      const open = panelStatus.remaining.filter((item) => !item.done).length;
      line.appendChild(stripDot());
      const remainingSpan = textSpan('shrink-0 whitespace-nowrap text-slate-200',
        open + ' of ' + panelStatus.remaining.length + ' left');
      remainingSpan.dataset.remainingCount = String(open);
      line.appendChild(remainingSpan);
    }
    if (!phone && panelStatus.now) {
      line.appendChild(stripDot());
      const now = textSpan('min-w-0 truncate', 'Now: ' + panelStatus.now);
      now.dataset.nowText = panelStatus.now;
      line.appendChild(now);
    }
    line.appendChild(statusViewLink('Status ›', 'shrink-0 text-blue-400 hover:text-blue-300'));
    return line;
  }

  // The desktop strip's own-item lines: one per item up to three, then a
  // "+K more" line that opens the Status view. Each line cuts its text to one
  // line and carries the waited age and the item's safe link.
  function stripWaitingItem(item) {
    const line = document.createElement('div');
    line.className = 'flex items-baseline gap-2 min-w-0';
    line.dataset.statusRow = 'waiting-item';
    line.dataset.waitingItem = 'true';
    line.appendChild(textSpan('min-w-0 truncate text-amber-200', item.text));
    const age = waitedAgeSpan(item);
    if (age) line.appendChild(age);
    const href = safeLink(item.link);
    if (href) line.appendChild(openLink(href));
    return line;
  }

  function stripBody(el) {
    if (!panelStatus) {
      const row = statusRow('none');
      row.appendChild(textSpan('text-slate-400', 'No status yet'));
      el.appendChild(row);
      return;
    }
    el.appendChild(stripLine1());
    if (platform.isMobile) return;
    const items = panelStatus.waiting;
    // More items than the strip's lines hold: the first two make room for the
    // "+K more" line that opens the Status view.
    const shown = items.length > STRIP_ITEM_LINES ? STRIP_ITEM_LINES - 1 : items.length;
    for (const item of items.slice(0, shown)) el.appendChild(stripWaitingItem(item));
    if (items.length > STRIP_ITEM_LINES) {
      const more = statusViewLink('+' + (items.length - shown) + ' more',
        'text-left text-slate-400 hover:text-slate-200 transition-colors');
      more.dataset.moreCount = String(items.length - shown);
      el.appendChild(more);
    }
  }

  // ---- Status view ---------------------------------------------------------

  // The descendants' items, grouped by session: one header per session, its
  // items under it with their contexts folded.
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
      for (const item of child.waiting) group.appendChild(childItem(item, child.session_id));
      column.appendChild(group);
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

  function statusViewBody(el) {
    if (!panelStatus) {
      const none = statusRow('none');
      none.appendChild(textSpan('text-slate-400', 'No status yet'));
      el.appendChild(none);
    } else {
      el.appendChild(goalRow(panelStatus.goal));
      if (panelStatus.remaining && panelStatus.remaining.length) el.appendChild(remainingRow(panelStatus));
      el.appendChild(nowRow(panelStatus));
      const waiting = statusRow('waiting');
      waiting.appendChild(labelSpan('Waiting on you'));
      const column = document.createElement('div');
      column.className = 'min-w-0 flex-1 space-y-1';
      if (!panelStatus.waiting.length) {
        const nothing = textSpan('text-slate-400', 'Nothing');
        nothing.dataset.waitingEmpty = 'true';
        column.appendChild(nothing);
      }
      for (const item of panelStatus.waiting) column.appendChild(waitingItem(item, true));
      waiting.appendChild(column);
      el.appendChild(waiting);
    }
    if (panelDescendants.length) el.appendChild(childrenRow());
    if (panelChildren.length) el.appendChild(childrenNowRow());
  }

  // The whole panel: the strip redraws on every status update, and the Status
  // view redraws with it while it is shown.
  function renderPanel() {
    const el = document.getElementById(PANEL_ID);
    if (el) {
      clearChildren(el);
      el.classList.toggle('hidden', !panelSessionId || statusViewShown);
      if (panelSessionId) stripBody(el);
    }
    const view = document.getElementById(VIEW_ID);
    if (!view) return;
    clearChildren(view);
    view.classList.toggle('hidden', !statusViewShown || !panelSessionId);
    if (statusViewShown && panelSessionId) statusViewBody(view);
  }

  // The Status view's show and hide, through the toggle's control. Showing it
  // hides the strip and the floating scroll-to-bottom button, which would
  // otherwise hover over the status text; hiding it puts the button back as it was.
  let scrollButtonWasShown = false;
  function setStatusViewShown(shown) {
    const btn = document.getElementById(SCROLL_BUTTON_ID);
    if (shown && !statusViewShown && btn) {
      scrollButtonWasShown = !btn.classList.contains('hidden');
      btn.classList.add('hidden');
    } else if (!shown && statusViewShown && btn) {
      btn.classList.toggle('hidden', !scrollButtonWasShown);
    }
    statusViewShown = Boolean(shown);
    renderPanel();
  }

  Chat.registerStatusViewControl({setShown: setStatusViewShown});

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
    if (body.web_status && (!panelStatus || Date.parse(body.web_status.updated_at) >= Date.parse(panelStatus.updated_at))) {
      panelStatus = body.web_status;
    }
    panelDescendants = body.descendants;
    panelChildren = body.children;
    renderPanel();
  }

  // Session open (first load and SPA switch): the session's own status, then the
  // history's web_status messages, then the descendant rows. The Status view
  // opens shown when the session's held view choice is Status.
  function openSessionStatusPanel(session, messages) {
    if (!Chat.ForYou.isWebSession(session)) {
      clearSessionStatusPanel();
      return;
    }
    panelSessionId = session.id;
    panelStatus = session.web_status || null;
    panelDescendants = [];
    panelChildren = [];
    setStatusViewShown(Chat.ForYou.viewFor(session) === STATUS_VIEW);
    absorbWebStatusMessages(messages);
    renderPanel();
    fetchDescendants(session.id);
  }

  function clearSessionStatusPanel() {
    panelSessionId = null;
    panelStatus = null;
    panelDescendants = [];
    panelChildren = [];
    setStatusViewShown(false);
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
