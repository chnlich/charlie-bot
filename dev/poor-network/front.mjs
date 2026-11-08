// The TLS front: the bench's stand-in for Tailscale Serve. It terminates TLS for the browser, serves HTTP/2 and
// HTTP/1.1 on one port, and forwards each request to the trial server as HTTP/1.1 on loopback. An HTTP/1.1 upgrade (the
// chat socket) is passed through as raw bytes, as Serve passes it, so the socket stays its own HTTP/1.1 connection.
//
// The live front terminates TLS in Go, and Go's crypto/tls chooses a connection's record sizes (see
// `maxPlaintextForWrite`): records grow in arithmetic progression from one TCP MSS estimate up to 16 KB, and the
// connection uses 16 KB records once it has sent 128 KB. A Node TLS record carries whatever one write gives it, so the
// front writes each record on its own, at the size Go would pick.

import http from 'node:http';
import http2 from 'node:http2';
import net from 'node:net';

// Hop-by-hop headers belong to one connection and are never forwarded (RFC 9113 section 8.2.2).
const HOP_BY_HOP = new Set([
  'connection',
  'keep-alive',
  'proxy-connection',
  'transfer-encoding',
  'upgrade',
  'http2-settings',
  'te',
]);

// Go's crypto/tls record sizing (crypto/tls/conn.go, `maxPayloadSizeForWrite`).
const MAX_PLAINTEXT_BYTES = 16384;
const TCP_MSS_ESTIMATE_BYTES = 1208;
const RECORD_HEADER_BYTES = 5;
const RECORD_SIZE_BOOST_THRESHOLD_BYTES = 128 * 1024;
const AEAD_OVERHEAD_BYTES = 16;
// TLS 1.3 hides the content type inside the record's payload, where TLS 1.2 names it in the header.
const TLS13_CONTENT_TYPE_BYTES = 1;
// An HTTP/2 DATA frame carries a nine-byte header ahead of the bytes the page reads.
const HTTP2_DATA_FRAME_HEADER_BYTES = 9;
// The per-connection record state, shared by every stream of the connection, as Go shares it.
const RECORD_STATE = Symbol('poor-network.go-record-state');

// The TLS record payload Go's crypto/tls would write next on this connection.
function maxPlaintextForWrite(state) {
  if (state.bytesSent >= RECORD_SIZE_BOOST_THRESHOLD_BYTES || state.packetsSent > 1000) {
    return MAX_PLAINTEXT_BYTES;
  }
  const payload = state.maxPayloadBytes * (state.packetsSent + 1);
  state.packetsSent += 1;
  return Math.min(payload, MAX_PLAINTEXT_BYTES);
}

// The payload bytes of one record before the arithmetic progression, as Go subtracts the record header, the explicit
// nonce, the AEAD tag and TLS 1.3's content type from its TCP MSS estimate. The bench's Chrome negotiates an AEAD suite.
function maxPayloadBytes(tlsSocket) {
  const tls13 = tlsSocket.getProtocol() === 'TLSv1.3';
  const cipherName = tlsSocket.getCipher()?.name ?? '';
  if (!/GCM|CHACHA20|POLY1305/i.test(cipherName)) {
    throw new Error(`the TLS front sizes records only for AEAD suites, got ${cipherName || 'none'}`);
  }
  // A TLS 1.2 GCM record carries an 8-byte explicit nonce; TLS 1.3 and ChaCha20-Poly1305 carry none.
  const explicitNonce = !tls13 && /GCM/i.test(cipherName) ? 8 : 0;
  return (
    TCP_MSS_ESTIMATE_BYTES -
    RECORD_HEADER_BYTES -
    explicitNonce -
    AEAD_OVERHEAD_BYTES -
    (tls13 ? TLS13_CONTENT_TYPE_BYTES : 0)
  );
}

function forwardedHeaders(headers) {
  const out = {};
  for (const [name, value] of Object.entries(headers)) {
    if (!name.startsWith(':') && !HOP_BY_HOP.has(name)) {
      out[name] = value;
    }
  }
  return out;
}

// Writes one stream's bytes to the browser one TLS record at a time, each the size Go would pick. A write stays in
// flight until its callback fires, so the next record is a write of its own. `headerBytes` is what the transport adds
// ahead of each write (the HTTP/2 DATA frame header), which the record's payload makes room for.
class RecordWriter {
  #state;
  #headerBytes;
  #write;
  #end;
  #pending = [];
  #buffer = null;
  #offset = 0;
  #busy = false;
  #finished = false;
  #done = false;

  constructor({ state, headerBytes, write, end }) {
    this.#state = state;
    this.#headerBytes = headerBytes;
    this.#write = write;
    this.#end = end;
  }

  write(chunk) {
    if (this.#done || chunk.length === 0) {
      return;
    }
    this.#pending.push(chunk);
    this.#pump();
  }

  finish() {
    if (this.#done) {
      return;
    }
    this.#finished = true;
    this.#pump();
  }

  abort() {
    this.#done = true;
    this.#pending = [];
    this.#buffer = null;
  }

  #pump() {
    if (this.#busy || this.#done) {
      return;
    }
    if (this.#buffer === null || this.#offset >= this.#buffer.length) {
      this.#buffer = this.#pending.shift() ?? null;
      this.#offset = 0;
    }
    if (this.#buffer === null) {
      if (this.#finished) {
        this.#done = true;
        this.#end();
      }
      return;
    }
    const payloadBytes = Math.max(1, maxPlaintextForWrite(this.#state) - this.#headerBytes);
    const chunk = this.#buffer.subarray(this.#offset, this.#offset + payloadBytes);
    this.#offset += chunk.length;
    // Go counts the bytes of the record it writes; the transport's own framing is part of that payload.
    this.#state.bytesSent += chunk.length + this.#headerBytes;
    this.#busy = true;
    try {
      this.#write(chunk, () => {
        this.#busy = false;
        this.#pump();
      });
    } catch {
      this.abort();
    }
  }
}

// Serves `target` through TLS with the given key and certificate. Returns {port, close}.
export async function startFront({ key, cert, target }) {
  const server = http2.createSecureServer({ key, cert, allowHTTP1: true });
  const sockets = new Set();
  // The upstream sockets of upgraded connections (the chat sockets), which close() must end too.
  const tunnels = new Set();
  const stateOf = (socket) => {
    const state = socket[RECORD_STATE];
    if (state === undefined) {
      throw new Error('a TLS socket reached the front without a record state');
    }
    return state;
  };
  server.on('secureConnection', (socket) => {
    sockets.add(socket);
    socket.on('close', () => sockets.delete(socket));
    socket[RECORD_STATE] = { packetsSent: 0, bytesSent: 0, maxPayloadBytes: maxPayloadBytes(socket) };
  });

  server.on('request', (req, res) => {
    const upstream = http.request({
      host: target.host,
      port: target.port,
      method: req.method,
      path: req.url,
      headers: forwardedHeaders(req.headers),
    });
    const writer = new RecordWriter({
      state: stateOf(req.socket),
      headerBytes: HTTP2_DATA_FRAME_HEADER_BYTES,
      write: (chunk, done) => res.write(chunk, done),
      end: () => res.end(),
    });
    upstream.on('response', (upstreamRes) => {
      res.writeHead(upstreamRes.statusCode, forwardedHeaders(upstreamRes.headers));
      upstreamRes.on('data', (chunk) => writer.write(chunk));
      upstreamRes.on('end', () => writer.finish());
      upstreamRes.on('error', () => writer.abort());
    });
    upstream.on('error', (error) => {
      writer.abort();
      if (!res.headersSent) {
        res.writeHead(502, { 'content-type': 'text/plain' });
      }
      res.end(`front: upstream failed: ${error.message}`);
    });
    res.on('error', () => writer.abort());
    // A client that goes away takes its response with it; a finished response keeps its end path.
    res.on('close', () => {
      if (!res.writableEnded) {
        writer.abort();
      }
    });
    req.pipe(upstream);
  });

  server.on('upgrade', (req, socket, head) => {
    const upstream = net.connect({ host: target.host, port: target.port });
    upstream.setNoDelay(true);
    tunnels.add(upstream);
    const writer = new RecordWriter({
      state: stateOf(socket),
      headerBytes: 0,
      write: (chunk, done) => socket.write(chunk, done),
      end: () => socket.end(),
    });
    upstream.on('connect', () => {
      let raw = `${req.method} ${req.url} HTTP/1.1\r\n`;
      for (let i = 0; i < req.rawHeaders.length; i += 2) {
        raw += `${req.rawHeaders[i]}: ${req.rawHeaders[i + 1]}\r\n`;
      }
      upstream.write(`${raw}\r\n`);
      if (head.length > 0) {
        upstream.write(head);
      }
      socket.pipe(upstream);
      upstream.on('data', (chunk) => writer.write(chunk));
      upstream.on('end', () => writer.finish());
      upstream.on('error', () => writer.abort());
    });
    // Either side's end ends the pair: a tunnel left open would hold the bench's process.
    const close = () => {
      writer.abort();
      socket.destroy();
      upstream.destroy();
    };
    upstream.on('error', close);
    upstream.on('close', () => {
      tunnels.delete(upstream);
      socket.destroy();
    });
    socket.on('error', close);
    socket.on('close', () => upstream.destroy());
  });

  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  const port = server.address().port;
  return {
    port,
    close: async () => {
      for (const socket of sockets) {
        socket.destroy();
      }
      for (const upstream of tunnels) {
        upstream.destroy();
      }
      await new Promise((resolve) => server.close(resolve));
    },
  };
}
