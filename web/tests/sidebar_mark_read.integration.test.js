const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const { createFakeWebSocketClass } = require('./fake_websocket');
const { readStatic } = require('./read_static');
const { bootstrapPayload, buildSidebarFilterElements, buildSwitchFlowHarness, makeSessionMeta, makeSidebarRow,
  sessionColumns } = require('./session_context_stub');

const flush = () => new Promise((resolve) => setImmediate(resolve));

// ---------------------------------------------------------------------------
// Switch flow: the POST lands only on the winning generation's landed render,
// through the shared switch-flow harness in session_context_stub.js.
// ---------------------------------------------------------------------------

const BOOTSTRAP = {
  'session-b': bootstrapPayload('session-b', 0, false),
  'session-c': bootstrapPayload('session-c', 0, false),
};

function buildHarness() {
  return buildSwitchFlowHarness({
    rows: [
      makeSidebarRow('session-a', 'Alpha'),
      makeSidebarRow('session-b', 'Beta'),
      makeSidebarRow('session-c', 'Gamma'),
    ],
    scrollHeight: 100,
    fields: {readPosts: [], pendingBootstraps: {}},
    fetch: (h, url, opts = {}) => {
      if (url.includes('/read') && opts.method === 'POST') {
        h.readPosts.push(url);
        return Promise.resolve({ok: true, json: async () => ({})});
      }
      const boot = url.match(/\/api\/sessions\/([^/]+)\/bootstrap/);
      if (boot) {
        const gate = h.pendingBootstraps[boot[1]];
        if (gate) return gate.promise;
        return Promise.resolve({ok: true, status: 200, json: async () => BOOTSTRAP[boot[1]]});
      }
      return Promise.resolve({ok: true, status: 200, json: async () => ({})});
    },
  });
}

// Park one session's bootstrap fetch so the test owns when that switch's
// response lands (the generation check runs after it).
function parkBootstrap(h, sessionId) {
  const gate = {};
  gate.promise = new Promise((resolve) => { gate.resolve = resolve; });
  gate.keep = () => gate.resolve({ok: true, status: 200, json: async () => BOOTSTRAP[sessionId]});
  return gate;
}

test('a completed switch posts /read for the switched-to session once', async () => {
  const h = buildHarness();

  await h.context.switchSession('session-b');

  assert.deepEqual(h.readPosts, ['/api/sessions/session-b/read?page_dot=false']);
  assert.equal(h.context.ownUnread('session-b'), false, 'the local unread clear stays');
  assert.equal(h.context.SESSION_ID, 'session-b');
});

test('a superseded switch generation never posts /read', async () => {
  const h = buildHarness();
  h.pendingBootstraps['session-b'] = parkBootstrap(h, 'session-b');

  const superseded = h.context.switchSession('session-b'); // parks on B's bootstrap
  await h.context.switchSession('session-c');              // wins: renders and posts
  h.pendingBootstraps['session-b'].keep();                 // B lands after; generation check drops it
  await superseded;

  assert.deepEqual(h.readPosts, ['/api/sessions/session-c/read?page_dot=false'],
      'the superseded generation must return before the POST, not after it');
});

test('a render error posts no /read', async () => {
  const h = buildHarness();
  h.context.renderSessionView = () => { throw new Error('boom'); };

  await assert.rejects(h.context.switchSession('session-b'), /boom/);

  assert.deepEqual(h.readPosts, []);
});

// ---------------------------------------------------------------------------
// The unread dot against the server's flag, on one page socket that stays open
// across switches. A fake server owns each session's flag: a reply sets it, the
// read POST clears it, and each flip goes out as an unread_changed frame on the
// socket. The list, row and status answers read the same flags. The page runs
// the real switch, socket and sidebar modules. Each list paint rebuilds the
// row indicator elements from its HTML, as the browser does, so dotShown reads
// the dot the person sees.
// ---------------------------------------------------------------------------

const SESSION_IDS = ['session-a', 'session-b', 'session-c'];

function buildOpenSocketPage({unread}) {
  const server = {flags: new Map(SESSION_IDS.map((id) => [id, unread.includes(id)])), moved: new Map(), version: 0};
  const listeners = new Map();
  const h = buildSwitchFlowHarness({
    rows: [],
    scrollHeight: 100,
    fields: {readPosts: [], readDots: [], server, held: {}},
    fetch: (harness, url, opts = {}) => answer(url, opts),
    contextTweaks: (context, harness) => {
      context.WebSocket = createFakeWebSocketClass();
      context.wsUrlWithToken = (path) => path;
      context.TextEncoder = TextEncoder;
      context.INITIAL_SESSIONS = sessionColumns(SESSION_IDS.map(row));
      context.INITIAL_LOAD_ERRORS = [];
      context.document.addEventListener = (type, fn) => {
        if (!listeners.has(type)) listeners.set(type, []);
        listeners.get(type).push(fn);
      };
      for (const [id, element] of buildSidebarFilterElements()) harness.elements.set(id, element);
      harness.elements.set('session-list', sessionListElement(harness.elements));
    },
  });
  const {context} = h;
  vm.runInContext(readStatic('websocket.js'), context, {filename: 'websocket.js'});
  vm.runInContext(readStatic('websocket-handlers.js'), context, {filename: 'websocket-handlers.js'});

  function row(id) {
    return makeSessionMeta(id, {group: 'Work', has_unread: server.flags.get(id)});
  }

  // The store saves the flag and broadcasts the flip on the sidebar channel; the frame reaches the
  // page after the request that caused it returns.
  function setFlag(id, value) {
    if (server.flags.get(id) === value) return;
    server.flags.set(id, value);
    server.moved.set(id, ++server.version);
    setImmediate(() => h.socket().emitMessage({type: 'unread_changed', session_id: id, has_unread: value}));
  }

  function statusAnswer(url) {
    const since = Number(new URL(url, 'http://page').searchParams.get('since') || -1);
    const rows = {};
    for (const id of SESSION_IDS) {
      if ((server.moved.get(id) || 0) <= since) continue;
      rows[id] = {has_unread: server.flags.get(id), has_running_tasks: false, thinking_since: null,
        has_pending_trigger: false, pending_trigger_count: 0, next_trigger_at: null};
    }
    return {token: String(server.version), rows, gone: []};
  }

  // A route named in h.held answers later: the body is read now, at request time, and delivered when the test
  // calls the release h.held[route] holds.
  function answer(url, opts) {
    const ok = (route, body) => {
      const response = {ok: true, status: 200, json: async () => body};
      if (!(route in h.held)) return Promise.resolve(response);
      return new Promise((resolve) => { h.held[route] = () => resolve(response); });
    };
    const read = url.match(/^\/api\/sessions\/([^/?]+)\/read(?:\?page_dot=(true|false))?$/);
    if (read && opts.method === 'POST') {
      h.readPosts.push(read[1]);
      // The dot a click's read reports, beside the flag the server held: the route logs a mismatch.
      if (read[2]) h.readDots.push({id: read[1], pageDot: read[2] === 'true', serverUnread: server.flags.get(read[1])});
      setFlag(read[1], false);
      return ok('read', {session_id: read[1], has_unread: false});
    }
    const boot = url.match(/^\/api\/sessions\/([^/]+)\/bootstrap/);
    if (boot) return ok('bootstrap', bootstrapPayload(boot[1], 0, false));
    if (url.startsWith('/api/sessions/status?')) return ok('status', statusAnswer(url));
    const ids = url.match(/^\/api\/sessions\/\?compact=1&ids=(.+)$/);
    if (ids) {
      return ok('rows', {sessions: sessionColumns(decodeURIComponent(ids[1]).split(',').map(row)), gone: []});
    }
    if (url === '/api/sessions/?compact=1') return ok('list', sessionColumns(SESSION_IDS.map(row)));
    return ok('other', {});
  }

  h.socket = () => context.WebSocket.instances[context.WebSocket.instances.length - 1];
  h.dotShown = (id) => !h.elements.get('unread-' + id).classList.contains('hidden');
  // The page load: the server-rendered list paints, then the page's one socket opens.
  h.load = async () => {
    context.restoreSidebarFromUrl();
    vm.runInContext('connectWS()', context);
    h.socket().emitOpen();
    await flush();
  };
  // A click on a row: the switch lands, and the socket answers its subscribe frame.
  h.switchTo = async (id) => {
    await context.switchSession(id);
    const subscribe = JSON.parse(h.socket().sent[h.socket().sent.length - 1]);
    h.socket().emitMessage({type: 'subscribed', session_id: id, subscription: subscribe.subscription});
    h.socket().emitMessage({type: 'catchup_complete', session_id: id, subscription: subscribe.subscription,
      event_total: 6});
    await flush();
  };
  // A reply lands in a session: the server marks it unread.
  h.reply = async (id) => {
    setFlag(id, true);
    await flush();
    await flush();
  };
  // Another session's task fact: the list refreshes that row and the Workspace badges, and both repaint the list.
  h.otherSessionChanged = async (id) => {
    h.socket().emitMessage({type: 'task_tree_changed', session_id: id});
    await flush();
    await flush();
  };
  h.setHidden = (hidden) => {
    context.document.hidden = hidden;
    context.document.visibilityState = hidden ? 'hidden' : 'visible';
    (listeners.get('visibilitychange') || []).forEach((fn) => fn());
  };
  return h;
}

// The session list as the browser holds it: each paint replaces the row elements, so the indicator elements the
// paint path toggles are the ones the latest list HTML built.
function sessionListElement(elements) {
  const html = {value: ''};
  return new Proxy({scrollTop: 0, querySelector: () => null, querySelectorAll: () => []}, {
    get: (target, prop) => (prop === 'innerHTML' ? html.value : target[prop]),
    set: (target, prop, value) => {
      if (prop !== 'innerHTML') {
        target[prop] = value;
        return true;
      }
      html.value = value;
      for (const match of value.matchAll(/<span id="((?:subtree-)?unread-[^"]+)"[^>]*?class="([^"]*)"/g)) {
        elements.set(match[1], makeIndicator(match[2]));
      }
      return true;
    },
  });
}

function makeIndicator(className) {
  const names = new Set(className.split(/\s+/).filter(Boolean));
  return {classList: {
    contains: (name) => names.has(name),
    toggle: (name, force) => {
      if (force === undefined ? !names.has(name) : force) names.add(name); else names.delete(name);
    },
    add: (name) => names.add(name),
    remove: (name) => names.delete(name),
  }};
}

test('an opened session’s dot clears and stays clear while later list paints repaint its row', async () => {
  const page = buildOpenSocketPage({unread: ['session-b']});
  await page.load();
  assert.equal(page.dotShown('session-b'), true, 'the unread reply shows its dot');

  await page.switchTo('session-b');
  assert.deepEqual(page.readPosts, ['session-b']);
  assert.equal(page.server.flags.get('session-b'), false);
  assert.equal(page.dotShown('session-b'), false, 'opening the session clears its dot');

  await page.switchTo('session-c');
  await page.otherSessionChanged('session-a');
  assert.equal(page.socket(), page.context.WebSocket.instances[0], 'the switches kept the page’s one socket');
  assert.equal(page.dotShown('session-b'), false, 'a list repaint keeps the read session’s dot clear');
  await page.context.pollSessionStatus();
  assert.equal(page.dotShown('session-b'), false, 'the status poll keeps it clear');
});

test('a reply in the displayed session is read: its dot stays clear and the server flag clears', async () => {
  const page = buildOpenSocketPage({unread: []});
  await page.load();

  await page.reply('session-a');
  assert.deepEqual(page.readPosts, ['session-a'], 'the open view rendered the reply, so the page reads it');
  assert.equal(page.server.flags.get('session-a'), false);
  await page.context.pollSessionStatus();
  await page.otherSessionChanged('session-c');
  assert.equal(page.dotShown('session-a'), false, 'neither the poll nor a list repaint paints the dot');

  // A hidden tab has not shown the reply: the read waits until the tab is visible.
  page.setHidden(true);
  await page.reply('session-a');
  assert.deepEqual(page.readPosts, ['session-a']);
  assert.equal(page.server.flags.get('session-a'), true);
  page.setHidden(false);
  await flush();
  assert.deepEqual(page.readPosts, ['session-a', 'session-a']);
  assert.equal(page.server.flags.get('session-a'), false);
  assert.equal(page.dotShown('session-a'), false);
});

// The guard over the class: whatever channel moves an unread flag, each painted dot ends equal to the server's flag.
// The steps cover every channel that writes or paints the unread facts — the page's rows, a broadcast, the read, a
// reply in the displayed session, the row refresh and its merge, the Workspace badge repaint, the status poll, the
// full list reload and the inline delete — plus the two orders that let a stale value through: a row answer captured
// before the read, and a reply that lands while a switch leaves its session.
test('every unread channel leaves each painted dot equal to the server’s flag', async () => {
  const page = buildOpenSocketPage({unread: ['session-b']});
  const step = async (name, run) => {
    await run();
    await flush();
    for (const id of SESSION_IDS) {
      if (!page.elements.has('unread-' + id)) continue;
      assert.equal(page.dotShown(id), page.server.flags.get(id), `${name}: the dot of ${id}`);
    }
  };

  await step('page load', () => page.load());
  await step('a reply in another session', () => page.reply('session-c'));
  await step('a row answer captured before the read lands after it', async () => {
    page.held.rows = null;
    await page.otherSessionChanged('session-b');
    await page.switchTo('session-b');
    page.held.rows();
    delete page.held.rows;
  });
  await step('a list repaint from held rows', () => page.otherSessionChanged('session-a'));
  await step('the status poll', () => page.context.pollSessionStatus());
  await step('a reply in the displayed session', () => page.reply('session-b'));
  await step('a reply in the session a switch is leaving', async () => {
    page.held.bootstrap = null;
    const switched = page.context.switchSession('session-c');
    await page.reply('session-b');
    page.held.bootstrap();
    delete page.held.bootstrap;
    await switched;
  });
  assert.equal(page.server.flags.get('session-b'), true, 'the reply the switch left behind stays unread');
  await step('the full list reload', () => page.socket().emitMessage({type: 'session_group_changed'}));
  await step('the inline delete repaint', () => page.context.Sidebar.removeSessionFromRenderedList('session-a'));
  assert.ok(page.readDots.length >= 2, 'the clicks reported their dots');
  for (const read of page.readDots) {
    assert.equal(read.pageDot, read.serverUnread, `the dot the click on ${read.id} saw`);
  }
});
