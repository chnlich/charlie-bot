// Chrome for the bench: a headless Chromium that speaks the DevTools protocol over a pipe (fd 3 and fd 4), so the bench
// needs no WebSocket client and no npm package. Every browser connection goes through the shaping proxy.

import { spawn } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

// The Chrome binary the bench drives, named by POOR_NETWORK_CHROME. run.sh resolves it before the trial starts, because
// the trial replaces HOME for its own tree. The report prints the path, so a run names its browser.
export function chromeBinary() {
  const binary = process.env.POOR_NETWORK_CHROME;
  if (!binary) {
    throw new Error('POOR_NETWORK_CHROME must name a Chrome binary');
  }
  if (!fs.existsSync(binary)) {
    throw new Error(`POOR_NETWORK_CHROME names a missing file: ${binary}`);
  }
  return binary;
}

const CHROME_FLAGS = [
  '--headless=new',
  '--no-sandbox',
  '--disable-gpu',
  '--no-first-run',
  '--no-default-browser-check',
  '--disable-background-networking',
  '--disable-component-update',
  '--disable-sync',
  '--disable-default-apps',
  '--disable-domain-reliability',
  '--disable-client-side-phishing-detection',
  '--safebrowsing-disable-auto-update',
  '--metrics-recording-only',
  '--password-store=basic',
  '--use-mock-keychain',
  '--disable-features=Translate,OptimizationHints,MediaRouter,AutofillServerCommunication,CertificateTransparencyComponentUpdater',
  // The loopback bypass would send the bench's own host past the proxy, so it is cleared.
  '--proxy-bypass-list=<-loopback>',
  '--ignore-certificate-errors',
  '--window-size=1440,900',
];

// The DevTools protocol over the pipe: one JSON message per NUL-terminated frame in each direction.
class CdpConnection {
  #writable;
  #nextId = 1;
  #pending = new Map();
  #listeners = new Set();
  #buffer = '';

  constructor(writable, readable) {
    this.#writable = writable;
    readable.setEncoding('utf8');
    readable.on('data', (chunk) => this.#onData(chunk));
  }

  #onData(chunk) {
    this.#buffer += chunk;
    for (;;) {
      const end = this.#buffer.indexOf('\0');
      if (end < 0) {
        return;
      }
      const message = JSON.parse(this.#buffer.slice(0, end));
      this.#buffer = this.#buffer.slice(end + 1);
      if (message.id !== undefined) {
        const waiter = this.#pending.get(message.id);
        this.#pending.delete(message.id);
        if (message.error) {
          waiter.reject(new Error(`${waiter.method}: ${message.error.message}`));
        } else {
          waiter.resolve(message.result ?? {});
        }
      } else {
        for (const listener of this.#listeners) {
          listener(message);
        }
      }
    }
  }

  send(method, params = {}, sessionId = undefined) {
    const id = this.#nextId++;
    const message = sessionId ? { id, method, params, sessionId } : { id, method, params };
    return new Promise((resolve, reject) => {
      this.#pending.set(id, { resolve, reject, method });
      this.#writable.write(`${JSON.stringify(message)}\0`);
    });
  }

  onMessage(listener) {
    this.#listeners.add(listener);
  }
}

// A page target: its session id and the DevTools calls the bench makes on it.
export class Page {
  constructor(cdp, sessionId, targetId) {
    this.cdp = cdp;
    this.sessionId = sessionId;
    this.targetId = targetId;
  }

  send(method, params = {}) {
    return this.cdp.send(method, params, this.sessionId);
  }

  async evaluate(expression) {
    const result = await this.send('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true });
    if (result.exceptionDetails) {
      throw new Error(`page script failed: ${result.exceptionDetails.text}`);
    }
    return result.result.value;
  }
}

export class Browser {
  #child;
  #cdp;
  #exited;

  constructor(child, cdp, exited) {
    this.#child = child;
    this.#cdp = cdp;
    this.#exited = exited;
  }

  // Opens a page target. Its network and page events go to the listener that the page's owner registers.
  async newPage() {
    const { targetId } = await this.#cdp.send('Target.createTarget', { url: 'about:blank' });
    const { sessionId } = await this.#cdp.send('Target.attachToTarget', { targetId, flatten: true });
    const page = new Page(this.#cdp, sessionId, targetId);
    await page.send('Page.enable');
    await page.send('Network.enable');
    await page.send('Runtime.enable');
    return page;
  }

  // Shows one page tab and hides the others, the way the window does for a background tab.
  async activate(page) {
    await this.#cdp.send('Target.activateTarget', { targetId: page.targetId });
  }

  onMessage(listener) {
    this.#cdp.onMessage(listener);
  }

  send(method, params = {}) {
    return this.#cdp.send(method, params);
  }

  // Stops Chrome and every process in its group. The bench calls it on every path, including a failed run.
  async close() {
    if (this.#child.exitCode === null && this.#child.signalCode === null) {
      try {
        process.kill(-this.#child.pid, 'SIGTERM');
      } catch (error) {
        if (error.code !== 'ESRCH') {
          throw error;
        }
      }
      const timer = setTimeout(() => {
        try {
          process.kill(-this.#child.pid, 'SIGKILL');
        } catch (error) {
          if (error.code !== 'ESRCH') {
            throw error;
          }
        }
      }, 3000);
      await this.#exited;
      clearTimeout(timer);
    }
    // A killed group can leave a child of Chrome behind; the group kill above reaches every process Chrome forked.
    try {
      process.kill(-this.#child.pid, 'SIGKILL');
    } catch (error) {
      if (error.code !== 'ESRCH') {
        throw error;
      }
    }
  }
}

// Starts Chrome with its proxy setting and returns once its DevTools pipe answers.
export async function launchBrowser({ binary, proxyPort, userDataDir, logPath }) {
  fs.mkdirSync(userDataDir, { recursive: true });
  const log = fs.openSync(logPath, 'a');
  const args = [
    ...CHROME_FLAGS,
    `--user-data-dir=${userDataDir}`,
    `--proxy-server=http://127.0.0.1:${proxyPort}`,
    '--remote-debugging-pipe',
    'about:blank',
  ];
  const child = spawn(binary, args, {
    detached: true,
    stdio: ['ignore', log, log, 'pipe', 'pipe'],
  });
  const exited = new Promise((resolve) => child.once('exit', resolve));
  const failed = new Promise((_, reject) => {
    child.once('error', reject);
    child.once('exit', (code, signal) => reject(new Error(`Chrome exited (${code ?? signal}) before its DevTools pipe answered`)));
  });
  // The race below handles a launch failure; an exit after launch is handled by Browser.close, not by this promise.
  failed.catch(() => {});
  const cdp = new CdpConnection(child.stdio[3], child.stdio[4]);
  const browser = new Browser(child, cdp, exited);
  const { product } = await Promise.race([browser.send('Browser.getVersion'), failed]);
  browser.version = product;
  return browser;
}
