// The live page capture that `scripts/switch-server.sh` runs after a release switch: one page view, one window size,
// one PNG of the size the release's window-size set names.
//
//   CHARLIEBOT_ACCESS_KEY=<key> node scripts/live-shot.mjs <base url> <view path> <width> <height> <out file>
//
// The key travels in the environment: a command line is readable from every process, and the server serves the
// page only to a caller that carries the key. The capture drives Chrome over the DevTools protocol on a pipe, the
// same way `dev/readme-screenshots/capture.sh` does, so it needs no npm package. The emulated viewport is the
// requested CSS size at device pixel ratio 1, so the PNG's pixel size must equal it; a capture that came out at
// another size fails here instead of passing a screenshot the release's check would refuse.
//
// Chrome resolves from LIVE_SHOT_CHROME, then `google-chrome`, `chromium-browser`, `chromium`, then the newest
// Playwright Chromium under ~/.cache/ms-playwright.
//
// Failure: the message goes to stderr and the process exits 1, so the switch's capture step reports the failure.

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
        if (message.id === undefined) continue;
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
  close() {
    for (const waiter of this.pending.values()) waiter.reject(new Error(`${waiter.method}: the browser closed`));
    this.pending.clear();
    this.writable.destroy();
  }
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

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

async function waitFor(page, expression, label, timeoutMs = 15000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await evaluate(page, expression)) return;
    await sleep(100);
  }
  throw new Error(`the page never reached: ${label}`);
}

async function capture(browser, url) {
  const { targetId } = await browser.cdp.send('Target.createTarget', { url: 'about:blank' });
  const { sessionId } = await browser.cdp.send('Target.attachToTarget', { targetId, flatten: true });
  const page = { send: (method, params = {}) => browser.cdp.send(method, params, sessionId) };
  try {
    await page.send('Page.enable');
    await page.send('Network.enable');
    await page.send('Runtime.enable');
    await page.send('Network.setExtraHTTPHeaders', { headers: { Authorization: `Bearer ${key}` } });
    await page.send('Emulation.setDeviceMetricsOverride', {
      width,
      height,
      deviceScaleFactor: 1,
      mobile: width < 600,
    });
    // `/` is public, so this load needs no key. The page's own script reads the key from localStorage and mirrors it
    // into the cookie, which is how a browser that unlocked the page carries the key; the capture writes both the same
    // way, and every later request of the view then authenticates.
    await page.send('Page.navigate', { url: `${baseUrl}/` });
    await waitFor(page, 'document.readyState === "complete"', 'the server root');
    await evaluate(
      page,
      `localStorage.setItem('charliebot_access_key', ${JSON.stringify(key)});` +
        `document.cookie = 'charliebot_access_key=' + ${JSON.stringify(key)} + '; path=/; SameSite=Strict';`,
    );
    await page.send('Page.navigate', { url });
    await waitFor(page, 'document.readyState === "complete"', 'the loaded page');
    await sleep(1000);
    const { data } = await page.send('Page.captureScreenshot', { format: 'png' });
    return Buffer.from(data, 'base64');
  } finally {
    await browser.cdp.send('Target.closeTarget', { targetId });
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
