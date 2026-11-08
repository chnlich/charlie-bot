// The browser's tabs for the bench: each tab records its network events (request bytes, WebSocket frames, handshakes) and
// answers the page's CDN requests from the local assets. A tab also drives the page: it clicks a session row, sends a
// message, and reports when a text shows in the chat, all with timestamps in wall-clock milliseconds, which the bench
// shares with the server and the proxy.

import { INTERCEPT_PATTERNS } from './assets.mjs';

const POLL_MS = 25;
// The localStorage key the web client reads its access key from (web/static/js/config.js).
const ACCESS_KEY_NAME = 'charliebot_access_key';
// The wait for a row that a group's "Show all" fetches: the full listing crosses the link beside the background load.
const ROW_PAINT_TIMEOUT_MS = 180000;

// A page expression for the session's row anchor in the sidebar list, or undefined when the list has no such row.
function sessionRowExpression(sessionId) {
  return `Array.from(document.querySelectorAll('#session-list a.session-row'))
    .find((link) => link.href.includes('session=${sessionId}&') || link.href.endsWith('session=${sessionId}'))`;
}

// The sidebar spinner probe the page runs while the bench measures a turn: it
// counts the full sidebar repaints — the paints that replace the session
// list's rows wholesale — and the spinner restarts, a visible running
// spinner's rotation jumping against its steady turn, which is the stutter a
// repaint gives every recreated spinner. The restart check samples every
// running spinner's rotation angle on each frame and flags a move that leaves
// the angle the steady turn predicts by more than 45 degrees (a frame's
// sampling lag stays under 10 degrees). The counts ride in
// window.__pnSidebarProbe for sidebarProbeCounts().
const SIDEBAR_PROBE_SOURCE = `(() => {
  if (window.__pnSidebarProbe) return;
  const probe = { repaints: 0, restarts: 0 };
  window.__pnSidebarProbe = probe;
  const hasRow = (nodes) => [...nodes].some((node) =>
    node.nodeType === 1 && (node.matches('a.session-row') || node.querySelector('a.session-row')));
  const list = document.getElementById('session-list');
  if (list) {
    new MutationObserver((records) => {
      for (const record of records) {
        if (hasRow(record.removedNodes) && hasRow(record.addedNodes)) probe.repaints += 1;
      }
    }).observe(list, { childList: true, subtree: true });
  }
  const phase = new Map();
  const angleOf = (el) => {
    const matrix = getComputedStyle(el).transform.match(/matrix\\(([^)]+)\\)/);
    if (!matrix) return null;
    const parts = matrix[1].split(',').map(Number);
    const angle = (Math.atan2(parts[1], parts[0]) * 180) / Math.PI;
    return angle < 0 ? angle + 360 : angle;
  };
  const sample = () => {
    if (!window.__pnSidebarProbe) return;
    const now = Date.now();
    const seen = new Set();
    document.querySelectorAll('svg[id^="spinner-"]').forEach((el) => {
      if (el.classList.contains('hidden')) return;
      seen.add(el.id);
      const angle = angleOf(el);
      if (angle === null) return;
      const prev = phase.get(el.id);
      if (prev) {
        const expected = (prev.angle + (now - prev.at) * 0.36) % 360;
        const drift = Math.abs(((angle - expected) % 360 + 540) % 360 - 180);
        if (drift > 45) probe.restarts += 1;
      }
      phase.set(el.id, { at: now, angle });
    });
    for (const id of [...phase.keys()]) {
      if (!seen.has(id)) phase.delete(id);
    }
    requestAnimationFrame(sample);
  };
  requestAnimationFrame(sample);
})()`;

export function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

// Polls `check` until it returns a truthy value, and returns that value; null when `timeoutMs` passes first.
export async function pollFor(check, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const value = await check();
    if (value) {
      return value;
    }
    if (Date.now() > deadline) {
      return null;
    }
    await sleep(POLL_MS);
  }
}

// Polls `check` until it returns a truthy value, and returns that value. Fails after `timeoutMs`.
export async function waitFor(check, timeoutMs, label) {
  const value = await pollFor(check, timeoutMs);
  if (value === null) {
    throw new Error(`timed out after ${timeoutMs} ms waiting for ${label}`);
  }
  return value;
}

export class Tab {
  name;
  page;
  // requestId -> {url, startAt, finishAt, bytes, status, wireStatus, failed}, in the order the browser sent them.
  // `status` is the answer the page got. `wireStatus` is the one the server sent: a revalidated answer reaches the page
  // as the cached 200, while its wire status is 304.
  requests = new Map();
  // Every WebSocket frame the page received: {at, bytes, type, text, requestId}.
  frames = [];
  // Every WebSocket the page opened: {at, url, requestId}.
  sockets = [];
  // Every WebSocket handshake answer: {at, status, requestId}.
  handshakes = [];
  // Every WebSocket handshake the browser started sending: {at, requestId}. Creation precedes it when the request
  // waits in the browser's connection pool.
  handshakeSends = [];
  // Every WebSocket close the browser saw: {at, requestId, error}.
  socketCloses = [];
  // The page's console calls and uncaught exceptions, oldest first, capped so a chatty page cannot grow them
  // without bound: what the page said about its own sockets and failures.
  consoleLines = [];
  pageErrors = [];
  // The URLs whose 200 bodies the tab keeps, as `request.body` text: a request's body reads after it finishes. A
  // request that finished before keepBodies has no `body`.
  #bodyPattern = null;
  #bodyReads = new Set();

  constructor(name, page) {
    this.name = name;
    this.page = page;
  }

  // Keeps the body of every later 200 answer whose URL matches `pattern`.
  keepBodies(pattern) {
    this.#bodyPattern = pattern;
  }

  // Waits until every kept body that a finished request owes has been read, also the ones whose requests finish while
  // this waits: a read that starts after the loop took its snapshot is awaited on the next pass.
  async bodiesRead() {
    while (this.#bodyReads.size > 0) {
      await Promise.all([...this.#bodyReads]);
    }
  }

  // The DOM. Each call runs in the page.
  evaluate(expression) {
    return this.page.evaluate(expression);
  }

  async ready() {
    await waitFor(
      () => this.evaluate("document.readyState === 'complete' && !!document.getElementById('msg-input')").catch(() => false),
      240000,
      `${this.name} to load the chat`,
    );
  }

  // Installs the sidebar spinner probe (SIDEBAR_PROBE_SOURCE above) on this tab. Call on the visible tab before the
  // window whose counts the scenario reports.
  async installSidebarProbe() {
    await this.evaluate(SIDEBAR_PROBE_SOURCE);
  }

  // The probe's counts since its install: {repaints, restarts}, or null when the probe is not installed.
  async sidebarProbeCounts() {
    return this.evaluate(
      'window.__pnSidebarProbe ? {repaints: window.__pnSidebarProbe.repaints, restarts: window.__pnSidebarProbe.restarts} : null');
  }

  // Puts the session's sidebar row on screen, as a person does before clicking it. A group paints its first five roots
  // and keeps the others behind its "Show all" toggle (web/static/js/sidebar/groups.js). A kept-back row is either
  // hidden in the document, or absent when the page carries only the first screen's rows; in that case the toggle
  // fetches the full listing and repaints the list. A painted row needs nothing. Otherwise the bench clicks the toggle
  // of the session's group and waits for the row. When the row is not painted and the group shows no toggle to click,
  // the call fails. The toggle's fetch shares the link, so call this before a timed click. Returns whether it clicked
  // the toggle.
  async showRow(session) {
    const step = await this.evaluate(`(() => {
      const row = ${sessionRowExpression(session.id)};
      if (row && row.checkVisibility()) return 'painted';
      const toggle = Array.from(document.querySelectorAll('.session-group-limit-toggle')).find((button) =>
        button.dataset.sgroupLimitToggleKey === ${JSON.stringify(session.group)}
          && button.getAttribute('aria-expanded') === 'false' && button.checkVisibility());
      if (!toggle) return 'unreachable';
      toggle.click();
      return 'expanded';
    })()`);
    if (step === 'unreachable') {
      throw new Error(`no sidebar row for session ${session.id} on ${this.name}, and its group ` +
        `${JSON.stringify(session.group)} shows no Show all toggle`);
    }
    if (step === 'painted') {
      return false;
    }
    await waitFor(
      () => this.evaluate(`(() => { const row = ${sessionRowExpression(session.id)}; return !!row && row.checkVisibility(); })()`),
      ROW_PAINT_TIMEOUT_MS,
      `${this.name} to paint the row of session ${session.id} after its group's Show all`,
    );
    return true;
  }

  // Clicks the session's row, which runs the page's own switch as a person's click does. The row must be on screen:
  // showRow puts a row that its group keeps back there.
  async clickSession(sessionId) {
    const clicked = await this.evaluate(`(() => {
      const anchor = ${sessionRowExpression(sessionId)};
      if (!anchor || !anchor.checkVisibility()) return null;
      const at = Date.now();
      anchor.click();
      return at;
    })()`);
    if (clicked === null) {
      throw new Error(`no painted sidebar row for session ${sessionId} on ${this.name}`);
    }
    return clicked;
  }

  // Writes the text into the composer and clicks send, as a person does. Returns the click's wall-clock time.
  async sendText(text) {
    const at = await this.evaluate(`(() => {
      const input = document.getElementById('msg-input');
      const setter = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value').set;
      setter.call(input, ${JSON.stringify(text)});
      input.dispatchEvent(new Event('input', { bubbles: true }));
      const at = Date.now();
      document.getElementById('send-btn').click();
      return at;
    })()`);
    return at;
  }

  // Starts watching the chat for `marker`. The page records the wall-clock time the marker first shows in the chat.
  async watchChat(name, marker) {
    await this.evaluate(`(() => {
      window.__pnWatch = window.__pnWatch || {};
      const entry = { at: null };
      window.__pnWatch[${JSON.stringify(name)}] = entry;
      const check = () => {
        if (entry.at === null && (document.getElementById('messages')?.textContent || '').includes(${JSON.stringify(marker)})) {
          entry.at = Date.now();
        }
      };
      new MutationObserver(check).observe(document.body, { subtree: true, childList: true, characterData: true });
      check();
    })()`);
  }

  // Starts watching the page's own switch telemetry for `sessionId`: the switch's completed phase,
  // which the page dispatches when the first screen has rendered. The detection reads the phase
  // and not the chat's text, because the first screen renders the session's newest turn, and a
  // turn a reminder woke can sit between the switch's click and the text a watcher might seek.
  async watchSwitchPhase(name, sessionId) {
    await this.evaluate(`(() => {
      window.__pnWatch = window.__pnWatch || {};
      const entry = { at: null };
      window.__pnWatch[${JSON.stringify(name)}] = entry;
      document.addEventListener('charliebot:session-switch-phase', (ev) => {
        if (entry.at === null && ev.detail && ev.detail.phase === 'completed'
            && ev.detail.to_session === ${JSON.stringify(sessionId)}) {
          entry.at = Date.now();
        }
      });
    })()`);
  }

  // When the watched marker showed after `from`, and whether it did so only at the cap. A marker that has not shown
  // within `capMs` is recorded at `from + capMs`, so the time it reports is a lower bound.
  async shownBy(name, from, capMs) {
    const at = await pollFor(
      () => this.evaluate(`window.__pnWatch?.[${JSON.stringify(name)}]?.at ?? null`),
      capMs,
    );
    return at === null ? { at: from + capMs, capped: true } : { at, capped: false };
  }

  // Waits until the watched marker shows, and returns its wall-clock time.
  async whenShown(name, timeoutMs) {
    return waitFor(
      () => this.evaluate(`window.__pnWatch?.[${JSON.stringify(name)}]?.at ?? null`),
      timeoutMs,
      `${this.name} to show ${name}`,
    ).catch(async (caught) => {
      throw new Error(`${caught.message}; the tab since the start: ${await this.diagnose(0)}`);
    });
  }

  // Frames that arrived in [from, to] (wall-clock milliseconds), inclusive.
  framesBetween(from, to) {
    return this.frames.filter((frame) => frame.at >= from && frame.at <= to);
  }

  // The frames received in [from, to], counted and sized by type: what the window's socket bytes are made of.
  frameBreakdown(from, to) {
    const byType = {};
    for (const frame of this.framesBetween(from, to)) {
      const entry = byType[frame.type] ?? { count: 0, bytes: 0 };
      entry.count += 1;
      entry.bytes += frame.bytes;
      byType[frame.type] = entry;
    }
    return byType;
  }

  // What the tab has seen since `from`, for a timeout's message: the frame types, the sockets, and the chat's end.
  async diagnose(from) {
    const frameTypes = {};
    for (const frame of this.frames) {
      if (frame.at >= from) {
        frameTypes[frame.type] = (frameTypes[frame.type] ?? 0) + 1;
      }
    }
    const page = await this.evaluate(
      "({ href: location.href, chatEnd: (document.getElementById('messages')?.innerText ?? '').slice(-200) })",
    );
    const bootstraps = [...this.requests.values()]
      .filter((request) => request.url.includes('/bootstrap'))
      .map((request) => ({ finished: request.finishAt !== null, failed: request.failed }));
    return JSON.stringify({
      frameTypes,
      sockets: this.sockets.length,
      handshakes: this.handshakes.map((handshake) => handshake.status),
      bootstraps,
      page,
    });
  }

  // Bytes of every request whose URL matches `pattern` and that finished in [from, to].
  bytesOf(pattern, from, to) {
    let bytes = 0;
    for (const request of this.requests.values()) {
      if (pattern.test(request.url) && request.finishAt !== null && request.finishAt >= from && request.finishAt <= to) {
        bytes += request.bytes;
      }
    }
    return bytes;
  }

  // How often each request path went out in [from, to], for the requests whose URL matches
  // `pattern`. The path is the URL without its query, so a repeated download of one endpoint
  // shows as a count above one.
  requestPathCounts(pattern, from, to) {
    const counts = {};
    for (const request of this.requests.values()) {
      if (pattern.test(request.url) && request.startAt >= from && request.startAt <= to) {
        const path = new URL(request.url).pathname;
        counts[path] = (counts[path] ?? 0) + 1;
      }
    }
    return counts;
  }

  // Routes one CDP event of this tab.
  handle(message, assets) {
    const at = Date.now();
    switch (message.method) {
      case 'Network.requestWillBeSent': {
        const { requestId, request } = message.params;
        this.requests.set(requestId, {
          url: request.url,
          startAt: at,
          finishAt: null,
          bytes: 0,
          status: null,
          wireStatus: null,
          failed: null,
        });
        return;
      }
      case 'Network.responseReceived': {
        const request = this.requests.get(message.params.requestId);
        if (request) request.status = message.params.response.status;
        return;
      }
      case 'Network.responseReceivedExtraInfo': {
        const request = this.requests.get(message.params.requestId);
        if (request) request.wireStatus = message.params.statusCode;
        return;
      }
      case 'Network.loadingFinished': {
        const request = this.requests.get(message.params.requestId);
        if (request) {
          request.finishAt = at;
          request.bytes = message.params.encodedDataLength;
          if (request.status === 200 && this.#bodyPattern?.test(request.url)) {
            this.#readBody(message.params.requestId, request);
          }
        }
        return;
      }
      case 'Network.loadingFailed': {
        const request = this.requests.get(message.params.requestId);
        if (request) {
          request.failed = message.params.errorText;
        }
        return;
      }
      case 'Network.webSocketCreated':
        this.sockets.push({ at, url: message.params.url, requestId: message.params.requestId });
        return;
      case 'Network.webSocketWillSendHandshakeRequest':
        this.handshakeSends.push({ at, requestId: message.params.requestId });
        return;
      case 'Network.webSocketHandshakeResponseReceived':
        this.handshakes.push({ at, status: message.params.response.status, requestId: message.params.requestId });
        return;
      case 'Network.webSocketClosed':
        this.socketCloses.push({ at, requestId: message.params.requestId, error: null });
        return;
      case 'Network.webSocketFrameError':
        this.socketCloses.push({ at, requestId: message.params.requestId, error: message.params.errorMessage });
        return;
      case 'Network.webSocketFrameReceived': {
        const text = message.params.response.payloadData;
        let type = null;
        try {
          type = JSON.parse(text).type ?? null;
        } catch {
          type = null;
        }
        this.frames.push({ at, bytes: Buffer.byteLength(text), type, text, requestId: message.params.requestId });
        return;
      }
      case 'Fetch.requestPaused':
        this.answerAsset(message.params, assets);
        return;
      case 'Runtime.consoleAPICalled': {
        const text = message.params.args.map((arg) => arg.value ?? arg.description ?? arg.type).join(' ');
        this.consoleLines.push({ at, text: text.slice(0, 200) });
        if (this.consoleLines.length > 50) this.consoleLines.shift();
        return;
      }
      case 'Runtime.exceptionThrown': {
        const details = message.params.exceptionDetails;
        const text = `${details.text} ${details.exception?.description ?? details.url ?? ''}`.trim();
        this.pageErrors.push({ at, text: text.slice(0, 300) });
        return;
      }
      default:
    }
  }

  // Reads a finished request's body into `request.body`, which is null until the read ends. A read that fails leaves
  // the body null and its error in `request.bodyError`.
  #readBody(requestId, request) {
    request.body = null;
    const read = this.page
      .send('Network.getResponseBody', { requestId })
      .then(({ body, base64Encoded }) => {
        request.body = base64Encoded ? Buffer.from(body, 'base64').toString('utf8') : body;
      })
      .catch((caught) => {
        request.bodyError = caught.message;
      })
      .finally(() => this.#bodyReads.delete(read));
    this.#bodyReads.add(read);
  }

  // Serves a CDN request from the local assets, or fails it, so an unknown host never reaches the link.
  answerAsset(params, assets) {
    const asset = assets.get(params.request.url);
    if (asset) {
      this.page.send('Fetch.fulfillRequest', {
        requestId: params.requestId,
        responseCode: 200,
        responseHeaders: [
          { name: 'Content-Type', value: asset.type },
          { name: 'Access-Control-Allow-Origin', value: '*' },
          { name: 'Cache-Control', value: 'max-age=31536000' },
        ],
        body: asset.body.toString('base64'),
      }).catch(() => {});
    } else {
      this.page.send('Fetch.failRequest', { requestId: params.requestId, errorReason: 'AddressUnreachable' }).catch(() => {});
    }
  }
}

// Opens tabs on one browser. Events are routed to the tab whose page they came from.
export class Tabs {
  #browser;
  #assets;
  #accessKey;
  #origin;
  #cdn;
  #tabs = [];

  // `origin` is the TLS front's https origin; the page and its socket live there. `cdn` says where the CDN files come
  // from: `local` answers them in the browser from the assets, so they never cross the link; `link` lets the browser
  // fetch them, through the proxy, from the CDN stand-in.
  constructor({ browser, assets, accessKey, origin, cdn }) {
    if (cdn !== 'local' && cdn !== 'link') {
      throw new Error(`cdn must be local or link, got ${cdn}`);
    }
    this.#browser = browser;
    this.#assets = assets;
    this.#accessKey = accessKey;
    this.#origin = origin;
    this.#cdn = cdn;
    browser.onMessage((message) => {
      if (!message.sessionId) {
        return;
      }
      const tab = this.#tabs.find((candidate) => candidate.page.sessionId === message.sessionId);
      if (tab) {
        tab.handle(message, this.#assets);
      }
    });
  }

  get all() {
    return this.#tabs;
  }

  // Opens a tab, with the access key on every request and, for `local` CDN files, the intercepted CDN patterns.
  // `initScript`, when given, runs in each document of the tab before the page's own scripts.
  async open(name, initScript = null) {
    const page = await this.#browser.newPage();
    const tab = new Tab(name, page);
    this.#tabs.push(tab);
    if (this.#cdn === 'local') {
      await page.send('Fetch.enable', {
        patterns: INTERCEPT_PATTERNS.map((urlPattern) => ({ urlPattern, requestStage: 'Request' })),
      });
    }
    // The page takes its access key from localStorage, where the login page leaves it: the fetch wrapper sends it as a
    // Bearer header and every WebSocket URL carries it as a token. The script runs before each document's own scripts.
    await page.send('Page.addScriptToEvaluateOnNewDocument', {
      source: `localStorage.setItem(${JSON.stringify(ACCESS_KEY_NAME)}, ${JSON.stringify(this.#accessKey)});`,
    });
    if (initScript !== null) {
      await page.send('Page.addScriptToEvaluateOnNewDocument', { source: initScript });
    }
    return tab;
  }

  // Navigates the tab. Resolves with the navigation's answer: its errorText names a navigation that failed.
  async navigate(tab, path) {
    return tab.page.send('Page.navigate', { url: `${this.#origin}${path}` });
  }

  // Closes the tab's target: its sockets and requests end with it.
  async close(tab) {
    await this.#browser.send('Target.closeTarget', { targetId: tab.page.targetId });
    this.#tabs = this.#tabs.filter((candidate) => candidate !== tab);
  }

  // Shows `tab` and hides every other tab, as the window does for the tab a person looks at.
  async show(tab) {
    await this.#browser.activate(tab.page);
    for (const other of this.#tabs) {
      if (other !== tab) {
        const state = await other.evaluate('document.visibilityState');
        if (state !== 'hidden') {
          throw new Error(`${other.name} is ${state} while ${tab.name} is shown`);
        }
      }
    }
  }

  // Waits until every request that is in flight now has finished. The page's own polls keep starting and finishing
  // while it runs, and over the shaped link some are always in flight; a request that starts after this call does not
  // hold the settle, so the wait ends with the load that was under way when it began.
  async waitIdle(timeoutMs) {
    const pending = this.#tabs.flatMap((tab) =>
      [...tab.requests.entries()]
        .filter(([, request]) => request.finishAt === null && request.failed === null)
        .map(([requestId]) => ({ tab, requestId })),
    );
    await waitFor(
      () =>
        pending.every(({ tab, requestId }) => {
          const request = tab.requests.get(requestId);
          return request.finishAt !== null || request.failed !== null;
        }),
      timeoutMs,
      'the page to settle',
    ).catch((caught) => {
      const stuck = pending
        .filter(({ tab, requestId }) => {
          const request = tab.requests.get(requestId);
          return request.finishAt === null && request.failed === null;
        })
        .map(({ tab, requestId }) => `${tab.name} ${tab.requests.get(requestId).url.slice(0, 120)}`);
      throw new Error(`${caught.message}; still in flight: ${stuck.slice(0, 10).join(' | ')}`);
    });
  }
}
