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
