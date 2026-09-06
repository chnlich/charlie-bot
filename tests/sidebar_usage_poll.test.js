const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const { readStatic } = require('./read_static');
const { createElement } = require('./dom_element_stub');
const { escapeHtml } = require('./escape_html_stub');
const { baseSessionContext, buildSidebarFilterElements, buildUsageElements, createChatSidebarContext } = require('./session_context_stub');

const WEBSOCKET_JS = readStatic('websocket.js');

function buildContext(overrides = {}) {
  const fetchCalls = [];
  const fetchRequests = [];
  const intervals = [];
  const timeouts = [];
  const clears = [];
  const alerts = [];
  const {context, elements, localStorageData} = baseSessionContext(overrides);

  // No stored key: mirrors page-load order config.js → websocket.js on the no-key path.
  context.wsUrlWithToken = (path) => path;
  context.fetch = async (url, opts = {}) => {
    fetchCalls.push(url);
    fetchRequests.push({url, opts});
    if (url.endsWith('/usage')) {
      return {
        ok: true,
        async json() {
          return {
            session: {id: 'session-a', thinking_since: '2026-03-31T20:42:52Z'},
            usage: {
              context_tokens: 49179,
              context_full: 258400,
              context_compact_at: 180000,
              total_cost_usd: 1.25,
            },
            active_backend: context.ACTIVE_BACKEND_ID,
          };
        },
      };
    }
    return {
      ok: true,
      async json() {
        return {id: 'session-b', backend: context.ACTIVE_BACKEND_ID};
      },
    };
  };
  context.setInterval = (fn, ms) => {
    intervals.push({fn, ms});
    return intervals.length;
  };
  context.setTimeout = (fn, ms) => {
    timeouts.push({fn, ms});
    return timeouts.length;
  };
  context.clearInterval = (id) => {
    clears.push(id);
  };
  context.clearTimeout = (id) => {
    clears.push(id);
  };
  context.document.getElementById = (id) => elements.get(id) || null;
  context.document.querySelectorAll = overrides.querySelectorAll || (() => []);
  context.document.querySelector = overrides.querySelector || (() => null);
  context.renderSessionView = () => {};
  context.renderUserMessageBubble = (content, isVoice, timestamp, uploadedFiles) =>
    `<div data-content="${content || ''}" data-voice="${isVoice ? '1' : '0'}" data-ts="${timestamp || ''}" data-files="${(uploadedFiles || []).length}"></div>`;
  context.alert = (message) => {
    alerts.push(message);
  };
  context.confirm = overrides.confirm || (() => true);

  createChatSidebarContext(context);
  return {context, fetchCalls, fetchRequests, intervals, timeouts, clears, alerts, elements, localStorageData};
}

function buildSessionActionElements() {
  return new Map([
    ['session-action-modal-overlay', createElement({className: 'hidden'})],
    ['session-action-modal-title', createElement()],
    ['session-action-modal-body', createElement()],
    ['session-action-backend', createElement({tagName: 'SELECT'})],
    ['session-action-modal-confirm', createElement()],
  ]);
}

function makeSession(id, name, overrides = {}) {
  return {
    id,
    name,
    group: null,
    updated_at: '2026-04-02T04:00:00Z',
    has_unread: false,
    has_running_tasks: false,
    has_pending_trigger: false,
    pending_trigger_count: 0,
    next_trigger_at: null,
    starred: false,
    backend: 'claude-opus-4.6',
    ...overrides,
  };
}

function sessionAnchorOpenTag(html, id) {
  const match = html.match(new RegExp(`<a\\b[^>]*id="session-${id}"[^>]*>`));
  if (!match) throw new Error(`Missing rendered session anchor for ${id}`);
  return match[0];
}

test('pollActiveSessionView refreshes usage from the lazy usage endpoint', async () => {
  const {context, fetchCalls} = buildContext();
  let renderedUsage = null;

  context.masterThinking = true;
  context.renderUsageFromData = (usage) => {
    renderedUsage = usage;
  };
  context.ensureActiveSessionViewPolling = () => {};

  await context.pollActiveSessionView();

  assert.deepEqual(fetchCalls, ['/api/sessions/session-a/usage']);
  assert.deepEqual(renderedUsage, {
    context_tokens: 49179,
    context_full: 258400,
    context_compact_at: 180000,
    total_cost_usd: 1.25,
  });
  assert.equal(context.THINKING_SINCE, '2026-03-31T20:42:52Z');
});

// ---------------------------------------------------------------------------
// renderUsageFromData: compaction line + colour-relative-to-line
// ---------------------------------------------------------------------------

test('renderUsageFromData draws the compact line at the right percentage', () => {
  const elements = buildUsageElements();
  const {context} = buildContext({elements});

  context.renderUsageFromData({
    context_tokens: 100000,
    context_full: 258400,
    context_compact_at: 180000,
    total_cost_usd: 1.25,
    model: 'codex-test',
  });

  const line = elements.get('usage-compact-line');
  assert.doesNotMatch(line.className, /hidden/);
  assert.equal(line.style.left, ((180000 / 258400) * 100).toFixed(1) + '%');
  assert.equal(line.title, '180000');
});

test('renderUsageFromData draws no compact line when context_compact_at is null', () => {
  const elements = buildUsageElements();
  const {context} = buildContext({elements});

  context.renderUsageFromData({
    context_tokens: 100000,
    context_full: 258400,
    context_compact_at: null,
    total_cost_usd: 1.25,
    model: 'codex-test',
  });

  const line = elements.get('usage-compact-line');
  assert.match(line.className, /hidden/);
  assert.equal(line.style.left, '0%');
  // The bar is still drawn (no line, but context_full present).
  const bar = elements.get('usage-bar');
  assert.doesNotMatch(bar.className, /hidden/);
});

test('renderUsageFromData bar turns red past the compaction line', () => {
  const elements = buildUsageElements();
  const {context} = buildContext({elements});

  context.renderUsageFromData({
    context_tokens: 200000,  // past the line at 180000
    context_full: 258400,
    context_compact_at: 180000,
    total_cost_usd: 1.25,
    model: 'codex-test',
  });

  const bar = elements.get('usage-bar');
  assert.match(bar.className, /bg-red-500/);
  assert.doesNotMatch(bar.className, /bg-blue-500/);
  assert.doesNotMatch(bar.className, /bg-yellow-500/);
});

test('renderUsageFromData bar is yellow at 50%-100% of the compaction line', () => {
  const elements = buildUsageElements();
  const {context} = buildContext({elements});

  context.renderUsageFromData({
    context_tokens: 100000,  // 100000 / 180000 ~ 55% of the line
    context_full: 258400,
    context_compact_at: 180000,
    total_cost_usd: 1.25,
    model: 'codex-test',
  });

  assert.match(elements.get('usage-bar').className, /bg-yellow-500/);
});

test('a result WebSocket event forces a poll without writing the header directly', async () => {
  const elements = new Map([
    ['usage-indicator', createElement({className: 'hidden'})],
    ['usage-bar', createElement({className: 'h-full rounded-full bg-blue-500', style: {width: '0%'}})],
    ['usage-text', createElement({textContent: 'before'})],
    ['usage-cost', createElement({textContent: 'before'})],
  ]);
  const {context} = buildContext({elements});
  vm.runInContext(WEBSOCKET_JS, context, {filename: 'websocket.js'});

  let pollCall = null;
  context.pollActiveSessionView = (opts) => { pollCall = opts; };
  let renderCall = null;
  context.renderUsageFromData = (usage) => { renderCall = usage; };

  context.handleWSEvent({type: 'result', total_cost_usd: 5.0}, 'session-a', 0);

  // The WebSocket handler must not write the header; renderUsageFromData is the
  // only writer and it was not called by the result event.
  assert.equal(elements.get('usage-text').textContent, 'before');
  assert.equal(elements.get('usage-cost').textContent, 'before');
  assert.equal(elements.get('usage-bar').style.width, '0%');
  assert.equal(renderCall, null);
  // The forced poll is what updates the header.
  assert.ok(pollCall && pollCall.force === true, 'result event must force the poll');
});

test('ensureActiveSessionViewPolling only schedules while the active session is running', () => {
  const {context, intervals, clears} = buildContext();

  context.ensureActiveSessionViewPolling();
  assert.equal(intervals.length, 0);

  context.THINKING_SINCE = '2026-03-31T20:42:52Z';
  context.ensureActiveSessionViewPolling();
  assert.equal(intervals.length, 1);
  assert.equal(intervals[0].ms, 3000);

  context.ensureActiveSessionViewPolling();
  assert.equal(intervals.length, 1);

  context.THINKING_SINCE = null;
  context.stopActiveSessionViewPolling();
  assert.deepEqual(clears, [1]);
});

test('renderMessage preserves clone_start banners for SPA rebuilds', () => {
  const {context} = buildContext();
  context.escapeHtml = escapeHtml;

  const html = context.renderMessage({
    role: 'clone_start',
    content: 'Parent & Session',
    parent_session_id: 'parent/session?tab=chat',
  }, 'session-a');

  assert.match(html, /Cloned from/);
  assert.match(html, /href="\/\?session=parent%2Fsession%3Ftab%3Dchat"/);
  assert.match(html, /Parent &amp; Session/);
});

test('renderMessage passes uploaded_files through for user attachment bubbles', () => {
  const {context} = buildContext();

  const html = context.renderMessage({
    role: 'user',
    content: '',
    uploaded_files: [{filename: 'report.pdf', path: '/tmp/report.pdf'}],
  }, 'session-a');

  assert.match(html, /message-attachment/);
  assert.match(html, /report\.pdf/);
});

// ---------------------------------------------------------------------------
// Status polls are scoped to the sessions the sidebar has in the DOM
// ---------------------------------------------------------------------------

function sessionAnchors(ids) {
  return ids.map((id) => ({id: 'session-' + id, tagName: 'A'}));
}

test('sidebarSessionIds sends the rendered session anchors plus the active session, deduplicated', () => {
  const {context} = buildContext({
    querySelectorAll: (selector) =>
      selector === 'a[id^="session-"]' ? sessionAnchors(['session-b', 'session-a', 'session-c']) : [],
  });

  assert.deepEqual(Array.from(context.sidebarSessionIds()), ['session-a', 'session-b', 'session-c']);
});

test('sidebarSessionIds includes the active session even when the sidebar has not rendered it', () => {
  const {context} = buildContext();

  assert.deepEqual(Array.from(context.sidebarSessionIds()), ['session-a']);
});

test('pollSessionStatus asks only for the sessions the sidebar renders', async () => {
  const requested = [];
  const {context} = buildContext({
    querySelectorAll: (selector) =>
      selector === 'a[id^="session-"]' ? sessionAnchors(['session-a', 'session-b']) : [],
  });
  context.fetch = async (url) => {
    requested.push(url);
    return {ok: true, json: async () => ({})};
  };

  await context.pollSessionStatus();

  assert.deepEqual(requested, ['/api/sessions/status?ids=session-a,session-b']);
});

test('a session id list too long for one request is split and the results are merged', async () => {
  const ids = [];
  for (let i = 0; i < 400; i++) ids.push('session-' + String(i).padStart(4, '0') + '-'.repeat(20));
  const requested = [];
  const {context} = buildContext({
    querySelectorAll: (selector) => (selector === 'a[id^="session-"]' ? sessionAnchors(ids) : []),
  });
  const seen = [];
  context.fetch = async (url) => {
    requested.push(url);
    const batch = url.slice(url.indexOf('?ids=') + 5).split(',').map(decodeURIComponent);
    return {
      ok: true,
      json: async () => Object.fromEntries(batch.map((sid) => [sid, {has_unread: false, has_running_tasks: false}])),
    };
  };
  context.setSessionIndicator = (sid) => { seen.push(sid); };

  await context.pollSessionStatus();

  assert.ok(requested.length > 1, 'the id list was split across requests');
  requested.forEach((url) => assert.ok(url.length <= 8192, 'no request URL exceeds the 8 KB budget'));
  // session-a is the active session and is always included on top of the rendered ids.
  assert.equal(seen.length, ids.length + 1);
  assert.equal(new Set(seen).size, seen.length, 'no id is requested twice');
});

test('pollSessionStatus updates pending trigger indicators from the status endpoint', async () => {
  const {context} = buildContext();
  const indicatorUpdates = [];
  const pendingUpdates = [];

  context.fetch = async (url) => {
    assert.equal(url, '/api/sessions/status?ids=session-a');
    return {
      ok: true,
      async json() {
        return {
          'session-a': {
            has_unread: false,
            has_running_tasks: false,
            has_pending_trigger: true,
            pending_trigger_count: 2,
            next_trigger_at: '2026-04-02T06:00:00Z',
          },
          'session-b': {
            has_unread: true,
            has_running_tasks: true,
            has_pending_trigger: false,
            pending_trigger_count: 0,
            next_trigger_at: null,
          },
        };
      },
    };
  };
  context.setSessionIndicator = (sid, state) => {
    indicatorUpdates.push({sid, state});
  };
  context.setSessionPendingTriggerIndicator = (sid, status) => {
    pendingUpdates.push({sid, status});
  };

  const anyRunning = await context.pollSessionStatus();

  assert.equal(anyRunning, true);
  assert.deepEqual(indicatorUpdates, [
    {sid: 'session-a', state: 'idle'},
    {sid: 'session-b', state: 'worker_only'},
  ]);
  assert.deepEqual(pendingUpdates, [
    {
      sid: 'session-a',
      status: {
        has_unread: false,
        has_running_tasks: false,
        has_pending_trigger: true,
        pending_trigger_count: 2,
        next_trigger_at: '2026-04-02T06:00:00Z',
      },
    },
    {
      sid: 'session-b',
      status: {
        has_unread: true,
        has_running_tasks: true,
        has_pending_trigger: false,
        pending_trigger_count: 0,
        next_trigger_at: null,
      },
    },
  ]);
});

test('restoreSidebarFromUrl renders initial all sessions through grouped renderer without fetching sessions', () => {
  const nav = createElement();
  const filterAll = createElement({className: 'filter-pill'});
  const filterStarred = createElement({className: 'filter-pill'});
  const filterArchived = createElement({className: 'filter-pill'});
  const filterScheduled = createElement({className: 'filter-pill'});
  const {context, fetchRequests} = buildContext({
    elements: new Map([
      ['session-list', nav],
      ['filter-all', filterAll],
      ['filter-starred', filterStarred],
      ['filter-archived', filterArchived],
      ['filter-scheduled', filterScheduled],
      ['cron-add-btn', createElement()],
    ]),
    querySelectorAll: (selector) => selector === '.filter-pill'
      ? [filterAll, filterStarred, filterArchived, filterScheduled] : [],
  });

  context.INITIAL_SESSIONS = [
    {
      id: 'session-a',
      name: 'Grouped session',
      group: 'Work',
      updated_at: '2026-04-02T04:00:00Z',
      has_unread: true,
      has_running_tasks: false,
      has_pending_trigger: true,
      pending_trigger_count: 1,
      next_trigger_at: '2026-04-02T06:00:00Z',
      starred: true,
      backend: 'claude-opus-4.6',
    },
    {
      id: 'session-b',
      name: 'Ungrouped session',
      group: null,
      updated_at: '2026-04-01T04:00:00Z',
      has_unread: false,
      has_running_tasks: false,
      has_pending_trigger: false,
      pending_trigger_count: 0,
      next_trigger_at: null,
      starred: false,
      backend: 'claude-opus-4.6',
    },
  ];
  context.INITIAL_LOAD_ERRORS = [];
  context.location.search = '';

  context.restoreSidebarFromUrl();

  assert.equal(fetchRequests.length, 0);
  assert.match(nav.innerHTML, /class="session-group group"/);
  assert.match(nav.innerHTML, /Work/);
  assert.match(nav.innerHTML, /\(No group\)/);
  assert.match(nav.innerHTML, /id="pending-trigger-session-a"/);
  assert.match(nav.innerHTML, /id="star-session-a"/);
  assert.equal(filterAll.classList.contains('bg-blue-600/20'), true);
});

test('renderSessionList limits each grouped session section to five visible sessions', () => {
  const nav = createElement();
  const {context} = buildContext({
    elements: new Map([['session-list', nav]]),
  });
  const workSessions = Array.from({length: 7}, (_, idx) =>
    makeSession(`work-${idx + 1}`, `Work ${idx + 1}`, {group: 'Work'}));
  const personalSessions = Array.from({length: 6}, (_, idx) =>
    makeSession(`personal-${idx + 1}`, `Personal ${idx + 1}`, {group: 'Personal'}));

  context.renderSessionList([...workSessions, ...personalSessions], 'all');

  assert.match(nav.innerHTML, /Work/);
  assert.match(nav.innerHTML, /Personal/);
  assert.equal((nav.innerHTML.match(/session-group-limit-toggle/g) || []).length, 2);
  assert.match(nav.innerHTML, />Show all<\/button>/);
  assert.equal(sessionAnchorOpenTag(nav.innerHTML, 'work-5').includes('session-group-limit-extra'), false);
  assert.equal(sessionAnchorOpenTag(nav.innerHTML, 'work-6').includes('session-group-limit-extra hidden'), true);
  assert.equal(sessionAnchorOpenTag(nav.innerHTML, 'personal-6').includes('session-group-limit-extra hidden'), true);
  assert.match(nav.innerHTML, /renameGroup\(this\.dataset\.groupName\)/);
  assert.match(nav.innerHTML, /deleteGroup\(this\.dataset\.groupName\)/);
});

test('toggleSessionGroupLimit expansion is ephemeral and resets on the filter-pill enter path', async () => {
  const nav = createElement();
  const workExtra = createElement({
    className: 'session-group-limit-extra hidden',
    dataset: {sessionGroupLimitExtra: 'Work'},
  });
  const personalExtra = createElement({
    className: 'session-group-limit-extra hidden',
    dataset: {sessionGroupLimitExtra: 'Personal'},
  });
  const workToggle = createElement({
    className: 'session-group-limit-toggle',
    dataset: {sgroupLimitToggleKey: 'Work'},
    textContent: 'Show all',
  });
  const personalToggle = createElement({
    className: 'session-group-limit-toggle',
    dataset: {sgroupLimitToggleKey: 'Personal'},
    textContent: 'Show all',
  });
  const workSessions = Array.from({length: 7}, (_, idx) =>
    makeSession(`work-${idx + 1}`, `Work ${idx + 1}`, {group: 'Work'}));
  const personalSessions = Array.from({length: 6}, (_, idx) =>
    makeSession(`personal-${idx + 1}`, `Personal ${idx + 1}`, {group: 'Personal'}));
  const sessions = [...workSessions, ...personalSessions];
  const {context} = buildContext({
    elements: new Map([['session-list', nav]]),
    querySelectorAll: (selector) => {
      if (selector === '.session-group-limit-extra') return [workExtra, personalExtra];
      if (selector === '.session-group-limit-toggle') return [workToggle, personalToggle];
      return [];
    },
  });

  context.toggleSessionGroupLimit('Work');

  assert.equal(workExtra.classList.contains('hidden'), false);
  assert.equal(personalExtra.classList.contains('hidden'), true);
  assert.equal(workToggle.textContent, 'Show less');
  assert.equal(workToggle.getAttribute('aria-expanded'), 'true');
  assert.equal(personalToggle.textContent, 'Show all');

  // Re-render by entering a different filter through the pill entry point:
  // expansion state lives in module memory only, so the enter path wipes it
  // and both groups fall back to the five-session preview.
  context.fetch = async (url) => {
    assert.equal(url, '/api/sessions/starred');
    return {ok: true, json: async () => sessions};
  };
  context.enterSidebarFilter('starred');
  await new Promise(setImmediate);

  assert.equal(sessionAnchorOpenTag(nav.innerHTML, 'work-5').includes('session-group-limit-extra'), false);
  assert.equal(sessionAnchorOpenTag(nav.innerHTML, 'work-6').includes('session-group-limit-extra hidden'), true);
  assert.equal(sessionAnchorOpenTag(nav.innerHTML, 'work-7').includes('session-group-limit-extra hidden'), true);
  assert.equal(sessionAnchorOpenTag(nav.innerHTML, 'personal-6').includes('session-group-limit-extra hidden'), true);
});

test('switchSidebarFilter owns no expansion reset and preserves expansion across filters', async () => {
  const nav = createElement();
  const {context} = buildContext({
    elements: new Map([['session-list', nav]]),
  });
  const sessions = Array.from({length: 7}, (_, idx) =>
    makeSession(`work-${idx + 1}`, `Work ${idx + 1}`, {group: 'Work'}));
  context.fetch = async () => ({ok: true, json: async () => sessions});

  // The render entry point must never reset the expansion table, not even when
  // the filter value differs from the current one; only the pill-enter path may.
  context.toggleSessionGroupLimit('Work');
  context.switchSidebarFilter('starred');
  await new Promise(setImmediate);

  assert.equal(context.currentFilter, 'starred');
  assert.equal(sessionAnchorOpenTag(nav.innerHTML, 'work-5').includes('session-group-limit-extra'), false);
  assert.equal(sessionAnchorOpenTag(nav.innerHTML, 'work-6').includes('session-group-limit-extra hidden'), false);
  assert.match(nav.innerHTML, />Show less<\/button>/);
});

test('enterSidebarFilter collapses expansions on a tab change and keeps them on the active pill', async () => {
  // Entering a different filter clears the expansion table.
  {
    const nav = createElement();
    const {context} = buildContext({
      elements: new Map([['session-list', nav]]),
    });
    const sessions = Array.from({length: 7}, (_, idx) =>
      makeSession(`work-${idx + 1}`, `Work ${idx + 1}`, {group: 'Work'}));
    context.fetch = async () => ({ok: true, json: async () => sessions});

    context.toggleSessionGroupLimit('Work');
    context.enterSidebarFilter('starred');
    await new Promise(setImmediate);

    assert.equal(context.currentFilter, 'starred');
    assert.equal(sessionAnchorOpenTag(nav.innerHTML, 'work-6').includes('session-group-limit-extra hidden'), true);
    assert.match(nav.innerHTML, />Show all<\/button>/);
    assert.doesNotMatch(nav.innerHTML, />Show less<\/button>/);
  }

  // Re-clicking the pill for the filter already shown keeps the expansion.
  {
    const nav = createElement();
    const {context} = buildContext({
      elements: new Map([['session-list', nav]]),
    });
    const sessions = Array.from({length: 7}, (_, idx) =>
      makeSession(`work-${idx + 1}`, `Work ${idx + 1}`, {group: 'Work'}));
    context.fetch = async () => ({ok: true, json: async () => sessions});

    context.toggleSessionGroupLimit('Work');
    context.enterSidebarFilter('all');
    await new Promise(setImmediate);

    assert.equal(context.currentFilter, 'all');
    assert.equal(sessionAnchorOpenTag(nav.innerHTML, 'work-6').includes('session-group-limit-extra hidden'), false);
    assert.match(nav.innerHTML, />Show less<\/button>/);
  }
});

test('in-place refresh paths preserve group expansion', async () => {
  const makeWorkSessions = () => Array.from({length: 7}, (_, idx) =>
    makeSession(`work-${idx + 1}`, `Work ${idx + 1}`, {group: 'Work'}));
  const paths = [
    {
      name: 'websocket session_group_changed event',
      setup(context) {
        vm.runInContext(WEBSOCKET_JS, context, {filename: 'websocket.js'});
      },
      drive(context) {
        context.handleWSEvent({type: 'session_group_changed', session_id: 'session-a', group: 'Work'}, 'session-a', 0);
      },
    },
    {
      name: 'setSessionGroup',
      async drive(context) {
        await context.setSessionGroup('work-1', 'Personal');
      },
    },
    {
      name: 'unarchiveSession',
      async drive(context) {
        await context.unarchiveSession('work-1');
      },
    },
    {
      name: 'toggleSessionStar unstarring on the starred filter',
      setup(context) {
        context.currentFilter = 'starred';
      },
      async drive(context) {
        await context.toggleSessionStar('work-1', true);
      },
    },
    {
      name: 'renameGroup',
      setup(context) {
        context.prompt = () => 'Renamed';
      },
      async drive(context) {
        await context.renameGroup('Work');
      },
    },
    {
      name: 'deleteGroup',
      async drive(context) {
        await context.deleteGroup('Work');
      },
    },
    {
      name: 'handleSidebarSearch exit',
      drive(context) {
        context.handleSidebarSearch('');
      },
    },
  ];

  for (const path of paths) {
    const nav = createElement();
    const {context} = buildContext({
      elements: new Map([['session-list', nav]]),
    });
    const sessions = makeWorkSessions();
    context.fetch = async (url, opts = {}) => {
      if (opts.method && opts.method !== 'GET') return {ok: true, async json() { return {}; }};
      return {ok: true, async json() { return sessions; }};
    };
    if (path.setup) path.setup(context);

    context.toggleSessionGroupLimit('Work');
    await path.drive(context);
    await new Promise(setImmediate);

    assert.equal(
      sessionAnchorOpenTag(nav.innerHTML, 'work-6').includes('session-group-limit-extra hidden'),
      false,
      `${path.name}: the refreshed group must stay expanded`
    );
    assert.match(nav.innerHTML, />Show less<\/button>/, `${path.name}: the toggle must read Show less`);
  }
});

test('page load renders every group collapsed with the five-row preview and a Show all toggle', () => {
  const nav = createElement();
  const {context} = buildContext({
    elements: new Map([['session-list', nav]]),
  });
  // A fresh page load starts with an empty expansion table; nothing has been
  // expanded yet, so every group renders at its preview length.
  const sessions = Array.from({length: 7}, (_, idx) =>
    makeSession(`work-${idx + 1}`, `Work ${idx + 1}`, {group: 'Work'}));

  context.renderSessionList(sessions, 'all');

  assert.match(nav.innerHTML, />Show all<\/button>/);
  assert.equal(sessionAnchorOpenTag(nav.innerHTML, 'work-5').includes('session-group-limit-extra'), false);
  assert.equal(sessionAnchorOpenTag(nav.innerHTML, 'work-6').includes('session-group-limit-extra hidden'), true);
  assert.equal(sessionAnchorOpenTag(nav.innerHTML, 'work-7').includes('session-group-limit-extra hidden'), true);
});

test('stale session-group-list-expanded localStorage seed stays inert', () => {
  const nav = createElement();
  const {context} = buildContext({
    elements: new Map([['session-list', nav]]),
    localStorageItems: {
      'session-group-list-expanded': '{"Work": true}',
    },
  });
  const sessions = Array.from({length: 7}, (_, idx) =>
    makeSession(`work-${idx + 1}`, `Work ${idx + 1}`, {group: 'Work'}));

  context.renderSessionList(sessions, 'archived');

  assert.match(nav.innerHTML, /session-group-limit-toggle/);
  assert.match(nav.innerHTML, />Show all<\/button>/);
  assert.equal(sessionAnchorOpenTag(nav.innerHTML, 'work-5').includes('session-group-limit-extra'), false);
  assert.equal(sessionAnchorOpenTag(nav.innerHTML, 'work-6').includes('session-group-limit-extra hidden'), true);
  assert.equal(sessionAnchorOpenTag(nav.innerHTML, 'work-7').includes('session-group-limit-extra hidden'), true);
});

test('group limit toggles and filter switches never write the expansion keys to localStorage', async () => {
  const nav = createElement();
  const {context, localStorageData} = buildContext({
    elements: new Map([['session-list', nav]]),
    localStorageItems: {
      'session-group-list-expanded': '{"Work": true}',
      'cron-group-list-expanded': '{"Nightly": true}',
    },
  });

  // Expand, collapse, expand the cron bucket, then a public filter-switch
  // round-trip; none of it may touch the seeded storage values.
  context.toggleSessionGroupLimit('Work');
  context.toggleSessionGroupLimit('Work');
  context.toggleCronGroupLimit('Nightly');
  context.fetch = async () => ({ok: true, json: async () => []});
  context.switchSidebarFilter('archived');
  await new Promise(setImmediate);
  context.switchSidebarFilter('all');
  await new Promise(setImmediate);

  assert.equal(localStorageData.get('session-group-list-expanded'), '{"Work": true}');
  assert.equal(localStorageData.get('cron-group-list-expanded'), '{"Nightly": true}');
});

test('renderSessionList keeps active grouped session visible outside the first five', () => {
  const nav = createElement();
  const {context} = buildContext({
    elements: new Map([['session-list', nav]]),
  });
  context.SESSION_ID = 'work-7';
  const sessions = Array.from({length: 7}, (_, idx) =>
    makeSession(`work-${idx + 1}`, `Work ${idx + 1}`, {group: 'Work'}));

  context.renderSessionList(sessions, 'all');

  assert.equal(sessionAnchorOpenTag(nav.innerHTML, 'work-6').includes('session-group-limit-extra hidden'), true);
  assert.equal(sessionAnchorOpenTag(nav.innerHTML, 'work-7').includes('session-group-limit-extra'), false);
  assert.equal(sessionAnchorOpenTag(nav.innerHTML, 'work-7').includes('bg-blue-600/20 text-blue-300'), true);
});

test('renderSessionList leaves search results flat and untrimmed', () => {
  const nav = createElement();
  const {context} = buildContext({
    elements: new Map([['session-list', nav]]),
  });
  const sessions = Array.from({length: 7}, (_, idx) =>
    makeSession(`search-${idx + 1}`, `Search ${idx + 1}`, {group: 'Work'}));

  context.renderSessionList(sessions, 'search');

  assert.doesNotMatch(nav.innerHTML, /class="session-group group"/);
  assert.doesNotMatch(nav.innerHTML, /session-group-limit-toggle/);
  assert.doesNotMatch(nav.innerHTML, /session-group-limit-extra/);
  assert.match(nav.innerHTML, /id="session-search-7"/);
});

test('renderGroupedScheduledList limits project groups to five visible sessions', () => {
  const nav = createElement();
  const {context} = buildContext({
    localStorageItems: {
      'cron-group-collapsed': JSON.stringify({Nightly: false}),
    },
    elements: new Map([['session-list', nav]]),
  });
  const sessions = Array.from({length: 7}, (_, idx) =>
    makeSession(`scheduled-${idx + 1}`, `Scheduled ${idx + 1}`, {
      schedule_project: 'Nightly',
      scheduled_task: 'nightly',
      schedule_enabled: true,
      schedule_cron: '0 2 * * *',
      schedule_timezone: 'UTC',
    }));

  context.renderSessionList(sessions, 'scheduled');

  assert.match(nav.innerHTML, /Nightly/);
  assert.match(nav.innerHTML, /cron-group-limit-toggle/);
  assert.match(nav.innerHTML, />Show all<\/button>/);
  assert.equal(sessionAnchorOpenTag(nav.innerHTML, 'scheduled-5').includes('cron-group-limit-extra'), false);
  assert.equal(sessionAnchorOpenTag(nav.innerHTML, 'scheduled-6').includes('cron-group-limit-extra hidden'), true);
  assert.equal(sessionAnchorOpenTag(nav.innerHTML, 'scheduled-7').includes('cron-group-limit-extra hidden'), true);
});

test('switchSession preserves worker icon until authoritative status returns', async () => {
  const workerIcon = createElement();
  const tabWorkers = createElement();
  const messages = createElement();
  const input = createElement();
  const {context} = buildContext({
    BACKEND_TYPES: {'claude-opus-4.6': 'claude-code'},
    elements: new Map([
      ['msg-input', input],
      ['header-session-name', createElement()],
      ['backend-badge', createElement()],
      ['input-model-badge', createElement()],
      ['messages', messages],
      ['tab-workers', tabWorkers],
      ['worker-indicator-session-b', workerIcon],
      ['spinner-session-b', createElement({className: 'hidden'})],
      ['unread-session-b', createElement({className: 'hidden'})],
    ]),
  });
  context.fetch = async (url) => {
    assert.equal(url, '/api/sessions/session-b/bootstrap');
    return {
      ok: true,
      async json() {
        return {
          session: makeSession('session-b', 'Session B'),
          messages: [],
          pending_draft: null,
          event_count: 0,
          active_backend: 'claude-opus-4.6',
          active_backend_type: 'claude-code',
          has_more: false,
        };
      },
    };
  };
  context.pollSessionStatus = () => new Promise(() => {});

  await context.switchSession('session-b');

  assert.equal(workerIcon.classList.contains('hidden'), false);
  assert.match(tabWorkers.innerHTML, /Loading worker threads/);
  assert.equal(context.location.href, '');
});

test('missing bootstrap worker data and empty worker tab do not imply idle', () => {
  const workerIcon = createElement();
  const tabWorkers = createElement();
  const {context} = buildContext({
    BACKEND_TYPES: {'claude-opus-4.6': 'claude-code'},
    elements: new Map([
      ['header-session-name', createElement()],
      ['backend-badge', createElement()],
      ['input-model-badge', createElement()],
      ['messages', createElement()],
      ['tab-workers', tabWorkers],
      ['worker-indicator-session-a', workerIcon],
      ['spinner-session-a', createElement({className: 'hidden'})],
      ['unread-session-a', createElement({className: 'hidden'})],
    ]),
  });
  let statusPolls = 0;
  context.pollSessionStatus = () => {
    statusPolls += 1;
    return Promise.resolve(false);
  };

  context.renderSessionView({
    session: makeSession('session-a', 'Session A'),
    messages: [],
    pending_draft: null,
    event_count: 0,
    active_backend: 'claude-opus-4.6',
    active_backend_type: 'claude-code',
    has_more: false,
  });
  context.updateSpinner();

  assert.match(tabWorkers.innerHTML, /Loading worker threads/);
  assert.equal(workerIcon.classList.contains('hidden'), false);
  assert.equal(statusPolls, 1);
});

test('loadOlderIfNeeded post-processes prepended messages through the shared helper', async () => {
  const messages = createElement({scrollTop: 0});
  messages.clientHeight = 500;
  messages.scrollHeight = 1000;
  const {context} = buildContext({
    elements: new Map([
      ['messages', messages],
    ]),
  });
  const postProcessedHtml = [];
  context.postProcessRenderedMessages = (root) => {
    postProcessedHtml.push(root.innerHTML);
  };
  context.fetch = async (url) => {
    assert.equal(url, '/api/sessions/session-a/events?before=4&limit=40');
    return {
      ok: true,
      async json() {
        return {
          has_more: false,
          next_before: 2,
          messages: [{
            role: 'assistant',
            content: 'older $x$',
            event_index: 3,
            timestamp: '2026-04-02T04:00:00Z',
          }],
        };
      },
    };
  };

  context.renderSessionView({
    session: {id: 'session-a', backend: 'claude-opus-4.6', round_ratings: {}},
    messages: [{role: 'assistant', content: 'newer', event_index: 5}],
    pending_draft: null,
    event_count: 6,
    oldest_message_ordinal: 4,
    active_backend: 'claude-opus-4.6',
    active_backend_type: '',
    has_more: true,
  });
  postProcessedHtml.length = 0;
  messages.scrollTop = 0;

  await context.loadOlderIfNeeded(messages);

  assert.equal(postProcessedHtml.length, 1);
  assert.match(postProcessedHtml[0], /older \$x\$/);
});

test('loadOlderIfNeeded skips messages whose rendered id is already in the DOM', async () => {
  const messages = createElement({scrollTop: 0});
  messages.clientHeight = 500;
  messages.scrollHeight = 1000;
  const {context} = buildContext({
    elements: new Map([
      ['messages', messages],
    ]),
  });
  let renderedMessages = null;
  context.isRenderedMessage = (msg) => msg.id === 'assistant-event-1';
  context.renderMessagesToDetachedContainer = (msgs) => {
    renderedMessages = msgs;
    return createElement();
  };
  context.fetch = async (url) => {
    assert.equal(url, '/api/sessions/session-a/events?before=4&limit=40');
    return {
      ok: true,
      async json() {
        return {
          has_more: false,
          next_before: 2,
          messages: [
            {
              id: 'assistant-event-1',
              role: 'assistant',
              content: 'already visible',
              event_index: 3,
            },
            {
              id: 'user-event-0',
              role: 'user',
              content: 'older ask',
              event_index: 1,
            },
          ],
        };
      },
    };
  };

  context.renderSessionView({
    session: {id: 'session-a', backend: 'claude-opus-4.6', round_ratings: {}},
    messages: [{id: 'assistant-event-1', role: 'assistant', content: 'already visible', event_index: 5}],
    pending_draft: null,
    event_count: 6,
    oldest_message_ordinal: 4,
    active_backend: 'claude-opus-4.6',
    active_backend_type: '',
    has_more: true,
  });
  messages.scrollTop = 0;

  await context.loadOlderIfNeeded(messages);

  assert.deepEqual(renderedMessages, [{
    id: 'user-event-0',
    role: 'user',
    content: 'older ask',
    event_index: 1,
  }]);
});

test('renderMessage stamps committed messages with stable message ids', () => {
  const {context} = buildContext();

  const html = context.renderMessage({
    id: 'assistant-event-1',
    role: 'assistant',
    content: 'hello',
    event_index: 5,
  }, 'session-a');

  assert.match(html, /data-message-id="assistant-event-1"/);
});

test('renderSessionItem shows separate delayed-trigger and scheduled indicators', () => {
  const {context} = buildContext({
    BACKEND_TYPES: {'claude-tui': 'tui-cli'},
  });

  const html = context.renderSessionItem({
    id: 'session-a',
    name: 'Wake later',
    backend: 'claude-tui',
    updated_at: '2026-04-02T04:00:00Z',
    has_running_tasks: false,
    has_unread: false,
    has_pending_trigger: true,
    pending_trigger_count: 1,
    next_trigger_at: '2026-04-02T06:00:00Z',
    scheduled_task: 'nightly',
    schedule_enabled: true,
    starred: false,
  }, 'search');

  assert.match(html, /id="pending-trigger-session-a"/);
  assert.match(html, /1 pending delayed trigger/);
  assert.match(html, /Scheduled: nightly/);
  assert.match(html, /<\/svg>\s*<span class="tui-status-dot w-2 h-2 rounded-full flex-shrink-0"/);
  assert.match(html, /<span class="tui-status-dot w-2 h-2 rounded-full flex-shrink-0" data-session-id="session-a" title="Claude stopped"><\/span>\s*<span class="flex-1 min-w-0">/);
  assert.match(html, /<span class="truncate block session-name">Wake later<\/span>/);
  assert.doesNotMatch(html, /session-name">[^<]*<span class="tui-status-dot/);
});

test('renderScheduledSessionItem keeps delayed-trigger and cron indicators distinct', () => {
  const {context} = buildContext({
    BACKEND_TYPES: {'claude-tui': 'tui-cli'},
  });

  const html = context.renderScheduledSessionItem({
    id: 'session-a',
    name: 'Wake later',
    backend: 'claude-tui',
    has_running_tasks: false,
    has_unread: false,
    has_pending_trigger: true,
    pending_trigger_count: 2,
    next_trigger_at: '2026-04-02T06:00:00Z',
    scheduled_task: 'nightly',
    schedule_enabled: true,
    schedule_cron: '0 2 * * *',
    schedule_timezone: 'UTC',
    starred: false,
  });

  assert.match(html, /id="pending-trigger-session-a"/);
  assert.match(html, /2 pending delayed triggers/);
  assert.match(html, /Scheduled: nightly/);
  assert.match(html, /<\/svg>\s*<span class="tui-status-dot w-2 h-2 rounded-full flex-shrink-0"/);
  assert.match(html, /<span class="tui-status-dot w-2 h-2 rounded-full flex-shrink-0" data-session-id="session-a" title="Claude stopped"><\/span>\s*<span class="flex-1 min-w-0">/);
  assert.match(html, /<span class="truncate block session-name">Wake later<\/span>/);
  assert.doesNotMatch(html, /session-name">[^<]*<span class="tui-status-dot/);
});

test('renderSessionItem omits tui dot for non-tui backend', () => {
  const {context} = buildContext({
    BACKEND_TYPES: {'codex-o3': 'codex-cli'},
  });

  const html = context.renderSessionItem({
    id: 'session-a',
    name: 'SDK session',
    backend: 'codex-o3',
    updated_at: '2026-04-02T04:00:00Z',
    has_running_tasks: false,
    has_unread: false,
    has_pending_trigger: false,
    pending_trigger_count: 0,
    next_trigger_at: null,
    scheduled_task: '',
    starred: false,
  }, 'search');

  assert.doesNotMatch(html, /tui-status-dot/);
});

test('renderTuiStatusDot reflects stopped, idle, and busy states', () => {
  const {context} = buildContext({
    BACKEND_TYPES: {'claude-tui': 'tui-cli'},
  });

  context.TuiStatusMap = {
    stopped: {running: false, busy: false},
    idle: {running: true, busy: false},
    busy: {running: true, busy: true},
  };

  assert.equal(
    context.renderTuiStatusDot({id: 'stopped', backend: 'claude-tui'}),
    '<span class="tui-status-dot w-2 h-2 rounded-full flex-shrink-0" data-session-id="stopped" title="Claude stopped"></span>'
  );
  assert.equal(
    context.renderTuiStatusDot({id: 'idle', backend: 'claude-tui'}),
    '<span class="tui-status-dot w-2 h-2 rounded-full flex-shrink-0 running" data-session-id="idle" title="Claude idle"></span>'
  );
  assert.equal(
    context.renderTuiStatusDot({id: 'busy', backend: 'claude-tui'}),
    '<span class="tui-status-dot w-2 h-2 rounded-full flex-shrink-0 running busy" data-session-id="busy" title="Claude busy"></span>'
  );
});

test('refreshTuiDots updates dot classes, titles, and Stop button visibility', () => {
  const busyDot = createElement({className: 'tui-status-dot', dataset: {sessionId: 'busy'}});
  const idleDot = createElement({className: 'tui-status-dot busy', dataset: {sessionId: 'idle'}});
  const stoppedDot = createElement({className: 'tui-status-dot running busy', dataset: {sessionId: 'stopped'}});
  const stopBtn = createElement();
  const {context} = buildContext({
    elements: new Map([['stop-tui-btn', stopBtn]]),
    querySelectorAll: (selector) => {
      assert.equal(selector, '.tui-status-dot[data-session-id]');
      return [busyDot, idleDot, stoppedDot];
    },
  });

  context.ACTIVE_BACKEND_TYPE = 'tui-cli';
  context.SESSION_ID = 'stopped';
  context.TuiStatusMap = {
    busy: {running: true, busy: true},
    idle: {running: true, busy: false},
    stopped: {running: false, busy: false},
  };

  context.refreshTuiDots();

  assert.equal(busyDot.classList.contains('running'), true);
  assert.equal(busyDot.classList.contains('busy'), true);
  assert.equal(busyDot.title, 'Claude busy');
  assert.equal(idleDot.classList.contains('running'), true);
  assert.equal(idleDot.classList.contains('busy'), false);
  assert.equal(idleDot.title, 'Claude idle');
  assert.equal(stoppedDot.classList.contains('running'), false);
  assert.equal(stoppedDot.classList.contains('busy'), false);
  assert.equal(stoppedDot.title, 'Claude stopped');
  assert.equal(stopBtn.classList.contains('hidden'), true);

  context.SESSION_ID = 'idle';
  context.refreshTuiDots();
  assert.equal(stopBtn.classList.contains('hidden'), false);
});

test('createSession switches open chat through SPA state without full reload', async () => {
  const input = createElement({value: 'old draft'});
  const backendSelect = createElement({tagName: 'SELECT', value: 'codex-o3'});
  const {context, fetchRequests} = buildContext({
    ACTIVE_BACKEND_ID: 'codex-o3',
    BACKEND_TYPES: {'codex-o3': 'codex'},
    elements: new Map([
      ['msg-input', input],
      ['new-session-backend', backendSelect],
    ]),
  });
  let rendered = null;
  let pushedUrl = null;
  let connected = false;

  context.renderSessionView = (data) => { rendered = data; };
  context.history.pushState = (_state, _title, url) => { pushedUrl = url; };
  context.connectWS = () => { connected = true; };

  await context.createSession();

  assert.equal(fetchRequests[0].url, '/api/sessions/');
  assert.deepEqual(JSON.parse(fetchRequests[0].opts.body), {backend: 'codex-o3'});
  assert.equal(context.location.href, '');
  assert.equal(context.SESSION_ID, 'session-b');
  assert.equal(context.DRAFT_KEY, 'charliebot-draft-session-b');
  assert.equal(context.eventCursor, 0);
  assert.equal(pushedUrl, '/?session=session-b');
  assert.equal(connected, true);
  assert.equal(input.value, '');
  assert.deepEqual(JSON.parse(JSON.stringify(rendered)), {
    session: {id: 'session-b', backend: 'codex-o3'},
    messages: [],
    pending_draft: null,
    event_count: 0,
    oldest_message_ordinal: 0,
    active_backend: 'codex-o3',
    active_backend_type: 'codex',
    switchable_backends: [],
    has_more: false,
  });
});

test('archiveSession removes the row inline and switches to the next rendered session without refetching', async () => {
  const nav = createElement();
  const input = createElement();
  const rowA = createElement({tagName: 'A', id: 'session-session-a'});
  const {context} = buildContext({
    elements: new Map([
      ['session-list', nav],
      ['msg-input', input],
      ['session-session-a', rowA],
      ...buildSidebarFilterElements(),
    ]),
    querySelectorAll: (selector) =>
      selector === 'a[id^="session-"]' ? sessionAnchors(['session-b']) : [],
  });
  const requests = [];
  let rendered = null;
  let pushedUrl = null;
  context.fetch = async (url, opts = {}) => {
    requests.push({url, opts});
    if (url === '/api/sessions/session-a') {
      assert.equal(opts.method, 'DELETE');
      return {ok: true, async json() { return {}; }};
    }
    if (url === '/api/diag/switch-events') {
      assert.equal(opts.method, 'POST');
      return {ok: true, async json() { return {ok: true}; }};
    }
    if (url === '/api/sessions/session-b/bootstrap') {
      return {
        ok: true,
        async json() {
          return {
            session: makeSession('session-b', 'Backend Session B'),
            messages: [],
            pending_draft: null,
            event_count: 0,
            active_backend: 'claude-opus-4.6',
            active_backend_type: '',
            has_more: false,
          };
        },
      };
    }
    throw new Error('unexpected fetch ' + url);
  };
  context.renderSessionView = (data) => { rendered = data; };
  context.history.pushState = (_state, _title, url) => { pushedUrl = url; };
  context.pollSessionStatus = () => Promise.resolve(false);

  await context.archiveSession('session-a');

  assert.deepEqual(requests.map((req) => req.url), [
    '/api/sessions/session-a',
    '/api/diag/switch-events',
    '/api/sessions/session-b/bootstrap',
    '/api/diag/switch-events',
  ]);
  assert.equal(rowA.removed, true);
  assert.equal(context.location.href, '');
  assert.equal(pushedUrl, '/?session=session-b');
  assert.equal(context.SESSION_ID, 'session-b');
  assert.equal(rendered.session.id, 'session-b');
});

test('deleteSessionPermanently renders the welcome state inline when no rendered session remains', async () => {
  const nav = createElement();
  const main = createElement({tagName: 'MAIN'});
  const rowA = createElement({tagName: 'A', id: 'session-session-a'});
  const {context} = buildContext({
    elements: new Map([
      ['session-list', nav],
      ['session-session-a', rowA],
      ...buildSidebarFilterElements(),
      ['welcome-view', createElement({tagName: 'TEMPLATE', innerHTML: '<h2>Welcome to CharlieBot</h2>'})],
    ]),
    querySelector: (selector) => {
      if (selector === 'main') return main;
      return null;
    },
  });
  const requests = [];
  let pushedUrl = null;
  context.fetch = async (url, opts = {}) => {
    requests.push({url, opts});
    if (url === '/api/sessions/session-a/permanent') {
      assert.equal(opts.method, 'DELETE');
      return {ok: true, async json() { return {}; }};
    }
    throw new Error('unexpected fetch ' + url);
  };
  context.history.pushState = (_state, _title, url) => { pushedUrl = url; };

  await context.deleteSessionPermanently('session-a');

  assert.deepEqual(requests.map((req) => req.url), [
    '/api/sessions/session-a/permanent',
  ]);
  assert.equal(rowA.removed, true);
  assert.equal(context.location.href, '');
  assert.equal(pushedUrl, '/');
  assert.equal(context.SESSION_ID, null);
  assert.match(main.innerHTML, /Welcome to CharlieBot/);
});

test('saveCronTask sends backend selector value and null inherit value', async () => {
  const requests = [];
  const cronModal = createElement({className: 'hidden'});
  const elements = new Map([
    ['cron-modal-title', createElement()],
    ['cron-name', createElement()],
    ['cron-expr', createElement()],
    ['cron-prompt-file', createElement()],
    ['cron-repo', createElement()],
    ['cron-backend', createElement({tagName: 'SELECT'})],
    ['cron-project', createElement()],
    ['cron-timezone', createElement()],
    ['cron-enabled', createElement({checked: true})],
    ['cron-delete-btn', createElement({className: 'hidden'})],
    ['cron-save-btn', createElement()],
    ['cron-error-box', createElement({className: 'hidden'})],
    ['cron-modal', cronModal],
  ]);
  const {context} = buildContext({elements});
  context.fetch = async (url, opts = {}) => {
    requests.push({url, opts});
    return {ok: true, async json() { return {}; }, async text() { return ''; }};
  };
  context.switchSidebarFilter = () => {};

  context.openCronAdder();
  elements.get('cron-name').value = 'nightly';
  elements.get('cron-expr').value = '0 2 * * *';
  elements.get('cron-prompt-file').value = 'prompts/nightly.md';
  elements.get('cron-backend').value = '';

  await context.saveCronTask();

  let body = JSON.parse(requests[0].opts.body);
  assert.equal(requests[0].url, '/api/cron/tasks');
  assert.equal(body.backend, null);
  assert.equal(body.prompt_file, 'prompts/nightly.md');
  assert.equal(body.prompt, undefined);

  context.openCronAdder();
  elements.get('cron-name').value = 'nightly-codex';
  elements.get('cron-expr').value = '0 3 * * *';
  elements.get('cron-prompt-file').value = 'prompts/nightly-codex.md';
  elements.get('cron-backend').value = 'codex-o3';

  await context.saveCronTask();

  body = JSON.parse(requests[1].opts.body);
  assert.equal(requests[1].url, '/api/cron/tasks');
  assert.equal(body.backend, 'codex-o3');
  assert.equal(body.prompt_file, 'prompts/nightly-codex.md');
  assert.equal(body.prompt, undefined);
});

test('startTuiStatusPolling polls TUI status every three seconds', () => {
  const {context, intervals} = buildContext();

  context.startTuiStatusPolling();

  assert.equal(intervals.length, 1);
  assert.equal(intervals[0].ms, 3000);
});

test('switchSession reconnects when clicking the active stopped TUI session', async () => {
  const {context, timeouts} = buildContext();
  let disconnected = false;
  let connected = false;
  context.TuiStatusMap = {'session-a': {running: false, busy: false}};
  context.disconnectWS = () => { disconnected = true; };
  context.connectWS = () => { connected = true; };

  await context.switchSession('session-a');

  assert.equal(disconnected, true);
  assert.equal(connected, true);
  assert.equal(timeouts.length, 1);
  assert.equal(timeouts[0].ms, 1500);
});

test('stopActiveTui writes stopped object status before refreshing dots', async () => {
  const {context} = buildContext();
  let stoppedUrl = null;
  let refreshed = false;
  let bannerShown = false;
  context.fetch = async (url, opts = {}) => {
    stoppedUrl = url;
    assert.equal(opts.method, 'POST');
    return {
      ok: true,
      async json() {
        return {stopped: true};
      },
    };
  };
  context.refreshTuiDots = () => { refreshed = true; };
  context.TuiSession = {showStoppedBanner: () => { bannerShown = true; }};

  await context.stopActiveTui();

  assert.equal(stoppedUrl, '/api/sessions/session-a/tui/stop');
  const stoppedStatus = vm.runInContext('globalThis.TuiStatusMap["session-a"]', context);
  assert.equal(stoppedStatus.running, false);
  assert.equal(stoppedStatus.busy, false);
  assert.equal(refreshed, true);
  assert.equal(bannerShown, true);
});

test('updateSidebarSessionName leaves sibling tui dot untouched', () => {
  const nameEl = {textContent: 'Old name'};
  const tuiDot = {className: 'tui-status-dot w-2 h-2 rounded-full flex-shrink-0'};
  const link = {
    children: [tuiDot, nameEl],
    querySelector(selector) {
      assert.equal(selector, '.session-name');
      return nameEl;
    },
  };
  const {context} = buildContext({
    elements: new Map([['session-session-a', link]]),
  });

  context.updateSidebarSessionName('session-a', 'New name');

  assert.equal(nameEl.textContent, 'New name');
  assert.deepEqual(link.children, [tuiDot, nameEl]);
});

test('forkSession opens the reusable modal with the active backend selected by default', () => {
  const elements = buildSessionActionElements();
  const {context} = buildContext({
    ACTIVE_BACKEND_ID: 'codex-o3',
    BACKEND_OPTIONS: {
      'claude-opus-4.6': 'Opus',
      'codex-o3': 'Codex',
    },
    elements,
  });

  context.forkSession('session-a');

  assert.equal(elements.get('session-action-modal-overlay').classList.contains('hidden'), false);
  assert.equal(elements.get('session-action-backend').value, 'codex-o3');
  assert.deepEqual(
    elements.get('session-action-backend').options.map((option) => option.value),
    ['claude-opus-4.6', 'codex-o3']
  );
  assert.equal(elements.get('session-action-modal-confirm').textContent, 'Clone');
});

test('closeSessionActionModal hides the modal without sending a request', () => {
  const elements = buildSessionActionElements();
  const {context, fetchCalls} = buildContext({
    ACTIVE_BACKEND_ID: 'codex-o3',
    BACKEND_OPTIONS: {'codex-o3': 'Codex'},
    elements,
  });

  context.forkSession('session-a');
  context.closeSessionActionModal();

  assert.equal(elements.get('session-action-modal-overlay').classList.contains('hidden'), true);
  assert.deepEqual(fetchCalls, []);
});

test('submitSessionActionModal sends backend and null event_index for header clone', async () => {
  const elements = buildSessionActionElements();
  const {context, fetchRequests} = buildContext({
    ACTIVE_BACKEND_ID: 'codex-o3',
    BACKEND_OPTIONS: {
      'claude-opus-4.6': 'Opus',
      'codex-o3': 'Codex',
    },
    elements,
  });

  context.forkSession('session-a');
  await context.submitSessionActionModal();

  assert.equal(fetchRequests[0].url, '/api/sessions/session-a/fork');
  assert.deepEqual(JSON.parse(fetchRequests[0].opts.body), {
    event_index: null,
    backend: 'codex-o3',
  });
});

test('submitSessionActionModal sends backend and event_index for message-level clone and Elon-e', async () => {
  const elements = buildSessionActionElements();
  const {context, fetchRequests} = buildContext({
    ACTIVE_BACKEND_ID: 'claude-opus-4.6',
    BACKEND_OPTIONS: {
      'claude-opus-4.6': 'Opus',
      'codex-o3': 'Codex',
    },
    elements,
  });

  context.forkSession('session-a', 12);
  elements.get('session-action-backend').value = 'codex-o3';
  await context.submitSessionActionModal();

  context.eloneSession('session-a', 18);
  await context.submitSessionActionModal();

  assert.equal(fetchRequests[0].url, '/api/sessions/session-a/fork');
  assert.deepEqual(JSON.parse(fetchRequests[0].opts.body), {
    event_index: 12,
    backend: 'codex-o3',
  });
  assert.equal(fetchRequests[1].url, '/api/sessions/session-a/elone');
  assert.deepEqual(JSON.parse(fetchRequests[1].opts.body), {
    event_index: 18,
    backend: 'claude-opus-4.6',
  });
});

test('switchBackend confirms before POSTing when the switch rotates', async () => {
  const confirms = [];
  const {context, fetchRequests} = buildContext({
    confirm: () => {
      confirms.push(1);
      return false;
    },
  });
  context.setBackendSwitchRotates(true);

  await context.switchBackend('codex-o3');

  assert.equal(confirms.length, 1, 'rotating switch must confirm once');
  assert.deepEqual(fetchRequests, [], 'cancelled switch must not POST');
});

test('switchBackend does not confirm for a non-rotating switch', async () => {
  const confirms = [];
  const {context, fetchRequests} = buildContext({
    ACTIVE_BACKEND_ID: 'claude-opus-4.6',
    confirm: () => {
      confirms.push(1);
      return true;
    },
  });
  context.setBackendSwitchRotates(false);
  context.fetch = async (url, opts = {}) => {
    assert.equal(url, '/api/sessions/session-a/backend');
    return {ok: true, async json() { return {id: 'session-a', backend: 'codex-o3'}; }};
  };

  await context.switchBackend('codex-o3');

  assert.equal(confirms.length, 0, 'in-place switch must not confirm');
  assert.equal(context.getActiveBackendId(), 'codex-o3');
});

test('switchBackend navigates to the new session when the rotation returns a different id', async () => {
  const {context} = buildContext({
    ACTIVE_BACKEND_ID: 'claude-opus-4.6',
    confirm: () => true,
  });
  context.setBackendSwitchRotates(true);
  context.currentFilter = 'all';
  const filtersRefreshed = [];
  const pushedUrls = [];
  context.switchSidebarFilter = (f) => { filtersRefreshed.push(f); };
  context.history.pushState = (_state, _title, url) => { pushedUrls.push(url); };
  context.fetch = async (url, opts = {}) => {
    if (url === '/api/sessions/session-a/backend') {
      assert.equal(opts.method, 'POST');
      return {ok: true, async json() { return {id: 'session-new', backend: 'codex-o3'}; }};
    }
    if (url === '/api/sessions/session-new/bootstrap') {
      return {
        ok: true,
        async json() {
          return {
            session: makeSession('session-new', 'Session New', {backend: 'codex-o3'}),
            messages: [],
            pending_draft: null,
            event_count: 0,
            active_backend: 'codex-o3',
            active_backend_type: 'codex',
            has_more: false,
          };
        },
      };
    }
    throw new Error('unexpected fetch ' + url);
  };

  await context.switchBackend('codex-o3');

  assert.equal(context.SESSION_ID, 'session-new');
  assert.deepEqual(pushedUrls, ['/?session=session-new']);
  assert.deepEqual(filtersRefreshed, ['all'], 'must refresh the sidebar');
  assert.equal(context.getActiveBackendId(), 'codex-o3');
});

test('switchBackend stays in place and skips sidebar refresh when the rotation returns the same id', async () => {
  const {context} = buildContext({
    ACTIVE_BACKEND_ID: 'claude-opus-4.6',
    confirm: () => true,
  });
  context.setBackendSwitchRotates(false);
  const filtersRefreshed = [];
  context.switchSidebarFilter = (f) => { filtersRefreshed.push(f); };
  context.fetch = async (url, opts = {}) => {
    if (url === '/api/sessions/session-a/backend') {
      return {ok: true, async json() { return {id: 'session-a', backend: 'codex-o3'}; }};
    }
    throw new Error('unexpected fetch ' + url);
  };

  await context.switchBackend('codex-o3');

  assert.equal(context.SESSION_ID, 'session-a', 'same id must not navigate');
  assert.deepEqual(filtersRefreshed, [], 'same id must not refresh the sidebar');
  assert.equal(context.getActiveBackendId(), 'codex-o3');
});
