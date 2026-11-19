// ---------------------------------------------------------------------------
// Session status panel (the Status view)
// ---------------------------------------------------------------------------
// A web session's status shows in the Status view (#status-panel). That panel
// takes the message list's place in the Chat column while Status is selected in
// the view toggle. The selection and the panel's visibility belong to
// chat/for-you.js's applyChatView. This file renders the panel's content and
// the waiting count on the toggle option's badge. A thread session and a worker
// session show neither the option nor the badge. A web session is one that
// Chat.ForYou.isWebSession (for-you.js) accepts.
//
// The session's own status, list version, and answered marks come from GET
// /api/internal/web/status. The descendant and child rows use the same read,
// fetched at open and after status notifications.
(function() {
  const Chat = globalThis.Chat;

  const VIEW_ID = 'status-panel';
  const BADGE_ID = 'status-waiting-count';
  const DONE_HEADER = '[Status · Waiting on you]';
  const pendingDone = new Map();
  const itemNotices = new Map();
  const headerNotices = new Map();
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
  let panelHeaderNotice = '';

  function doneKey(sessionId, waitingVersion, position) {
    return JSON.stringify([sessionId, waitingVersion, position]);
  }

  function setHeaderNotice(sessionId, notice) {
    if (notice) headerNotices.set(sessionId, notice);
    else headerNotices.delete(sessionId);
    if (sessionId === panelSessionId) panelHeaderNotice = notice;
  }

  function pendingForSession(sessionId) {
    return [...pendingDone.values()].filter((entry) => entry.sessionId === sessionId);
  }

  function cancelPendingDone() {
    for (const [key, entry] of pendingDone) {
      entry.undo.cancel();
      pendingDone.delete(key);
    }
    renderPanel();
  }

  function textSpan(className, text) {
    const span = document.createElement('span');
    span.className = className;
    span.textContent = text;
    return span;
  }

  function clearChildren(el) {
    while (el.firstChild) el.removeChild(el.firstChild);
  }

  // A fold's mark: the disclosure icon for its state, the one every fold in the panel shows.
  function foldGlyph(open) {
    return Chat.disclosureIcon(open, 12);
  }

  function foldGlyphSpan(open) {
    const span = document.createElement('span');
    span.className = 'w-3 text-center';
    span.innerHTML = foldGlyph(open);
    return span;
  }

  // A fold toggle: the button, its glyph span and its label span are the same
  // in every fold. A caller names the style, the state, the label and the click.
  function foldToggle(className, open, label, onToggle) {
    const toggle = document.createElement('button');
    toggle.type = 'button';
    toggle.className = className;
    toggle.setAttribute('aria-expanded', String(open));
    toggle.appendChild(foldGlyphSpan(open));
    toggle.appendChild(textSpan('', label));
    toggle.onclick = onToggle;
    return toggle;
  }

  // A fold's state as its toggle shows it: aria-expanded for assistive tech, the glyph
  // for the eye.
  function setFoldOpen(toggle, open) {
    toggle.setAttribute('aria-expanded', String(open));
    toggle.children[0].innerHTML = foldGlyph(open);
  }

  function safeLink(link) {
    return link && SAFE_LINK.test(link) ? link : null;
  }

  function openLink(href) {
    const a = document.createElement('a');
    a.className = 'shrink-0 whitespace-nowrap text-blue-400 hover:text-blue-300';
    a.href = href;
    a.dataset.openLink = 'true';
    a.innerHTML = 'Open ' + cbIcon('open', 12);
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
  // context shown. Descendant items keep it folded until the operator clicks.
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

  // A phone stacks each label above its value at full width. The desktop keeps a label column.
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

  function undoButton(onClick) {
    const button = document.createElement('button');
    button.type = 'button';
    button.className = 'inline-flex items-center gap-1 text-blue-400 hover:text-blue-300';
    button.innerHTML = cbIcon('undo', 14);
    button.appendChild(textSpan('', 'Undo'));
    button.onclick = onClick;
    return button;
  }

  function doneButton(onClick) {
    const button = document.createElement('button');
    button.type = 'button';
    button.className = 'inline-flex items-center justify-center rounded text-slate-400 hover:text-slate-200';
    button.style.width = '24px';
    button.style.height = '24px';
    button.setAttribute('aria-label', 'Mark done');
    button.title = 'Mark done';
    button.innerHTML = cbIcon('done', 14);
    button.onclick = onClick;
    return button;
  }

  function cancelDone(key) {
    const entry = pendingDone.get(key);
    if (!entry) return;
    entry.undo.cancel();
    pendingDone.delete(key);
    renderPanel();
  }

  function itemControlRow(item, sessionId, position, canMarkDone) {
    const row = document.createElement('div');
    row.className = 'flex items-center gap-2 text-xs';
    row.dataset.itemControls = 'true';
    const waitingVersion = panelStatus && panelStatus.waiting_version;
    const key = position ? doneKey(sessionId, waitingVersion, position) : null;
    const pending = key && pendingDone.get(key);
    const notice = key && itemNotices.get(key);
    if (item.answered === true) {
      row.appendChild(textSpan('text-slate-400', 'Marked done'));
    } else if (pending && !pending.undo.sending) {
      row.appendChild(textSpan('text-slate-400', 'Marked done · '));
      row.appendChild(undoButton(() => cancelDone(key)));
    } else if (pending) {
      row.appendChild(textSpan('text-slate-400', 'Marked done'));
    } else if (notice) {
      row.appendChild(textSpan(notice.startsWith('Status unknown:') ? 'text-amber-200' : 'text-slate-400', notice));
      if (!notice.startsWith('Status unknown:') && canMarkDone && waitingVersion && position) {
        row.appendChild(doneButton(() => scheduleDone(sessionId, waitingVersion, position, item.since)));
      }
    } else if (canMarkDone && waitingVersion && position) {
      row.appendChild(doneButton(() => scheduleDone(sessionId, waitingVersion, position, item.since)));
    }
    return row;
  }

  function scheduleDone(sessionId, waitingVersion, position, since) {
    const key = doneKey(sessionId, waitingVersion, position);
    if (pendingDone.has(key)) return;
    const entry = {sessionId, waitingVersion, position, since, clickMs: Date.now(), undo: null};
    entry.undo = startUndoableSend(() => postDone(entry));
    pendingDone.set(key, entry);
    entry.undo.sent.then(
      (posted) => {
        // A cancelled click has already left pendingDone, in cancelDone or cancelPendingDone.
        if (posted !== null) finishDone(key, entry, null);
      },
      (err) => finishDone(key, entry, err),
    );
    renderPanel();
  }

  async function readStatus(sessionId, includeViewSignal = true) {
    const options = includeViewSignal ? {signal: viewRequestSignal()} : {};
    const response = await fetch('/api/internal/web/status?session_id=' + encodeURIComponent(sessionId), options);
    if (!response.ok) throw new Error('HTTP ' + response.status);
    return response.json();
  }

  async function fetchStatusForAction(sessionId) {
    const isCurrentSession = sessionId === panelSessionId;
    const gen = isCurrentSession ? ++panelFetchGen : null;
    const body = await readStatus(sessionId, isCurrentSession);
    if (isCurrentSession && gen === panelFetchGen && sessionId === panelSessionId) {
      applyStatus(body);
    }
    return body;
  }

  // The Status click's post, the delayed send's one attempt: an answer other than 202 is an error.
  // A failed click reports "Not sent" and the user clicks again; the post itself never retries.
  async function postDone(entry) {
    renderPanel();
    const content = buildClickMessage(DONE_HEADER, [{
      reference: 'Waiting on you · session ' + entry.sessionId + ' · list ' + entry.waitingVersion +
        ' · since ' + entry.since + ' · #' + entry.position,
      answer: 'Done',
    }]);
    const requestId = 'status-done-' + entry.sessionId + '-' + entry.clickMs;
    const response = await fetch('/api/chat/' + encodeURIComponent(entry.sessionId) + '/message', {
      method: 'POST',
      headers: {'Content-Type': 'application/json'},
      credentials: 'same-origin',
      body: JSON.stringify({content, uploaded_files: [], request_id: requestId}),
    });
    if (response.status !== 202) throw new Error('HTTP ' + response.status);
  }

  // The post settled (postError null when it was accepted): read the status back and say what the server holds.
  async function finishDone(key, entry, postError) {
    pendingDone.delete(key);
    try {
      const body = await fetchStatusForAction(entry.sessionId);
      const status = body && body.web_status;
      const item = status && status.waiting && status.waiting[entry.position - 1];
      const answered = status && status.waiting_version === entry.waitingVersion && item && item.answered === true;
      if (postError) {
        if (answered) {
          itemNotices.delete(key);
        } else if (status && status.waiting_version === entry.waitingVersion && item) {
          itemNotices.set(key, 'Not sent: ' + postError.message);
        } else {
          setHeaderNotice(entry.sessionId, 'Not sent: ' + postError.message);
        }
      } else if (!answered) {
        setHeaderNotice(entry.sessionId, 'Sent: the list changed');
      } else {
        itemNotices.delete(key);
        setHeaderNotice(entry.sessionId, '');
      }
    } catch (err) {
      if (postError) itemNotices.set(key, 'Status unknown: ' + err.message);
      else setHeaderNotice(entry.sessionId, 'Status unknown: ' + err.message);
    }
    if (entry.sessionId === panelSessionId) renderPanel();
  }

  // A Waiting on you item. contextOpen: the Status view shows an own item's
  // context open. The descendant rows keep it folded.
  function waitingItem(item, contextOpen, sessionId, position, canMarkDone) {
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
    if (item.answered === true || canMarkDone) {
      el.appendChild(itemControlRow(item, sessionId, position, canMarkDone));
    }
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
    if (item.answered === true) {
      const controls = document.createElement('div');
      controls.className = 'flex items-center gap-2 text-xs';
      controls.dataset.itemControls = 'true';
      controls.appendChild(textSpan('text-slate-400', 'Marked done'));
      el.appendChild(controls);
    }
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
  function waitingRow(storedItems, derivedItems) {
    const waiting = statusRow('waiting');
    const heading = document.createElement('div');
    heading.className = 'flex items-center gap-2';
    heading.appendChild(labelSpan('Waiting on you'));
    const controls = document.createElement('div');
    controls.className = 'flex items-center gap-2 text-xs';
    controls.dataset.waitingHeaderControls = 'true';
    const currentVersion = panelStatus && panelStatus.waiting_version;
    for (const [key, entry] of pendingDone) {
      if (entry.sessionId !== panelSessionId || entry.waitingVersion === currentVersion || entry.undo.sending) continue;
      controls.appendChild(textSpan('text-slate-400', 'Marked done · '));
      controls.appendChild(undoButton(() => cancelDone(key)));
    }
    if (panelHeaderNotice) controls.appendChild(textSpan('text-amber-200', panelHeaderNotice));
    heading.appendChild(controls);
    waiting.appendChild(heading);
    const column = document.createElement('div');
    column.className = 'min-w-0 flex-1 space-y-1';
    if (!storedItems.length && !derivedItems.length) {
      const nothing = textSpan('text-slate-400', 'Nothing');
      nothing.dataset.waitingEmpty = 'true';
      column.appendChild(nothing);
    }
    for (let i = 0; i < storedItems.length; i++) {
      column.appendChild(waitingItem(storedItems[i], true, panelSessionId, i + 1, true));
    }
    for (const item of derivedItems) column.appendChild(waitingItem(item, true, panelSessionId, null, false));
    waiting.appendChild(column);
    return waiting;
  }

  // The row an empty Status view shows. The toggle's one word names no subject, and the pane is
  // otherwise blank at both widths, so the row names what the view reports and what fills it. The
  // Waiting on you row also carries a host wait the server derives, so the note claims no report
  // for that row.
  function noStatusRow() {
    const row = statusRow('none');
    const line = document.createElement('p');
    line.className = 'min-w-0 break-words text-slate-400';
    line.dataset.statusNoneNote = 'true';
    const lead = document.createElement('b');
    lead.textContent = 'No status yet.';
    line.appendChild(lead);
    line.appendChild(textSpan('',
      " The session's Goal and Now appear here once it reports them, and Waiting on you lists the items waiting on you."));
    row.appendChild(line);
    return row;
  }

  function statusPanelBody(el) {
    if (!panelStatus) {
      el.appendChild(noStatusRow());
      const held = derivedFor(panelSessionId);
      if (held.length || pendingForSession(panelSessionId).length || panelHeaderNotice) el.appendChild(waitingRow([], held));
    } else {
      el.appendChild(goalRow(panelStatus.goal));
      el.appendChild(nowRow(panelStatus));
      el.appendChild(waitingRow(panelStatus.waiting, derivedFor(panelSessionId)));
    }
    if (panelDescendants.length) el.appendChild(childrenRow());
    if (panelChildren.length) el.appendChild(childrenNowRow());
  }

  // The panel redraws on every status update, and the badge follows the
  // same state. The panel may be hidden while Status is not selected. Its
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

  // A response for a session the page no longer shows, or one overtaken by a
  // later GET, is dropped. Only this endpoint supplies waiting_version/answered.
  function applyStatus(body) {
    panelStatus = body.web_status;
    panelDescendants = body.descendants;
    panelChildren = body.children;
    panelDerived = body.derived_waiting;
    renderPanel();
  }

  async function fetchStatus(sessionId) {
    const gen = ++panelFetchGen;
    let body;
    try {
      body = await readStatus(sessionId);
    } catch (err) {
      console.error('web status fetch failed:', err);
      return null;
    }
    if (gen !== panelFetchGen || sessionId !== panelSessionId) return {body, applied: false};
    applyStatus(body);
    return {body, applied: true};
  }

  // Session open loads the status tree from the server. Whether the panel shows
  // comes from the session's remembered view choice (chat/for-you.js applyChatView).
  function openSessionStatusPanel(session) {
    if (!Chat.ForYou.isWebSession(session)) {
      clearSessionStatusPanel();
      return;
    }
    panelSessionId = session.id;
    panelStatus = null;
    panelDescendants = [];
    panelChildren = [];
    panelDerived = [];
    panelHeaderNotice = headerNotices.get(session.id) || '';
    renderPanel();
    fetchStatus(session.id);
  }

  function clearSessionStatusPanel() {
    panelSessionId = null;
    panelStatus = null;
    panelDescendants = [];
    panelChildren = [];
    panelDerived = [];
    panelHeaderNotice = '';
    renderPanel();
  }

  // The push announces a change; the endpoint supplies the authoritative list and
  // its derived version and answered marks.
  function observeWebStatusMessages(messages) {
    if (!panelSessionId) return;
    if (messages.some((msg) => msg.role === 'web_status')) fetchStatus(panelSessionId);
  }

  // Either session's status change refreshes all status data for this tree.
  function refreshSessionStatusPanel() {
    if (panelSessionId) fetchStatus(panelSessionId);
  }

  window.addEventListener('pagehide', cancelPendingDone);
  document.addEventListener('charliebot:session-view-teardown', cancelPendingDone);

  // A fold-out or rotation changes the platform layout, so redraw the panel.
  platform.onChange(renderPanel);

  Chat.wire({
    openSessionStatusPanel,
    clearSessionStatusPanel,
    observeWebStatusMessages,
    refreshSessionStatusPanel,
  });

})();
