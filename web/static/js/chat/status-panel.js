// ---------------------------------------------------------------------------
// Session status panel (chat column)
// ---------------------------------------------------------------------------
// A web session's status shows in the Status view (#chat-status-view), which
// covers the whole message region while the Status view is selected: no border,
// radius or margin, the same box as the message list, which stays laid out
// underneath so the turn engine keeps reading its real height. The waiting
// count shows on the Status tab's badge. Chat.ForYou owns the toggle that
// selects the view (for-you.js); this file owns the view, the badge and the
// new-message count row. A thread session and a worker session show none of
// them. A web session is one that Chat.ForYou.isWebSession (for-you.js) accepts.
//
// The status is the session's web_status at open, then the web_status messages of
// this session, live or in history: the newest updated_at wins, so an older page
// of history never moves the view back. The descendant and child rows come from
// GET /api/internal/web/status, fetched at open and on each web_status_subtree
// notice (websocket-handlers.js).
//
// Opening the Status view saves the reader's place in the underlying list
// (turn-engine.js readingPlace) and closes the list to clicks and focus (the
// inert attribute); closing writes the place back for a reader who was in
// history and shows the new-message count row when messages arrived.
(function() {
  const Chat = globalThis.Chat;

  const VIEW_ID = 'chat-status-view';
  const SCROLL_BUTTON_ID = 'scroll-to-bottom';
  const MESSAGES_ID = 'messages';
  const BADGE_ID = 'status-waiting-count';
  const NEW_MESSAGES_ID = 'status-new-messages';
  // An item's link is text the agent wrote. Only web and host-absolute URLs become
  // anchors: any other scheme, javascript: above all, would run in this page.
  const SAFE_LINK = /^(https?:\/\/|\/)/;
  // The view name for-yous's toggle carries for the full status.
  const STATUS_VIEW = 'status';

  // The session the page shows the status for, and what it draws from. A null
  // panelSessionId means the page shows none of the surfaces. statusViewShown
  // holds the Status view's visibility.
  let panelSessionId = null;
  let panelStatus = null;
  let panelDescendants = [];
  let panelChildren = [];
  let statusViewShown = false;
  let panelFetchGen = 0;
  // The reader's place in the underlying list, saved when the Status view
  // opens with the session it was saved for, written back when it closes.
  let savedReadingPlace = null;
  // Messages that arrived while the Status view showed and that the returning
  // view will show. Zeroed at each open.
  let statusViewNewCount = 0;

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

  // The Status view redraws on every status update, and the badge follows the
  // same state.
  function renderPanel() {
    const view = document.getElementById(VIEW_ID);
    if (view) {
      clearChildren(view);
      view.classList.toggle('hidden', !statusViewShown || !panelSessionId);
      if (statusViewShown && panelSessionId) statusViewBody(view);
    }
    renderWaitingBadge();
  }

  // The Status tab's waiting count: this session's own Waiting on you items
  // only — a child session's waiting items show in the Status view's child
  // rows. Hidden at 0 and when the page shows no web session.
  function renderWaitingBadge() {
    const badge = document.getElementById(BADGE_ID);
    if (!badge) return;
    const count = panelSessionId && panelStatus ? panelStatus.waiting.length : 0;
    badge.textContent = count ? String(count) : '';
    badge.classList.toggle('hidden', !count);
  }

  function engineForMessages() {
    return Chat.activeTurnEngineFor(document.getElementById(MESSAGES_ID));
  }

  // The covered list takes no clicks and no focus while the Status view shows.
  function setListInert(inert) {
    const list = document.getElementById(MESSAGES_ID);
    if (!list) return;
    if (inert) list.setAttribute('inert', '');
    else list.removeAttribute('inert');
  }

  function showNewMessagesRow(show) {
    const row = document.getElementById(NEW_MESSAGES_ID);
    if (!row) return;
    row.textContent = show
      ? statusViewNewCount + ' new message' + (statusViewNewCount === 1 ? '' : 's')
      : '';
    row.classList.toggle('hidden', !show);
  }

  // The count row's jump: back to the newest message, the row goes away.
  function revealStatusNewMessages() {
    dropNewMessagesCount();
    if (typeof scrollToBottom === 'function') scrollToBottom();
  }

  // The count belongs to the session it counted: a Status open or a session
  // switch zeroes it and takes the row down, whether a close left the row showing.
  function dropNewMessagesCount() {
    statusViewNewCount = 0;
    showNewMessagesRow(false);
  }

  // The Status view's show and hide, through the toggle's control. Opening it
  // hides the floating scroll-to-bottom button, which would otherwise hover
  // over the status text, saves the reader's place in the underlying list and
  // zeroes the new-message count; closing it puts the button back as it was,
  // writes the place back and offers the count row's jump.
  let scrollButtonWasShown = false;
  function setStatusViewShown(shown) {
    shown = Boolean(shown);
    const btn = document.getElementById(SCROLL_BUTTON_ID);
    if (shown && !statusViewShown) {
      if (btn) {
        scrollButtonWasShown = !btn.classList.contains('hidden');
        btn.classList.add('hidden');
      }
      const engine = engineForMessages();
      savedReadingPlace = {session: panelSessionId, place: engine ? engine.readingPlace() : null};
      dropNewMessagesCount();
      setListInert(true);
    } else if (!shown && statusViewShown) {
      if (btn) btn.classList.toggle('hidden', !scrollButtonWasShown);
      setListInert(false);
      restoreReadingPlaceOnClose();
    }
    statusViewShown = shown;
    renderPanel();
  }

  // The close-time write-back. A pinned reader needs nothing: the engine kept
  // following the new messages, so the return lands on the newest one. A
  // history reader returns to the place saved at open — not the engine's live
  // anchor, which a mid-Status clamp may have moved. Only the same session's
  // place restores: a session switch disposes the reader's place with its
  // engine.
  function restoreReadingPlaceOnClose() {
    const saved = savedReadingPlace;
    savedReadingPlace = null;
    let restored = false;
    if (saved && saved.session === panelSessionId && saved.place && !saved.place.pinned) {
      const engine = engineForMessages();
      if (engine) engine.restoreReadingPlace(saved.place);
      restored = true;
    }
    showNewMessagesRow(restored && statusViewNewCount > 0);
  }

  // A committed message of this session while the Status view shows: it counts
  // for the return view's jump row when that view will show it — For you
  // applies its row filter (for-you.js), Everything counts every message.
  function noteMessageForStatusView(msg) {
    if (!statusViewShown || !panelSessionId) return;
    const engine = engineForMessages();
    if (engine && engine.view === 'foryou' && !Chat.ForYou.items([{msg}]).length) return;
    statusViewNewCount++;
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
    dropNewMessagesCount();
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
    dropNewMessagesCount();
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

  // The count row is a page fixture; its click wires once here.
  const newMessagesRow = document.getElementById(NEW_MESSAGES_ID);
  if (newMessagesRow) newMessagesRow.onclick = revealStatusNewMessages;

  Chat.wire({
    openSessionStatusPanel,
    clearSessionStatusPanel,
    observeWebStatusMessages,
    refreshSessionStatusPanel,
    noteMessageForStatusView,
  });

})();
