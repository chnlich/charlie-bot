// Tests of the shaping proxy against a loopback server, in real time. Each transfer is sized so one test takes seconds.
// The tolerance: a transfer may run 15% longer than its link time plus 300 ms, which covers the connect and the timer
// resolution; it may not run shorter than 90% of its link time.

import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import net from 'node:net';
import readline from 'node:readline';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

import { LINK } from './profile.mjs';
import { ShapingProxy } from './proxy.mjs';

const BPS = LINK.bytesPerSecond;
const RELAY = fileURLToPath(new URL('./relay.mjs', import.meta.url));

// A loopback server. A tunnel names itself with its first line, `ID <label>`, and the server counts the bytes after it
// per label. A `GET <n>` request in the data gets n bytes back on the same connection.
async function startTarget() {
  const received = new Map();
  const server = net.createServer((socket) => {
    let label = null;
    let head = '';
    socket.on('data', (chunk) => {
      let body = chunk;
      if (label === null) {
        head += chunk.toString('latin1');
        const end = head.indexOf('\n');
        if (end < 0) {
          return;
        }
        label = head.slice(3, end).trim();
        body = Buffer.from(head.slice(end + 1), 'latin1');
        head = '';
        received.set(label, 0);
      }
      received.set(label, received.get(label) + body.length);
      const match = /GET (\d+)/.exec(body.toString('latin1'));
      if (match) {
        socket.write(Buffer.alloc(Number(match[1]), 0x78));
      }
    });
    socket.on('error', () => {});
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  return {
    port: server.address().port,
    received,
    close: () => new Promise((resolve) => server.close(resolve)),
  };
}

// Opens a tunnel through the proxy to the target and names it. Resolves with the socket once the proxy answers 200.
function tunnel(proxyPort, targetPort, label) {
  return tunnelTo(proxyPort, `127.0.0.1:${targetPort}`, label);
}

// Opens a tunnel through the proxy to `authority` (`host:port`) and names it. Resolves with the socket once the proxy
// answers 200; rejects when the proxy refuses the authority.
function tunnelTo(proxyPort, authority, label) {
  return new Promise((resolve, reject) => {
    const socket = net.connect(proxyPort, '127.0.0.1');
    socket.once('error', reject);
    socket.write(`CONNECT ${authority} HTTP/1.1\r\nHost: ${authority}\r\n\r\n`);
    let head = Buffer.alloc(0);
    const onData = (chunk) => {
      head = Buffer.concat([head, chunk]);
      const end = head.indexOf('\r\n\r\n');
      if (end < 0) {
        return;
      }
      socket.off('data', onData);
      if (!head.subarray(0, end).toString('latin1').startsWith('HTTP/1.1 200')) {
        reject(new Error('the proxy refused the tunnel'));
        return;
      }
      socket.write(`ID ${label}\n`);
      resolve(socket);
    };
    socket.on('data', onData);
  });
}

// Resolves with the time when the target has counted `bytes` for `label`.
function untilReceived(target, label, bytes) {
  return new Promise((resolve) => {
    const check = () => {
      if ((target.received.get(label) ?? 0) >= bytes) {
        resolve(performance.now());
        return;
      }
      setTimeout(check, 10);
    };
    check();
  });
}

function assertWithin(actualMs, expectedMs, label) {
  const low = expectedMs * 0.9;
  const high = expectedMs * 1.15 + 300;
  assert.ok(
    actualMs >= low && actualMs <= high,
    `${label}: ${actualMs.toFixed(0)} ms outside [${low.toFixed(0)}, ${high.toFixed(0)}]`,
  );
}

// Starts the relay entry against a target. Resolves with the child and the port of the address the relay printed.
async function startRelay(targetPort) {
  const child = spawn(process.execPath, [RELAY, '--target', `http://127.0.0.1:${targetPort}`], {
    stdio: ['ignore', 'pipe', 'inherit'],
  });
  const lines = readline.createInterface({ input: child.stdout });
  const [first] = await once(lines, 'line');
  lines.close();
  return { child, port: Number(new URL(first).port) };
}

test('a route reaches its own server over the same link; other authorities are refused', { timeout: 60000 }, async () => {
  const target = await startTarget();
  const asset = await startTarget();
  const proxy = new ShapingProxy({
    target: { host: '127.0.0.1', port: target.port },
    routes: { 'cdn.example:443': { host: '127.0.0.1', port: asset.port } },
  });
  const proxyPort = await proxy.start();
  try {
    const socket = await tunnelTo(proxyPort, 'cdn.example:443', 'asset');
    socket.write('GET 10');
    await untilReceived(asset, 'asset', 6);
    assert.equal(target.received.has('asset'), false, 'the route does not reach the bench server');
    socket.destroy();
    await assert.rejects(tunnelTo(proxyPort, 'other.example:443', 'refused'));
  } finally {
    await proxy.stop();
    await target.close();
    await asset.close();
  }
});

test('a scheduled reset window resets the open tunnels when it starts', { timeout: 60000 }, async () => {
  const target = await startTarget();
  const proxy = new ShapingProxy({ target: { host: '127.0.0.1', port: target.port } });
  const proxyPort = await proxy.start();
  try {
    const socket = await tunnel(proxyPort, target.port, 'scheduled');
    const reset = new Promise((resolve) => socket.once('error', resolve));
    const scheduledAt = performance.now();
    proxy.schedule([{ at: 300, seconds: 1, mode: 'reset' }]);
    const error = await reset;
    assert.equal(error.code, 'ECONNRESET');
    assert.ok(performance.now() - scheduledAt >= 280, 'the reset waits for its start');
  } finally {
    await proxy.stop();
    await target.close();
  }
});

test('the relay entry carries a tunnel over the bench link and stops on SIGTERM', { timeout: 60000 }, async () => {
  const target = await startTarget();
  const { child, port } = await startRelay(target.port);
  const exited = once(child, 'exit');
  try {
    const bytes = 25000;
    const socket = await tunnel(port, target.port, 'relay');
    await new Promise((resolve) => setTimeout(resolve, 100));
    const start = performance.now();
    socket.write(Buffer.alloc(bytes, 0x61));
    const done = await untilReceived(target, 'relay', bytes);
    assertWithin(done - start, (bytes / BPS) * 1000 + LINK.roundTripMs / 2, '25 KB through the relay');
    socket.destroy();
    child.kill('SIGTERM');
    const [code] = await exited;
    assert.equal(code, 0);
    await assert.rejects(tunnel(port, target.port, 'late'));
  } finally {
    child.kill('SIGKILL');
    await target.close();
  }
});

test('the relay entry refuses a target without an http port and prints no address', { timeout: 60000 }, async () => {
  const child = spawn(process.execPath, [RELAY, '--target', 'http://127.0.0.1'], { stdio: 'pipe' });
  let stdout = '';
  let stderr = '';
  child.stdout.on('data', (chunk) => {
    stdout += chunk;
  });
  child.stderr.on('data', (chunk) => {
    stderr += chunk;
  });
  const [code] = await once(child, 'close');
  assert.notEqual(code, 0);
  assert.notEqual(stderr, '');
  assert.equal(stdout, '');
});

// A proxy on the bench link, torn down on every path.
async function withProxy(body) {
  const target = await startTarget();
  const proxy = new ShapingProxy({ target: { host: '127.0.0.1', port: target.port } });
  const port = await proxy.start();
  try {
    return await body({ proxy, port, target });
  } finally {
    await proxy.stop();
    await target.close();
  }
}

// What a tunnel attempt did within `ms`: its answer, its refusal, or nothing at all.
function attemptWithin(attempt, ms) {
  return Promise.race([attempt, new Promise((resolve) => setTimeout(() => resolve('silent'), ms))]);
}

test('a blackout drops a connection attempt: it is never answered, not even at the restore', { timeout: 30000 }, async () => {
  await withProxy(async ({ proxy, port, target }) => {
    proxy.blackout({ seconds: 1, mode: 'hold' });
    const attempt = tunnel(port, target.port, 'dropped').then(() => 'answered', () => 'refused');
    assert.equal(await attemptWithin(attempt, 1600), 'silent');
    // The restore does not answer the lost attempt: a person's retry starts a new one.
    assert.equal(await attemptWithin(attempt, 400), 'silent');
    const socket = await tunnel(port, target.port, 'after-restore');
    socket.destroy();
  });
});

test('a hold blackout keeps an established tunnel and delivers its bytes at the restore', { timeout: 30000 }, async () => {
  await withProxy(async ({ proxy, port, target }) => {
    const socket = await tunnel(port, target.port, 'held');
    proxy.blackout({ seconds: 1, mode: 'hold' });
    const started = performance.now();
    socket.write(Buffer.alloc(4000, 0x61));
    const doneAt = await untilReceived(target, 'held', 4000);
    assert.ok(doneAt - started >= 1000, `delivered after ${(doneAt - started).toFixed(0)} ms, before the restore`);
    socket.destroy();
  });
});

test('the link reports the hold a step window would run into, and none for a clear window', { timeout: 30000 }, async () => {
  const target = await startTarget();
  // A short period keeps the check quick: the first spike starts half a period after the proxy starts.
  const proxy = new ShapingProxy({
    target: { host: '127.0.0.1', port: target.port },
    link: { ...LINK, spikeEverySeconds: 4, spikeSeconds: 1 },
  });
  await proxy.start();
  try {
    const started = performance.now();
    // A window that ends before the first spike is clear.
    assert.equal(proxy.holdEndWithin(started, 500), 0);
    // A window that reaches the first spike waits for the spike's end.
    const end = proxy.holdEndWithin(started, 4000);
    assert.ok(end > started + 2500, `the window over the first spike ends ${(end - started).toFixed(0)} ms out`);
    // The window past that spike is clear again.
    assert.equal(proxy.holdEndWithin(end + 100, 500), 0);
    // A window that starts clear and reaches the next spike waits for that spike too: spikes start at 2 s and 6 s.
    assert.equal(proxy.holdEndWithin(started + 3500, 500), 0);
    const next = proxy.holdEndWithin(started + 3500, 3000);
    assert.ok(next > started + 6500, `the window reaching the second spike ends ${(next - started).toFixed(0)} ms out`);
    // A blackout is a hold like a spike.
    proxy.blackout({ seconds: 1, mode: 'hold' });
    assert.ok(proxy.holdEndWithin(performance.now(), 100) > 0, 'a blackout window reports no hold');
  } finally {
    await proxy.stop();
    await target.close();
  }
});

test('a reset blackout resets an established tunnel', { timeout: 30000 }, async () => {
  await withProxy(async ({ proxy, port, target }) => {
    const socket = await tunnel(port, target.port, 'reset');
    socket.on('error', () => {});
    // `events.once` rejects on an error, and a reset is the event under test: wait for the close itself.
    const closed = new Promise((resolve) => socket.once('close', resolve));
    proxy.blackout({ seconds: 1, mode: 'reset' });
    await closed;
  });
});
