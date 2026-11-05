// Tests of the shaping proxy against a loopback server, in real time. Each transfer is sized so one test takes seconds.
// The tolerance: a transfer may run 15% longer than its link time plus 300 ms, which covers the connect and the timer
// resolution; it may not run shorter than 90% of its link time.

import assert from 'node:assert/strict';
import net from 'node:net';
import { test } from 'node:test';

import { ShapingProxy } from './proxy.mjs';
import { LINK } from './profile.mjs';

const BPS = LINK.bytesPerSecond;
const CONNECT_RESPONSE_BYTES = Buffer.byteLength('HTTP/1.1 200 Connection Established\r\n\r\n');

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
  return new Promise((resolve, reject) => {
    const socket = net.connect(proxyPort, '127.0.0.1');
    socket.once('error', reject);
    socket.write(`CONNECT 127.0.0.1:${targetPort} HTTP/1.1\r\nHost: 127.0.0.1:${targetPort}\r\n\r\n`);
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

async function withProxy(fn) {
  const target = await startTarget();
  const proxy = new ShapingProxy({ target: { host: '127.0.0.1', port: target.port } });
  const proxyPort = await proxy.start();
  try {
    await fn({ target, proxy, proxyPort });
  } finally {
    await proxy.stop();
    await target.close();
  }
}

test('a transfer of N bytes takes N / 12,500 s of link time, plus one round trip', async () => {
  await withProxy(async ({ target, proxyPort }) => {
    const bytes = 25000;
    const socket = await tunnel(proxyPort, target.port, 'solo');
    await new Promise((resolve) => setTimeout(resolve, 100));
    const start = performance.now();
    socket.write(Buffer.alloc(bytes, 0x61));
    const done = await untilReceived(target, 'solo', bytes);
    assertWithin(done - start, (bytes / BPS) * 1000 + LINK.roundTripMs / 2, '25 KB upstream');
    socket.destroy();
  });
});

test('two concurrent transfers share one budget per direction', async () => {
  await withProxy(async ({ target, proxyPort }) => {
    const bytes = 25000;
    const first = await tunnel(proxyPort, target.port, 'first');
    const second = await tunnel(proxyPort, target.port, 'second');
    await new Promise((resolve) => setTimeout(resolve, 100));
    const start = performance.now();
    first.write(Buffer.alloc(bytes, 0x61));
    second.write(Buffer.alloc(bytes, 0x62));
    const [firstDone, secondDone] = await Promise.all([
      untilReceived(target, 'first', bytes),
      untilReceived(target, 'second', bytes),
    ]);
    const earlier = Math.min(firstDone, secondDone) - start;
    const later = Math.max(firstDone, secondDone) - start;
    // Two budgets would finish both transfers after one transfer's time; one shared budget finishes the pair after two.
    assertWithin(later, ((2 * bytes) / BPS) * 1000 + LINK.roundTripMs / 2, 'the pair of transfers');
    assert.ok(
      earlier > ((bytes / BPS) * 1000) * 1.3,
      `the earlier transfer finished in ${earlier.toFixed(0)} ms, too soon for a shared budget`,
    );
    first.destroy();
    second.destroy();
  });
});

test('a held blackout delays the bytes and keeps the tunnel open', async () => {
  await withProxy(async ({ target, proxy, proxyPort }) => {
    const bytes = 12500;
    const socket = await tunnel(proxyPort, target.port, 'held');
    await new Promise((resolve) => setTimeout(resolve, 100));
    let closed = false;
    socket.on('close', () => {
      closed = true;
    });
    const holdSeconds = 2;
    const start = performance.now();
    socket.write(Buffer.alloc(bytes, 0x61));
    await new Promise((resolve) => setTimeout(resolve, 200));
    proxy.blackout({ seconds: holdSeconds, mode: 'hold' });
    const done = await untilReceived(target, 'held', bytes);
    assert.equal(closed, false, 'a held blackout must not close the tunnel');
    assert.ok(
      done - start >= (bytes / BPS + holdSeconds) * 1000 * 0.9,
      `held blackout finished in ${(done - start).toFixed(0)} ms, faster than the hold allows`,
    );
    socket.destroy();
  });
});

test('a reset blackout resets the open tunnels, and the link carries new tunnels after it', async () => {
  await withProxy(async ({ target, proxy, proxyPort }) => {
    const socket = await tunnel(proxyPort, target.port, 'before-reset');
    const reset = new Promise((resolve) => socket.once('error', resolve));
    proxy.blackout({ seconds: 1, mode: 'reset' });
    const error = await reset;
    assert.equal(error.code, 'ECONNRESET');

    const blackoutStart = performance.now();
    const second = await tunnel(proxyPort, target.port, 'after-reset');
    assert.ok(performance.now() - blackoutStart >= 900, 'the new tunnel waits for the blackout to end');
    const answer = new Promise((resolve) => {
      let total = 0;
      second.on('data', (chunk) => {
        total += chunk.length;
        if (total >= 10) {
          resolve(total);
        }
      });
    });
    second.write('GET 10');
    assert.equal(await answer, 10);
    second.destroy();
  });
});

test('the byte counters match the bytes that crossed the link', async () => {
  await withProxy(async ({ target, proxy, proxyPort }) => {
    const sent = 6000;
    const answered = 4000;
    // `tunnel` consumed the CONNECT answer, which the proxy counted as received; the body is what the test counts here.
    const socket = await tunnel(proxyPort, target.port, 'counted');
    let clientBytes = 0;
    const got = new Promise((resolve) => {
      socket.on('data', (chunk) => {
        clientBytes += chunk.length;
        if (clientBytes >= answered) {
          resolve();
        }
      });
    });
    const upload = Buffer.alloc(sent, 0x61);
    socket.write(upload);
    await untilReceived(target, 'counted', sent);
    // The request follows the upload as its own write, so the target answers once, after every upload byte arrived.
    const request = `GET ${answered}`;
    socket.write(request);
    await got;
    await untilReceived(target, 'counted', sent + request.length);
    const counters = proxy.counters();
    // The tunnel's first line, `ID counted`, is upload too.
    const idLine = Buffer.byteLength('ID counted\n');
    assert.equal(counters.sent, sent + request.length + idLine);
    assert.equal(counters.received, clientBytes + CONNECT_RESPONSE_BYTES);
    socket.destroy();
  });
});
