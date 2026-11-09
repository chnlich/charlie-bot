// The put-back's live check, beside the bench it shares modules with: a message sent while the page is
// offline, a tab closed before the send lands, and the reopened page putting the text back into the
// composer once the first network first screen arrives — then one resend, and the server holds exactly
// one copy. Plan 2's trade-off 1 (放回草稿), exercised against a trial server.
//
//   scripts/release/trial.sh --serve -- node dev/poor-network/putback.mjs
//
// The trial exports CHARLIEBOT_SERVE_URL, CHARLIEBOT_SERVE_AUTH_HEADER and POOR_NETWORK_CHROME (run.sh
// resolves the browser before the trial starts). Exit 0 proves the case; a failed assert ends the probe
// with the step that broke.

import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';

import { ServerApi } from './api.mjs';
import { launchBrowser } from './chrome.mjs';
import { Tabs, WARM_PROBE_INIT, waitFor, waitForWarm } from './tabs.mjs';

const PENDING_SEND_KEY_PREFIX = 'charliebot-pending-send-';
// The put-back lands on the page-current event, which the catch-up's end raises; the store's answer
// rides the first network first screen. Neither arrives while the page is offline.
const PUTBACK_TIMEOUT_MS = 60000;
const RESEND_TIMEOUT_MS = 60000;

function env(name) {
  const value = process.env[name];
  if (!value) throw new Error(`${name} must be set (run under scripts/release/trial.sh --serve)`);
  return value;
}

function assert(condition, message) {
  if (!condition) throw new Error(`put-back failed: ${message}`);
}

// The page's own network stack: `offline` drops every request the page and its worker make, the way a
// dead link does, without a proxy in the path.
async function setOffline(tab, offline) {
  await tab.page.send('Network.enable', {});
  await tab.page.send('Network.emulateNetworkConditions', {
    offline,
    latency: 0,
    downloadThroughput: -1,
    uploadThroughput: -1,
  });
}

const api = new ServerApi({ url: env('CHARLIEBOT_SERVE_URL'), authHeader: env('CHARLIEBOT_SERVE_AUTH_HEADER') });
const tag = crypto.randomBytes(4).toString('hex');
const text = `pn-putback-${tag} 帮我看一下昨天那个 job 为什么挂了`;

const session = await api.createSession(`pn-putback-${tag}`, 'charlie-code-release-trial');
const origin = api.base;
const dir = fs.mkdtempSync(`${os.tmpdir()}/pn-putback-`);
const browser = await launchBrowser({
  binary: env('POOR_NETWORK_CHROME'), userDataDir: dir, logPath: `${dir}/chrome.log`,
});
const messagesWithTag = async () => {
  const page = await api.json('GET', `/api/sessions/${session.id}/bootstrap`);
  return (page.messages ?? []).filter((message) => String(message.content ?? '').includes(text)).length;
};

try {
  const tabs = new Tabs({ browser, assets: null, accessKey: api.accessKey, origin, cdn: 'link' });
  const tab = await tabs.open('putback', WARM_PROBE_INIT);
  await tabs.navigate(tab, `/?session=${session.id}`);
  await tab.ready();
  await tabs.waitIdle(60000);
  await waitForWarm(tab, 60000);

  // Offline: the composer's send fails and the pending-send record carries the text and its request id.
  await setOffline(tab, true);
  await tab.evaluate(`
    const input = document.getElementById('msg-input');
    input.value = ${JSON.stringify(text)};
    document.getElementById('send-btn').click();
  `);
  const pending = await waitFor(
    () => tab.evaluate(`localStorage.getItem(${JSON.stringify(PENDING_SEND_KEY_PREFIX + session.id)})`),
    10000,
    'the pending-send record',
  );
  assert(JSON.parse(pending).content === text, 'the pending-send record carries the sent text');

  // The tab closes before the send lands; the server has received nothing.
  await tabs.close(tab);
  assert((await messagesWithTag()) === 0, 'the offline send reached the server');

  // The reopen runs offline, so the stored shell and the stored copy serve it; the link returns and the
  // first network first screen puts the text back into the composer.
  const second = await tabs.open('putback-reopen', WARM_PROBE_INIT);
  await tabs.navigate(second, `/?session=${session.id}`);
  await second.ready();
  await setOffline(second, false);
  const composer = await waitFor(
    () => second.evaluate(`document.getElementById('msg-input')?.value ?? null`),
    PUTBACK_TIMEOUT_MS,
    'the reopened composer to take the unsent text back',
  );
  assert(composer === text, `the composer holds the put-back text, got ${JSON.stringify(composer)}`);
  assert((await messagesWithTag()) === 0, 'the put-back sent the message by itself');

  // One resend, reusing the recorded request id: the server ends with exactly one copy.
  await second.evaluate(`document.getElementById('send-btn').click()`);
  const cleared = await waitFor(
    () => second.evaluate(`localStorage.getItem(${JSON.stringify(PENDING_SEND_KEY_PREFIX + session.id)}) === null`),
    RESEND_TIMEOUT_MS,
    'the resend to land and clear the pending-send record',
  );
  assert(cleared, 'the pending-send record cleared');
  assert((await messagesWithTag()) === 1, 'the server holds exactly one copy of the message');
  const composerAfter = await second.evaluate(`document.getElementById('msg-input').value`);
  assert(composerAfter === '', 'the composer emptied after the resend');

  console.log(`put-back ok: session ${session.id}, tag ${tag}`);
} finally {
  await browser.shutdown();
  fs.rmSync(dir, { recursive: true, force: true });
}
