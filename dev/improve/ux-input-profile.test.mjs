// Tests of the improve-ux input profile. The fake page follows the Chromium build the round pins: touch emulation sets
// a coarse pointer with no hover. Without it, a browser started with INPUT_BROWSER_ARGS reports a fine pointer that
// hovers, and a browser started without them reports neither. On a loaded page, turning touch emulation off leaves the
// page with neither pointer until the page loads again; before the first navigation the off call has no lasting effect.
// The fake keeps the viewport and the touch emulation that the Emulation calls set.
// The last tests drive the pinned Chrome itself; they need IMPROVE_BROWSER and fail without it. A test file's name ends
// in .test.mjs, so the bench-lock naming check reads it as a test.

import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import vm from 'node:vm';
import { test } from 'node:test';

import {
  applyInputProfile,
  checkInputProfile,
  INPUT_BROWSER_ARGS,
  InputProfileRefused,
  WINDOW_SIZES,
} from './ux-input-profile.mjs';

const DESKTOP = {
  '(hover: hover)': true,
  '(hover: none)': false,
  '(pointer: fine)': true,
  '(pointer: coarse)': false,
};
const PHONE = {
  '(hover: hover)': false,
  '(hover: none)': true,
  '(pointer: fine)': false,
  '(pointer: coarse)': true,
};
const NO_INPUT = {
  '(hover: hover)': false,
  '(hover: none)': true,
  '(pointer: fine)': false,
  '(pointer: coarse)': false,
};
const EXPECTED = new Map([
  ['1536x900', DESKTOP],
  ['1400x1000', DESKTOP],
  ['390x844', PHONE],
]);

const READ_ANSWERS = `({
  '(hover: hover)': matchMedia('(hover: hover)').matches,
  '(hover: none)': matchMedia('(hover: none)').matches,
  '(pointer: fine)': matchMedia('(pointer: fine)').matches,
  '(pointer: coarse)': matchMedia('(pointer: coarse)').matches,
})`;

const MEDIA_QUERY = /^\((hover|pointer): (\w+)\)$/;

// The answers of each input state. The state is the touch emulation first, then the launch arguments.
const ANSWERS = {
  touch: { hover: 'none', pointer: 'coarse' },
  mouse: { hover: 'hover', pointer: 'fine' },
  none: { hover: 'none', pointer: 'none' },
};

class FakePage {
  constructor({ launchArgs = [] } = {}) {
    this.launchedWithInputArgs = INPUT_BROWSER_ARGS.every((arg) => launchArgs.includes(arg));
    this.calls = [];
    this.viewport = null;
    this.touch = { enabled: false, maxTouchPoints: 1 };
    this.captures = 0;
    this.loaded = false;
    this.pointerReset = false;
    this.context = this.documentContext();
  }

  // A new document starts with the launch defaults and none of the old document's globals.
  loadDocument() {
    this.loaded = true;
    this.pointerReset = false;
    this.context = this.documentContext();
  }

  documentContext() {
    return vm.createContext({
      matchMedia: (query) => ({ matches: this.matches(query) }),
      document: { readyState: 'complete' },
    });
  }

  matches(query) {
    const [, name, value] = MEDIA_QUERY.exec(query);
    let state = 'none';
    if (this.touch.enabled) state = 'touch';
    else if (this.launchedWithInputArgs && !this.pointerReset) state = 'mouse';
    return ANSWERS[state][name] === value;
  }
}

// The CDP send of a fake page. Runtime.evaluate runs in the page's context, and its reply carries a JSON copy of
// the value.
function browser(page) {
  return async (method, params) => {
    page.calls.push(method);
    switch (method) {
      case 'Emulation.setDeviceMetricsOverride':
        page.viewport = { width: params.width, height: params.height, mobile: params.mobile };
        return {};
      case 'Emulation.setTouchEmulationEnabled':
        page.touch = { enabled: params.enabled, maxTouchPoints: params.maxTouchPoints };
        if (page.loaded && !params.enabled) page.pointerReset = true;
        return {};
      case 'Page.navigate':
      case 'Page.reload':
        page.loadDocument();
        return {};
      case 'Runtime.evaluate': {
        const value = vm.runInContext(params.expression, page.context);
        return { result: { value: JSON.parse(JSON.stringify(value)) } };
      }
      case 'Page.captureScreenshot':
        page.captures += 1;
        return { data: '' };
      default:
        throw new Error(`the fake page has no ${method}`);
    }
  };
}

const sizeKey = ({ width, height }) => `${width}x${height}`;
const DESKTOP_SIZE = WINDOW_SIZES.find((size) => !size.touch);
const PHONE_SIZE = WINDOW_SIZES.find((size) => size.touch);

// The driver's order for one capture: the profile, then the check on the loaded page, then the capture.
async function captureAt(send, size) {
  await applyInputProfile(send, size);
  await checkInputProfile(send, size);
  return send('Page.captureScreenshot', { format: 'png' });
}

test('each size profile gives the expected hover and pointer answers to matchMedia', async () => {
  for (const size of WINDOW_SIZES) {
    const page = new FakePage({ launchArgs: INPUT_BROWSER_ARGS });
    const send = browser(page);
    await applyInputProfile(send, size);
    const answers = await send('Runtime.evaluate', { expression: READ_ANSWERS, returnByValue: true });
    assert.deepEqual(answers.result.value, EXPECTED.get(sizeKey(size)), sizeKey(size));
  }
});

test('a size sets a viewport of its size, and touch emulation on only for the phone', async () => {
  for (const size of WINDOW_SIZES) {
    const page = new FakePage({ launchArgs: INPUT_BROWSER_ARGS });
    await applyInputProfile(browser(page), size);
    assert.deepEqual(page.viewport, { width: size.width, height: size.height, mobile: size.touch }, sizeKey(size));
    assert.equal(page.touch.enabled, size.touch, sizeKey(size));
  }
});

test('a profile applied after the phone leaves no touch emulation for a desktop size', async () => {
  const page = new FakePage({ launchArgs: INPUT_BROWSER_ARGS });
  const send = browser(page);
  await send('Page.navigate', { url: 'about:blank' });
  await applyInputProfile(send, PHONE_SIZE);
  await applyInputProfile(send, DESKTOP_SIZE);
  const answers = await send('Runtime.evaluate', { expression: READ_ANSWERS, returnByValue: true });
  assert.deepEqual(answers.result.value, DESKTOP);
  assert.equal(page.touch.enabled, false);
  assert.equal(page.viewport.width, DESKTOP_SIZE.width);
});

test('the check resolves to the readings when the page answers the profile', async () => {
  for (const size of WINDOW_SIZES) {
    const page = new FakePage({ launchArgs: INPUT_BROWSER_ARGS });
    const send = browser(page);
    await applyInputProfile(send, size);
    assert.deepEqual(await checkInputProfile(send, size), EXPECTED.get(sizeKey(size)), sizeKey(size));
  }
});

test('a desktop size whose browser lacks the input arguments is refused, naming size and readings', async () => {
  const page = new FakePage();
  const send = browser(page);
  await applyInputProfile(send, DESKTOP_SIZE);
  await assert.rejects(checkInputProfile(send, DESKTOP_SIZE), (error) => {
    assert.ok(error instanceof InputProfileRefused);
    assert.match(error.message, /1536x900/);
    assert.deepEqual(error.expected, DESKTOP);
    assert.deepEqual(error.actual, NO_INPUT);
    assert.match(error.message, /\(pointer: fine\) false/, 'the page reading is named');
    return true;
  });
});

test('the phone size takes its profile in a browser started without the input arguments', async () => {
  const page = new FakePage();
  const send = browser(page);
  await applyInputProfile(send, PHONE_SIZE);
  assert.deepEqual(await checkInputProfile(send, PHONE_SIZE), PHONE);
});

test('a driver captures nothing at a desktop size whose browser lacks the input arguments', async () => {
  const page = new FakePage();
  await assert.rejects(captureAt(browser(page), DESKTOP_SIZE), InputProfileRefused);
  assert.equal(page.captures, 0, 'no capture reaches the page');
});

test('a driver captures once at each size in a browser started with the input arguments', async () => {
  for (const size of WINDOW_SIZES) {
    const page = new FakePage({ launchArgs: INPUT_BROWSER_ARGS });
    await captureAt(browser(page), size);
    assert.equal(page.captures, 1, sizeKey(size));
  }
});

test('a size the profile does not name is refused before any Emulation call', async () => {
  const page = new FakePage({ launchArgs: INPUT_BROWSER_ARGS });
  const send = browser(page);
  await assert.rejects(applyInputProfile(send, { width: 800, height: 600 }), /800x600/);
  await assert.rejects(checkInputProfile(send, { width: 800, height: 600 }), /800x600/);
  assert.deepEqual(page.calls, []);
});

// The real-browser tests. Each one starts the pinned Chrome, which IMPROVE_BROWSER names, with INPUT_BROWSER_ARGS, and
// drives one order of the window sizes on one page in the driver's order: the first size's profile, the navigation,
// then each later size's profile followed by its check. Run them where the pinned build is installed:
//   IMPROVE_BROWSER=<pinned Chrome> node --test dev/improve/ux-input-profile.test.mjs

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const LOAD_WAIT_MS = 20000;

function requirePinnedBrowser() {
  const browserPath = process.env.IMPROVE_BROWSER;
  if (!browserPath || !fs.existsSync(browserPath)) {
    throw new Error(`IMPROVE_BROWSER must name the pinned Chrome; it names ${browserPath || 'nothing'}`);
  }
  return browserPath;
}

// Every order of ITEMS, each order a new array.
function orders(items) {
  if (items.length <= 1) return [items];
  return items.flatMap((item, index) => orders([...items.slice(0, index), ...items.slice(index + 1)])
    .map((rest) => [item, ...rest]));
}

// A CDP session over the browser's pipes. Each message ends with a NUL byte, as the driver's session reads them.
class PipeCdp {
  constructor(writable, readable) {
    this.nextId = 1;
    this.pending = new Map();
    this.listeners = new Map();
    this.buffer = '';
    this.writable = writable;
    readable.setEncoding('utf8');
    readable.on('data', (chunk) => this.receive(chunk));
  }

  receive(chunk) {
    this.buffer += chunk;
    for (let end = this.buffer.indexOf('\0'); end >= 0; end = this.buffer.indexOf('\0')) {
      const message = JSON.parse(this.buffer.slice(0, end));
      this.buffer = this.buffer.slice(end + 1);
      if (message.id === undefined) {
        const listener = this.listeners.get(message.method);
        if (listener) listener(message);
        continue;
      }
      const waiter = this.pending.get(message.id);
      this.pending.delete(message.id);
      if (message.error) waiter.reject(new Error(`${waiter.method}: ${message.error.message}`));
      else waiter.resolve(message.result || {});
    }
  }

  send(method, params = {}, sessionId = undefined) {
    const id = this.nextId;
    this.nextId += 1;
    const message = sessionId ? { id, method, params, sessionId } : { id, method, params };
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject, method });
      this.writable.write(`${JSON.stringify(message)}\0`);
    });
  }
}

// Starts the pinned Chrome and one page, and returns the page's send, a navigation to URL and a close.
async function openPage(browserPath, url) {
  const profile = fs.mkdtempSync(path.join(os.tmpdir(), 'ux-input-profile-'));
  const child = spawn(browserPath, [
    '--headless=new', '--no-sandbox', '--disable-gpu', '--no-first-run', '--no-default-browser-check',
    ...INPUT_BROWSER_ARGS, `--user-data-dir=${profile}`, '--remote-debugging-pipe', 'about:blank',
  ], { detached: true, stdio: ['ignore', 'ignore', 'ignore', 'pipe', 'pipe'] });
  const exited = new Promise((resolve) => {
    child.once('exit', resolve);
    child.once('error', resolve);
  });
  const cdp = new PipeCdp(child.stdio[3], child.stdio[4]);
  await cdp.send('Browser.getVersion');
  const { targetId } = await cdp.send('Target.createTarget', { url: 'about:blank' });
  const { sessionId } = await cdp.send('Target.attachToTarget', { targetId, flatten: true });
  const send = (method, params = {}) => cdp.send(method, params, sessionId);
  const loaded = new Set();
  cdp.listeners.set('Page.lifecycleEvent', (message) => {
    if (message.sessionId === sessionId && message.params.name === 'load') loaded.add(message.params.loaderId);
  });
  await send('Page.enable');
  await send('Page.setLifecycleEventsEnabled', { enabled: true });
  await send('Runtime.enable');
  return {
    send,
    async navigate() {
      const navigation = await send('Page.navigate', { url });
      if (navigation.errorText) throw new Error(`navigation to ${url} failed: ${navigation.errorText}`);
      const deadline = Date.now() + LOAD_WAIT_MS;
      while (!loaded.has(navigation.loaderId)) {
        if (Date.now() > deadline) throw new Error(`${url} did not finish loading`);
        await sleep(50);
      }
    },
    async close() {
      try { process.kill(-child.pid, 'SIGTERM'); } catch (error) { if (error.code !== 'ESRCH') throw error; }
      await Promise.race([exited, sleep(5000)]);
      try { process.kill(-child.pid, 'SIGKILL'); } catch (error) { if (error.code !== 'ESRCH') throw error; }
      cdp.writable.destroy();
      fs.rmSync(profile, { recursive: true, force: true });
    },
  };
}

for (const order of orders(WINDOW_SIZES)) {
  test(`the input profile takes effect at each size in the order ${order.map(sizeKey).join(' -> ')}`, {
    timeout: 120000,
  }, async () => {
    const browserPath = requirePinnedBrowser();
    const server = http.createServer((request, response) => {
      response.writeHead(200, { 'content-type': 'text/html' });
      response.end('<!doctype html><title>input profile</title><p>input profile</p>');
    });
    await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
    const page = await openPage(browserPath, `http://127.0.0.1:${server.address().port}/`);
    try {
      await applyInputProfile(page.send, order[0]);
      await page.navigate();
      for (const [index, size] of order.entries()) {
        if (index > 0) await applyInputProfile(page.send, size);
        assert.deepEqual(await checkInputProfile(page.send, size), EXPECTED.get(sizeKey(size)), sizeKey(size));
      }
    } finally {
      await page.close();
      server.close();
    }
  });
}
