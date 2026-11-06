// The browser's tabs for the bench: each tab records its network events (request bytes, WebSocket frames, handshakes) and
// answers the page's CDN requests from the local assets. A tab also drives the page: it clicks a session row, sends a
// message, and reports when a text shows in the chat, all with timestamps in wall-clock milliseconds, which the bench
// shares with the server and the proxy.

import { INTERCEPT_PATTERNS } from './assets.mjs';

const POLL_MS = 25;
// The localStorage key the web client reads its access key from (web/static/js/config.js).
const ACCESS_KEY_NAME = 'charliebot_access_key';

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
  // requestId -> {url, startAt, finishAt, bytes, status, failed}, in the order the browser sent them.
  requests = new Map();
  // Every WebSocket frame the page received: {at, bytes, type, text, requestId}.
  frames = [];
  // Every WebSocket the page opened: {at, url, requestId}.
  sockets = [];
  // Every WebSocket handshake answer: {at, status, requestId}.
  handshakes = [];

  constructor(name, page) {
    this.name = name;
    this.page = page;
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

  // The session row's anchor. Clicking it runs the page's own switch, as a person's click does.
  async clickSession(sessionId) {
    const clicked = await this.evaluate(`(() => {
      const anchor = Array.from(document.querySelectorAll('#session-list a.session-row'))
        .find((link) => link.href.includes('session=${sessionId}&') || link.href.endsWith('session=${sessionId}'));
      if (!anchor) return null;
      const at = Date.now();
      anchor.click();
      return at;
    })()`);
    if (clicked === null) {
      throw new Error(`no sidebar row for session ${sessionId} on ${this.name}`);
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

  // HTTP statuses of every request whose URL matches `pattern` and that finished in [from, to], in send order.
  statusesOf(pattern, from, to) {
    return [...this.requests.values()]
      .filter((request) => pattern.test(request.url) && request.finishAt !== null && request.finishAt >= from && request.finishAt <= to)
      .map((request) => request.status);
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

  // Routes one CDP event of this tab.
  handle(message, assets) {
    const at = Date.now();
    switch (message.method) {
      case 'Network.requestWillBeSent': {
        const { requestId, request } = message.params;
        this.requests.set(requestId, { url: request.url, startAt: at, finishAt: null, bytes: 0, status: null, failed: null });
        return;
      }
      case 'Network.responseReceived': {
        const request = this.requests.get(message.params.requestId);
        if (request) request.status = message.params.response.status;
        return;
      }
      case 'Network.loadingFinished': {
        const request = this.requests.get(message.params.requestId);
        if (request) {
          request.finishAt = at;
          request.bytes = message.params.encodedDataLength;
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
      case 'Network.webSocketHandshakeResponseReceived':
        this.handshakes.push({ at, status: message.params.response.status, requestId: message.params.requestId });
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
      default:
    }
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
  #tabs = [];

  // `origin` is the TLS front's https origin; the page and its socket live there.
  constructor({ browser, assets, accessKey, origin }) {
    this.#browser = browser;
    this.#assets = assets;
    this.#accessKey = accessKey;
    this.#origin = origin;
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

  // Opens a tab, with the access key on every request and the intercepted CDN patterns.
  async open(name) {
    const page = await this.#browser.newPage();
    const tab = new Tab(name, page);
    this.#tabs.push(tab);
    await page.send('Fetch.enable', {
      patterns: INTERCEPT_PATTERNS.map((urlPattern) => ({ urlPattern, requestStage: 'Request' })),
    });
    // The page takes its access key from localStorage, where the login page leaves it: the fetch wrapper sends it as a
    // Bearer header and every WebSocket URL carries it as a token. The script runs before each document's own scripts.
    await page.send('Page.addScriptToEvaluateOnNewDocument', {
      source: `localStorage.setItem(${JSON.stringify(ACCESS_KEY_NAME)}, ${JSON.stringify(this.#accessKey)});`,
    });
    return tab;
  }

  async navigate(tab, path) {
    await tab.page.send('Page.navigate', { url: `${this.#origin}${path}` });
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
