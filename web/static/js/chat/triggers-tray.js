// ---------------------------------------------------------------------------
// Pending triggers tray (chat column)
// ---------------------------------------------------------------------------
// The chat column's view of the session's pending delayed triggers: one
// collapsible tray between the thinking indicator and the input area. It
// lists the schedule_trigger records that
// GET /api/sessions/{id}/pending-triggers returns (pending only, fire_at
// ascending). Each row's inline two-step cancel posts to
// POST /api/internal/triggers/{sid}/{tid}/cancel. A worker session (no input
// area) shows it too; a session with zero pending triggers shows nothing at
// all.
//
// The sidebar bell and this tray are the same signal — the same amber, the
// same bell glyph. So the status poll is the only refetch signal here. The
// tray remembers the session's last (pending_trigger_count, next_trigger_at)
// as applySessionStatus hands it over, and refetches when either changes. No
// timer of its own; an in-tray cancel refetches immediately.
(function() {
  const Chat = globalThis.Chat;
  // chatOnly member of shared.js: reachable through the namespace, not as a bare global.
  const escapeJsSingleQuoted = Chat.escapeJsSingleQuoted;

  const TRAY_ID = 'pending-triggers-tray';
  const CANCEL_REVERT_MS = 3000;
  // The pending-trigger bell's body: the sidebar's bell (sidebar/status.js)
  // renders the same glyph, so the two surfaces of one fact read as one
  // signal. Wired chatOnly for the sidebar's bell.
  const PENDING_TRIGGER_BELL_SVG_PATH = '<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M15 17h5l-1.405-1.405A2.032 2.032 0 0118 14.158V11a6.002 6.002 0 00-4-5.659V5a2 2 0 10-4 0v.341C7.67 6.165 6 8.388 6 11v3.159c0 .538-.214 1.055-.595 1.436L4 17h5m6 0a3 3 0 01-6 0"/>';
  const CHEVRON_DOWN_PATH = '<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M19 9l-7 7-7-7"/>';
  const CHEVRON_UP_PATH = '<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M5 15l7-7 7 7"/>';
  const X_SVG_PATH = '<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M6 18L18 6M6 6l12 12"/>';

  // One active session's tray state. traySessionId is the session the fetched
  // records belong to; the poll handoff and the bell only act on a session
  // the tray has opened.
  let traySessionId = null;
  let trayTriggers = [];
  let trayExpanded = false;
  let trayExpandOnNextOpen = false;
  let trayKnownStatus = null;  // {count, nextMs} — the last (count, next) the tray rendered
  let trayFetchGen = 0;
  const trayCancelArms = new Map();  // trigger id -> revert timer id

  function trayElement() {
    return document.getElementById(TRAY_ID);
  }

  function bellSvg() {
    return '<svg class="w-4 h-4 text-amber-400 flex-shrink-0" fill="none" stroke="currentColor" viewBox="0 0 24 24">'
      + PENDING_TRIGGER_BELL_SVG_PATH + '</svg>';
  }

  function chevronSvg(up, extraClass) {
    return '<svg class="w-4 h-4 text-slate-500 flex-shrink-0' + (up ? ' ml-auto' : '')
      + (extraClass ? ' ' + extraClass : '')
      + '" fill="none" stroke="currentColor" viewBox="0 0 24 24">' + (up ? CHEVRON_UP_PATH : CHEVRON_DOWN_PATH) + '</svg>';
  }

  // The --watch spellings: a local pid renders with its kind prefix so the
  // bare number cannot read as any other count. Remote and slurm targets keep
  // the schedule-trigger --watch form the master registered them with.
  function watchTargetLabel(t) {
    if (t.kind === 'slurm_job') return (t.host ? t.host + ':' : '') + 'slurm:' + t.job_id;
    if (t.kind === 'remote_pid') return t.host + ':' + t.pid;
    return 'pid ' + t.pid;
  }

  function watchTargetsLabel(targets) {
    return (targets || []).map(watchTargetLabel).join(', ');
  }

  // "(in 3h 52m)" — the remaining time until fire_at, in the viewer's local
  // frame. updateRelativeTimes keeps every .tray-reltime span current through
  // the page's existing relative-time sweep, so the text here only has to be
  // right at render time.
  function relTimeSpan(iso) {
    return '<span class="tray-reltime" data-time="' + escapeHtmlAttr(iso) + '">'
      + escapeHtml(relativeFireIn(iso)) + '</span>';
  }

  function pendingTriggerCountLabel(n) {
    return n + ' pending trigger' + (n === 1 ? '' : 's');
  }

  // "fires at MM/DD HH:mm (in ...)" for a pure delay; "fires when <targets>
  // exit(s) · at the latest MM/DD HH:mm (in ...)" for a watched trigger.
  function trayConditionHtml(tr) {
    const when = dateClockMDHM(new Date(tr.fire_at)) + ' ' + relTimeSpan(tr.fire_at);
    const targets = tr.watch_targets || [];
    if (!targets.length) return 'fires at ' + when;
    const verb = targets.length === 1 ? 'exits' : 'exit';
    return 'fires when <span class="tray-mono text-amber-300">' + escapeHtml(watchTargetsLabel(targets))
      + '</span> ' + verb + ' · at the latest ' + when;
  }

  function trayCancelButtonHtml(tr, armed) {
    const call = 'trayCancelClick(event, \'' + escapeJsSingleQuoted(tr.session_id) + '\', \''
      + escapeJsSingleQuoted(tr.id) + '\')';
    if (armed) {
      return '<button class="px-2 py-0.5 rounded-md text-xs font-medium text-red-400 border border-red-500 flex-shrink-0"'
        + ' title="Click again to cancel" onclick="' + call + '">Cancel?</button>';
    }
    return '<button class="p-1 rounded hover:bg-slate-700 text-slate-500 hover:text-red-400 flex-shrink-0"'
      + ' title="Cancel trigger" onclick="' + call + '">'
      + '<svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24">' + X_SVG_PATH + '</svg></button>';
  }

  function trayRowHtml(tr, last) {
    const cancelError = tr.cancelError
      ? '<p class="text-xs text-red-400 mt-1">' + escapeHtml(tr.cancelError) + '</p>' : '';
    return '<div class="flex items-start gap-3 px-3 py-2.5' + (last ? '' : ' border-b border-slate-700/60')
      + '" data-trigger-row="' + escapeHtmlAttr(tr.id) + '">'
      + '<div class="flex-1 min-w-0">'
      + '<p class="text-sm text-slate-200 break-words">' + escapeHtml(tr.message) + '</p>'
      + '<p class="text-xs text-slate-400 mt-0.5">' + trayConditionHtml(tr) + '</p>'
      + cancelError
      + '</div>'
      + trayCancelButtonHtml(tr, trayCancelArms.has(tr.id))
      + '</div>';
  }

  // The collapsed row container is a flex row (direction row) that wraps below
  // sm: and never wraps from sm: up. Below sm: the message takes the full flex
  // basis and the last order. So it always starts a second line of its own,
  // whatever other nodes join the first line (bell, label, time, +N, chevron).
  // The watch target, the relative time and both separators are sm:-only:
  // line 1 has no room for them, and line 2 belongs to the message.
  function trayCollapsedHtml() {
    const next = trayTriggers[0];
    const targets = next.watch_targets || [];
    let html = '<div class="tray-border rounded-xl bg-slate-800 flex flex-wrap sm:flex-nowrap'
      + ' items-center gap-x-2 gap-y-1 px-3 py-2 text-sm cursor-pointer"'
      + ' role="button" title="Show all pending triggers" onclick="togglePendingTriggersTray(true)">'
      + bellSvg()
      + '<span class="text-amber-300 font-medium whitespace-nowrap">Next trigger</span>'
      + '<span class="text-slate-200 whitespace-nowrap">' + escapeHtml(clockTimeHM(new Date(next.fire_at))) + '</span>'
      + '<span class="hidden sm:inline text-slate-500 whitespace-nowrap">' + relTimeSpan(next.fire_at) + '</span>';
    if (targets.length) {
      html += '<span class="shrink-0 text-slate-600 hidden sm:inline">·</span>'
        + '<span class="tray-mono text-amber-300 whitespace-nowrap min-w-0 truncate hidden sm:inline" title="'
        + escapeHtmlAttr(watchTargetsLabel(targets)) + '">' + escapeHtml(watchTargetsLabel(targets)) + '</span>';
    }
    html += '<span class="shrink-0 text-slate-600 hidden sm:inline">·</span>'
      + '<span class="truncate text-slate-400 min-w-[3rem] basis-full order-last sm:order-none sm:flex-1" title="'
      + escapeHtmlAttr(next.message) + '">' + escapeHtml(next.message) + '</span>';
    if (trayTriggers.length > 1) {
      // A phone row has no room for " more": the count alone shows below sm,
      // and the title keeps the full text.
      html += '<span class="text-xs text-slate-500 whitespace-nowrap" title="'
        + '+' + (trayTriggers.length - 1) + ' more">+' + (trayTriggers.length - 1)
        + '<span class="hidden sm:inline"> more</span></span>';
    }
    return html + chevronSvg(false, 'ml-auto sm:ml-0') + '</div>';
  }

  function trayExpandedHtml() {
    const next = trayTriggers[0];
    const rows = trayTriggers.map((tr, i) => trayRowHtml(tr, i === trayTriggers.length - 1)).join('');
    return '<div class="tray-border rounded-xl bg-slate-800 overflow-hidden">'
      + '<div class="flex items-center gap-2 px-3 py-2 text-sm border-b border-slate-700 cursor-pointer"'
      + ' role="button" title="Hide pending triggers" onclick="togglePendingTriggersTray(false)">'
      + bellSvg()
      + '<span class="text-amber-300 font-medium whitespace-nowrap">' + escapeHtml(pendingTriggerCountLabel(trayTriggers.length)) + '</span>'
      + '<span class="text-slate-500 whitespace-nowrap">· next '
      + '<span class="text-slate-200">' + escapeHtml(clockTimeHM(new Date(next.fire_at))) + '</span> '
      + relTimeSpan(next.fire_at) + '</span>'
      + chevronSvg(true)
      + '</div>'
      + '<div class="max-h-48 overflow-y-auto">' + rows + '</div>'
      + '</div>';
  }

  function renderTray() {
    const tray = trayElement();
    if (!tray) return;
    if (!trayTriggers.length) {
      tray.classList.add('hidden');
      tray.innerHTML = '';
      return;
    }
    tray.classList.remove('hidden');
    tray.innerHTML = trayExpanded ? trayExpandedHtml() : trayCollapsedHtml();
  }

  function disarmAllCancels() {
    trayCancelArms.forEach((timer) => clearTimeout(timer));
    trayCancelArms.clear();
  }

  // The (count, next) pair the fetched list itself proves: the open fetch
  // seeds the remembered pair. So the first status poll after a session open
  // refetches only when the state actually moved.
  function statusFromTriggers(triggers) {
    return {
      count: triggers.length,
      nextMs: triggers.length ? Date.parse(triggers[0].fire_at) : null,
    };
  }

  async function fetchAndRenderTray(sessionId) {
    const gen = ++trayFetchGen;
    try {
      const res = await fetch('/api/sessions/' + sessionId + '/pending-triggers');
      if (!res.ok) throw new Error('HTTP ' + res.status);
      const records = await res.json();
      if (gen !== trayFetchGen || sessionId !== SESSION_ID) return;
      trayTriggers = records;
      disarmAllCancels();
      trayKnownStatus = statusFromTriggers(records);
    } catch (err) {
      console.error('pending-triggers fetch failed:', err);
      return;
    }
    renderTray();
  }

  // Session open (first page load and SPA switch ride renderSessionView). A
  // fresh session resets the tray to its collapsed default unless the bell's
  // open-and-expand intent is pending.
  function renderPendingTriggersTray(sessionId) {
    const tray = trayElement();
    if (!tray) return;
    if (!sessionId) {
      traySessionId = null;
      trayTriggers = [];
      trayExpanded = false;
      trayExpandOnNextOpen = false;
      trayKnownStatus = null;
      disarmAllCancels();
      renderTray();
      return;
    }
    if (traySessionId !== sessionId) {
      traySessionId = sessionId;
      trayTriggers = [];
      trayExpanded = trayExpandOnNextOpen;
      trayExpandOnNextOpen = false;
      trayKnownStatus = null;
      disarmAllCancels();
    }
    fetchAndRenderTray(sessionId);
  }

  // The status poll's handoff (applySessionStatus): refetch only when the
  // session's (pending_trigger_count, next_trigger_at) pair moved against what
  // the tray last rendered.
  function setSessionPendingTriggerTrayStatus(sid, status) {
    if (sid !== SESSION_ID || traySessionId !== sid) return;
    const count = Number(status && status.pending_trigger_count) || 0;
    const nextMs = status && status.next_trigger_at ? Date.parse(status.next_trigger_at) : null;
    const known = trayKnownStatus;
    if (known && known.count === count && known.nextMs === nextMs) return;
    trayKnownStatus = {count, nextMs};
    fetchAndRenderTray(sid);
  }

  function togglePendingTriggersTray(expand) {
    trayExpanded = !!expand;
    renderTray();
  }

  // The sidebar bell: on the open session the tray expands in place, and the
  // answer is true. For another session the answer is false: the caller
  // switches there, and the tray opens expanded once the switch lands.
  function expandPendingTriggersTray(sid) {
    if (sid === SESSION_ID && traySessionId === sid) {
      trayExpanded = true;
      renderTray();
      return true;
    }
    trayExpandOnNextOpen = true;
    return false;
  }

  // The row's two-step cancel: the first click arms ("Cancel?") for
  // CANCEL_REVERT_MS, a second click inside the window calls the cancel
  // endpoint, a revert or a success refetch disarms. A non-2xx answer or a
  // network error keeps the row and shows what failed inside it.
  function trayCancelClick(event, sessionId, triggerId) {
    event.preventDefault();
    event.stopPropagation();
    const armed = trayCancelArms.get(triggerId);
    if (armed === undefined) {
      trayCancelArms.set(triggerId, setTimeout(() => {
        trayCancelArms.delete(triggerId);
        renderTray();
      }, CANCEL_REVERT_MS));
      renderTray();
      return;
    }
    clearTimeout(armed);
    trayCancelArms.delete(triggerId);
    cancelTrayTrigger(sessionId, triggerId);
  }

  async function cancelTrayTrigger(sessionId, triggerId) {
    const tr = trayTriggers.find((t) => t.id === triggerId);
    let failureText = null;
    try {
      const res = await fetch('/api/internal/triggers/' + encodeURIComponent(sessionId) + '/'
        + encodeURIComponent(triggerId) + '/cancel', {method: 'POST'});
      if (!res.ok) {
        let detail = null;
        if ((res.headers.get('content-type') || '').includes('application/json')) {
          const body = await res.json();
          detail = body && body.detail ? String(body.detail) : null;
        }
        failureText = 'Cancel failed: HTTP ' + res.status + (detail ? ' · ' + detail : '');
        throw new Error(failureText);
      }
    } catch (err) {
      console.error('trigger cancel failed:', err);
      if (tr) tr.cancelError = failureText || 'Cancel failed: network error · ' + err.message;
      renderTray();
      return;
    }
    if (sessionId !== SESSION_ID) return;
    fetchAndRenderTray(sessionId);
  }

  Chat.wire({
    renderPendingTriggersTray,
    setSessionPendingTriggerTrayStatus,
    togglePendingTriggersTray,
    expandPendingTriggersTray,
    trayCancelClick,
  }, {
    PENDING_TRIGGER_BELL_SVG_PATH,
  });

})();
