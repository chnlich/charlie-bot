// Tests of the TLS front's record sizes against the sizes Go's crypto/tls picks. The live front is Tailscale Serve, Go's
// crypto/tls, so the bench front writes records in the same sizes; a test reads them from a TCP relay that parses each
// TLS record header between the front and a client, without decrypting anything.

import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { once } from 'node:events';
import fs from 'node:fs';
import http from 'node:http';
import http2 from 'node:http2';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { test } from 'node:test';
import tls from 'node:tls';

import { startFront } from './front.mjs';

const TLS_APPLICATION_DATA = 23;
// The relay reads a TLS record's length field, which TLS 1.3 fills with the payload plus its 16-byte tag and the
// content-type byte. Go's crypto/tls picks the payload: one TCP MSS estimate (1208 bytes less the record's overheads)
// times the record's number in the connection's sequence, up to 16 KB.
const TLS13_RECORD_OVERHEAD = 17;
const GO_RECORD_PAYLOAD = 1186;
const GO_RECORD_MAX_PAYLOAD = 16384;

function goRecordLength(number) {
  return Math.min(GO_RECORD_PAYLOAD * number, GO_RECORD_MAX_PAYLOAD) + TLS13_RECORD_OVERHEAD;
}

function certificate(dir) {
  const key = path.join(dir, 'key.pem');
  const cert = path.join(dir, 'cert.pem');
  execFileSync(
    'openssl',
    [
      'req', '-x509', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:prime256v1', '-nodes',
      '-keyout', key, '-out', cert, '-days', '2', '-subj', '/CN=localhost',
      '-addext', 'subjectAltName=DNS:localhost,IP:127.0.0.1',
    ],
    { stdio: 'ignore' },
  );
  return { key: fs.readFileSync(key), cert: fs.readFileSync(cert) };
}

// A loopback upstream that answers a request with `bodyBytes`, and answers an upgrade with a 101 and then the body once
// the answer has gone out on its own write.
async function startUpstream(bodyBytes) {
  const upgraded = new Set();
  const server = http.createServer((req, res) => {
    res.writeHead(200, { 'content-type': 'application/octet-stream' });
    res.end(Buffer.alloc(bodyBytes, 0x61));
  });
  server.on('upgrade', (req, socket) => {
    // An upgraded socket leaves the server's own tracking, so close() must end it here.
    upgraded.add(socket);
    socket.on('close', () => upgraded.delete(socket));
    socket.write('HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n');
    setTimeout(() => socket.write(Buffer.alloc(bodyBytes, 0x62)), 50);
    socket.on('error', () => {});
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  return {
    port: server.address().port,
    close: async () => {
      for (const socket of upgraded) {
        socket.destroy();
      }
      await new Promise((resolve) => server.close(resolve));
    },
  };
}

// A relay in front of the front that records the payload length of every TLS application-data record the front writes.
async function recordRelay(targetPort) {
  const lengths = [];
  const server = net.createServer((client) => {
    const upstream = net.connect({ host: '127.0.0.1', port: targetPort });
    let head = Buffer.alloc(0);
    upstream.on('data', (chunk) => {
      head = Buffer.concat([head, chunk]);
      for (;;) {
        if (head.length < 5) {
          break;
        }
        const length = head.readUInt16BE(3);
        if (head.length < 5 + length) {
          break;
        }
        if (head[0] === TLS_APPLICATION_DATA) {
          lengths.push(length);
        }
        head = head.subarray(5 + length);
      }
      client.write(chunk);
    });
    client.on('data', (chunk) => upstream.write(chunk));
    const close = () => {
      client.destroy();
      upstream.destroy();
    };
    client.on('error', close);
    client.on('close', () => upstream.destroy());
    upstream.on('error', close);
    upstream.on('close', () => client.destroy());
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  return {
    port: server.address().port,
    lengths,
    close: () => new Promise((resolve) => server.close(resolve)),
  };
}

async function withFront(bodyBytes, body) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'pn-front-'));
  const tls = certificate(dir);
  const upstream = await startUpstream(bodyBytes);
  const front = await startFront({ key: tls.key, cert: tls.cert, target: { host: '127.0.0.1', port: upstream.port } });
  const relay = await recordRelay(front.port);
  try {
    return await body({ upstream, relay });
  } finally {
    await relay.close();
    await front.close();
    await upstream.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
}

// The data records of the body: the payload records a body of `bodyBytes` must take once the small control frames are
// left out. Go's growth from one TCP MSS estimate is arithmetic: n × 1186 bytes, up to 16 KB.
function bodyRecords(lengths, minimum) {
  return lengths.filter((length) => length >= minimum);
}

test('an HTTP/2 response carries the record sizes Go crypto/tls picks', { timeout: 60000 }, async () => {
  await withFront(30000, async ({ relay }) => {
    const client = http2.connect(`https://localhost:${relay.port}`, { rejectUnauthorized: false });
    try {
      const bytes = await new Promise((resolve, reject) => {
        const req = client.request({ ':path': '/' });
        let total = 0;
        req.on('data', (chunk) => {
          total += chunk.length;
        });
        req.on('end', () => resolve(total));
        req.on('error', reject);
        req.end();
      });
      assert.equal(bytes, 30000);
    } finally {
      client.close();
    }
    // The body's records grow as Go's do. The first one can also carry the response's small header frame, which the
    // transport writes in the same turn.
    const records = bodyRecords(relay.lengths, 1000);
    assert.ok(records.length >= 4, `only ${records.length} body records: ${records.join(', ')}`);
    const expected = [1, 2, 3, 4].map(goRecordLength);
    records.slice(0, expected.length).forEach((length, index) => {
      assert.ok(
        Math.abs(length - expected[index]) <= 120,
        `body record ${index + 1} is ${length} bytes, not about ${expected[index]}: ${records.join(', ')}`,
      );
    });
  });
});

test('an upgraded socket carries the record sizes Go crypto/tls picks', { timeout: 60000 }, async () => {
  await withFront(12000, async ({ relay }) => {
    const client = tls.connect({ host: '127.0.0.1', port: relay.port, rejectUnauthorized: false, servername: 'localhost' });
    try {
      await once(client, 'secureConnect');
      client.write(
        'GET /ws HTTP/1.1\r\n' +
          'Host: localhost\r\n' +
          'Connection: Upgrade\r\n' +
          'Upgrade: websocket\r\n' +
          'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n' +
          'Sec-WebSocket-Version: 13\r\n\r\n',
      );
      // The 101 answer first, then the upgraded stream's bytes.
      let body = Buffer.alloc(0);
      let answered = false;
      await new Promise((resolve, reject) => {
        const onData = (chunk) => {
          if (!answered) {
            body = Buffer.concat([body, chunk]);
            const end = body.indexOf('\r\n\r\n');
            if (end < 0) {
              return;
            }
            const head = body.subarray(0, end).toString('latin1');
            if (!head.startsWith('HTTP/1.1 101')) {
              reject(new Error(`the front answered ${JSON.stringify(head.split('\r\n')[0])}`));
              return;
            }
            answered = true;
            body = body.subarray(end + 4);
          } else {
            body = Buffer.concat([body, chunk]);
          }
          if (body.length >= 12000) {
            client.off('data', onData);
            resolve();
          }
        };
        client.on('data', onData);
        client.on('error', reject);
      });
      assert.equal(body.length, 12000);
    } finally {
      client.destroy();
    }
    // The 101 answer is a small record of its own; the body grows from the second record's size.
    const records = bodyRecords(relay.lengths, 1000);
    assert.ok(records.length >= 4, `only ${records.length} body records: ${records.join(', ')}`);
    const expected = [2, 3, 4].map(goRecordLength);
    records.slice(0, expected.length).forEach((length, index) => {
      assert.ok(
        Math.abs(length - expected[index]) <= 120,
        `body record ${index + 1} is ${length} bytes, not about ${expected[index]}: ${records.join(', ')}`,
      );
    });
  });
});
