// The live page capture that `scripts/live-capture.sh` runs for each View line and window size: one page view, one
// window size, one PNG of the size the release's window-size set names.
//
//   CHARLIEBOT_ACCESS_KEY=<key> node scripts/live-shot.mjs <base url> <view path> <width> <height> <out file>
//
// The key travels in the environment: a command line is readable from every process, and the server serves the
// page only to a caller that carries the key. The key reaches the CharlieBot origin only. A script that runs before
// any page script of a document sets the key in that origin's localStorage, where the page's own script reads it,
// and the capture adds it as the Authorization header of each request to that origin. Other origins get no key, and
// no cookie is written: a cookie reaches every port on the host.
//
// The capture loads the view itself, with the key in place, so its first document is the view's. A document loaded
// before the key is set opens the app's own access-key prompt (`initAuth` in web/static/js/config.js), and a capture
// that photographed it would show a login card for the app. The capture never waits on an earlier document.
//
// It drives Chrome over the DevTools protocol on a pipe, the same way `dev/readme-screenshots/capture.sh` does, so it
// needs no npm package. The emulated viewport is the requested CSS size at device pixel ratio 1, so the PNG's pixel
// size must equal it; a capture that came out at another size fails here instead of passing a screenshot the release's
// check would refuse.
//
// A capture is a picture of the app only when the rendered page shows so: the view must answer 200, the app shell must
// be present, and the page must show neither the access-key login page nor the app's own access-key prompt. Any other
// page fails the capture, and no PNG is written.
//
// Chrome resolves from LIVE_SHOT_CHROME, then the newest Playwright Chromium under ~/.cache/ms-playwright,
// then `chromium`, `chromium-browser` and `google-chrome`.
//
// Failure: the message goes to stderr and the process exits 1, so the capture stage reports the failure.

import { spawn, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const [baseUrl, view, widthArg, heightArg, outFile] = process.argv.slice(2);
const width = Number(widthArg);
const height = Number(heightArg);
const key = process.env.CHARLIEBOT_ACCESS_KEY || '';
if (!baseUrl || !view || !Number.isInteger(width) || !Number.isInteger(height) || !outFile) {
  console.error('usage: live-shot.mjs <base url> <view path> <width> <height> <out file>');
  process.exit(2);
}
if (!key) {
  console.error('live-shot: CHARLIEBOT_ACCESS_KEY is not set');
  process.exit(1);
}

function chromeBinary() {
  const candidates = [];
  if (process.env.LIVE_SHOT_CHROME) {
    candidates.push(process.env.LIVE_SHOT_CHROME);
  } else {
    for (const name of ['google-chrome', 'chromium-browser', 'chromium']) {
      const found = spawnSync('sh', ['-c', `command -v ${name}`], { encoding: 'utf8' });
      if (found.status === 0 && found.stdout.trim()) candidates.push(found.stdout.trim());
    }
    const cache = path.join(os.homedir(), '.cache', 'ms-playwright');
    if (fs.existsSync(cache)) {
      for (const entry of fs.readdirSync(cache).filter((name) => name.startsWith('chromium-')).sort()) {
        for (const layout of ['chrome-linux', 'chrome-linux64']) {
          candidates.push(path.join(cache, entry, layout, 'chrome'));
        }
      }
    }
  }
  for (const candidate of candidates.reverse()) {
    if (fs.existsSync(candidate) && fs.statSync(candidate).mode & 0o111) {
      const version = spawnSync(candidate, ['--version'], { encoding: 'utf8' });
      if (version.status === 0) return candidate;
    }
  }
  throw new Error('no Chrome binary: set LIVE_SHOT_CHROME to one');
}

// The DevTools protocol over the pipe: one JSON message per NUL-terminated frame in each direction.
class Cdp {
  constructor(writable, readable) {
    this.nextId = 1;
    this.pending = new Map();
    this.listeners = new Map();
    this.buffer = '';
    this.writable = writable;
    readable.setEncoding('utf8');
    readable.on('data', (chunk) => {
      this.buffer += chunk;
      for (;;) {
        const end = this.buffer.indexOf('\0');
        if (end < 0) return;
        const message = JSON.parse(this.buffer.slice(0, end));
        this.buffer = this.buffer.slice(end + 1);
        if (message.id === undefined) {
          this.listeners.get(message.method)?.(message);
          continue;
        }
        const waiter = this.pending.get(message.id);
        this.pending.delete(message.id);
        if (message.error) waiter.reject(new Error(`${waiter.method}: ${message.error.message}`));
        else waiter.resolve(message.result ?? {});
      }
    });
  }
  send(method, params = {}, sessionId = undefined) {
    const id = this.nextId++;
    const message = sessionId ? { id, method, params, sessionId } : { id, method, params };
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject, method });
      this.writable.write(`${JSON.stringify(message)}\0`);
    });
  }
  // One listener per event method; it receives each event message of that method, with its sessionId.
  on(method, listener) {
    this.listeners.set(method, listener);
  }
  close() {
    for (const waiter of this.pending.values()) waiter.reject(new Error(`${waiter.method}: the browser closed`));
    this.pending.clear();
    this.writable.destroy();
  }
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function drainPausedRequests(pending) {
  let timeout;
  try {
    const drained = await Promise.race([
      Promise.all(pending).then(() => true),
      new Promise((resolve) => {
        timeout = setTimeout(() => resolve(false), 5000);
      }),
    ]);
    if (!drained) throw new Error('a paused request did not continue before the page closed');
  } finally {
    clearTimeout(timeout);
  }
}

async function launch(binary) {
  const profile = fs.mkdtempSync(path.join(os.tmpdir(), 'live-shot-'));
  const child = spawn(binary, [
    '--headless=new', '--no-sandbox', '--disable-gpu', '--hide-scrollbars', '--no-first-run',
    '--no-default-browser-check', '--disable-background-networking', '--disable-component-update', '--disable-sync',
    '--metrics-recording-only', '--password-store=basic', '--use-mock-keychain',
    `--user-data-dir=${profile}`, '--remote-debugging-pipe', 'about:blank',
  ], { detached: true, stdio: ['ignore', 'ignore', 'ignore', 'pipe', 'pipe'] });
  child.stdio[4].on('error', () => {});
  const exited = new Promise((resolve) => {
    child.once('exit', resolve);
    child.once('error', resolve);
  });
  const cdp = new Cdp(child.stdio[3], child.stdio[4]);
  const failed = new Promise((_, reject) => {
    child.once('exit', (code, signal) => reject(new Error(`the browser exited (${code ?? signal}) before its pipe answered`)));
  });
  failed.catch(() => {});
  await Promise.race([cdp.send('Browser.getVersion'), failed]);
  return {
    cdp,
    async close() {
      try {
        process.kill(-child.pid, 'SIGKILL');
      } catch (error) {
        if (error.code !== 'ESRCH') throw error;
      }
      await exited;
      cdp.close();
      fs.rmSync(profile, { recursive: true, force: true });
    },
  };
}

async function evaluate(page, expression) {
  const result = await page.send('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true });
  if (result.exceptionDetails) throw new Error(`page script failed: ${result.exceptionDetails.text}`);
  return result.result.value;
}

// Sets the Authorization header of one paused request. The key goes only to the base URL's origin: the page's own
// Authorization header is dropped from every request, and the base URL's origin gets the key.
async function answerPausedRequest(page, paused, origin) {
  const headers = Object.entries(paused.request.headers)
    .filter(([name]) => name.toLowerCase() !== 'authorization')
    .map(([name, value]) => ({ name, value }));
  if (new URL(paused.request.url).origin === origin) headers.push({ name: 'Authorization', value: `Bearer ${key}` });
  await page.send('Fetch.continueRequest', { requestId: paused.requestId, headers });
}

// The script that sets the key in the CharlieBot origin's localStorage before a document of that origin runs its own
// scripts. Chrome runs it in every frame of the page, so the origin check keeps the key out of every other origin.
function seedAccessKey(origin) {
  return `if (location.origin === ${JSON.stringify(origin)}) ` +
    `localStorage.setItem('charliebot_access_key', ${JSON.stringify(key)});`;
}

// The facts of the rendered page that decide whether it is the app: the access-key login page's form, and the app's
// own access-key prompt when it is showing.
async function renderedPageFacts(page) {
  return evaluate(page, `(() => {
    const prompt = document.getElementById('auth-overlay');
    return {
      appShell: document.getElementById('auth-overlay') !== null &&
        document.getElementById('sidebar') !== null && document.getElementById('messages') !== null,
      loginForm: document.querySelector('input#k[type="password"]') !== null,
      accessPrompt: prompt !== null && getComputedStyle(prompt).display !== 'none',
    };
  })()`);
}

// Refuses a capture that is not the app: the view must answer 200, and the rendered page must show neither the
// access-key login page nor the app's access-key prompt.
function refuseNonAppPage(status, facts) {
  if (facts.loginForm) throw new Error('the page is the access-key login page, not the app');
  if (status !== 200) throw new Error(`the view answered ${status ?? 'with no status'}, not 200`);
  if (!facts.appShell) throw new Error('the page is not the CharlieBot app');
  if (facts.accessPrompt) throw new Error("the app's access-key prompt covers the page, so the key was not accepted");
}

async function capture(browser, url) {
  const { targetId } = await browser.cdp.send('Target.createTarget', { url: 'about:blank' });
  const { sessionId } = await browser.cdp.send('Target.attachToTarget', { targetId, flatten: true });
  const page = { send: (method, params = {}) => browser.cdp.send(method, params, sessionId) };
  const origin = new URL(baseUrl).origin;
  const loaded = new Set();
  const statuses = new Map();
  const pendingInterceptions = new Set();
  let capturing = true;
  let interceptFailure = null;
  browser.cdp.on('Fetch.requestPaused', (message) => {
    if (message.sessionId !== sessionId || !capturing) return;
    const pending = answerPausedRequest(page, message.params, origin).catch((error) => {
      interceptFailure ??= error;
      console.error(`live-shot: a paused request was not answered: ${error.message}`);
    });
    pendingInterceptions.add(pending);
    pending.finally(() => pendingInterceptions.delete(pending));
  });
  browser.cdp.on('Page.lifecycleEvent', (message) => {
    if (message.sessionId === sessionId && message.params.name === 'load') loaded.add(message.params.loaderId);
  });
  browser.cdp.on('Network.responseReceived', (message) => {
    if (message.sessionId === sessionId && message.params.type === 'Document') {
      statuses.set(message.params.loaderId, message.params.response.status);
    }
  });
  try {
    await page.send('Page.enable');
    await page.send('Page.setLifecycleEventsEnabled', { enabled: true });
    await page.send('Runtime.enable');
    await page.send('Network.enable');
    await page.send('Fetch.enable', { patterns: [{ urlPattern: '*' }] });
    await page.send('Emulation.setDeviceMetricsOverride', {
      width,
      height,
      deviceScaleFactor: 1,
      mobile: width < 600,
    });
    await page.send('Page.addScriptToEvaluateOnNewDocument', { source: seedAccessKey(origin) });
    const { loaderId, errorText } = await page.send('Page.navigate', { url });
    if (errorText) throw new Error(`the navigation failed: ${errorText}`);
    const deadline = Date.now() + 15000;
    while (!loaded.has(loaderId)) {
      if (Date.now() > deadline) throw new Error('the page never finished loading');
      await sleep(50);
    }
    await sleep(1000);
    refuseNonAppPage(statuses.get(loaderId), await renderedPageFacts(page));
    const { data } = await page.send('Page.captureScreenshot', { format: 'png' });
    return Buffer.from(data, 'base64');
  } finally {
    capturing = false;
    await drainPausedRequests(pendingInterceptions);
    await browser.cdp.send('Target.closeTarget', { targetId });
    if (interceptFailure) throw interceptFailure;
  }
}

/// The IHDR width and height of a PNG buffer.
function pngSize(buffer) {
  const signature = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
  if (!buffer.subarray(0, 8).equals(signature) || buffer.subarray(12, 16).toString('latin1') !== 'IHDR') {
    throw new Error('the browser returned no PNG capture');
  }
  return { width: buffer.readUInt32BE(16), height: buffer.readUInt32BE(20) };
}

const browser = await launch(chromeBinary());
try {
  const png = await capture(browser, `${baseUrl}${view}`);
  const size = pngSize(png);
  if (size.width !== width || size.height !== height) {
    throw new Error(`the capture is ${size.width}×${size.height}, not the requested ${width}×${height} at device pixel ratio 1`);
  }
  fs.mkdirSync(path.dirname(outFile), { recursive: true });
  fs.writeFileSync(outFile, png);
} catch (error) {
  console.error(`live-shot: ${view} at ${width}×${height}: ${error.message}`);
  process.exitCode = 1;
} finally {
  await browser.close();
}
