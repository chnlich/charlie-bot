// The TLS front: the bench's stand-in for Tailscale Serve. It terminates TLS for the browser, serves HTTP/2 and
// HTTP/1.1 on one port, and forwards each request to the trial server as HTTP/1.1 on loopback. An HTTP/1.1 upgrade (the
// chat socket) is passed through as raw bytes, as Serve passes it, so the socket stays its own HTTP/1.1 connection.

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

function forwardedHeaders(headers) {
  const out = {};
  for (const [name, value] of Object.entries(headers)) {
    if (!name.startsWith(':') && !HOP_BY_HOP.has(name)) {
      out[name] = value;
    }
  }
  return out;
}

// Serves `target` through TLS with the given key and certificate. Returns {port, close}.
export async function startFront({ key, cert, target }) {
  const server = http2.createSecureServer({ key, cert, allowHTTP1: true });
  const sockets = new Set();
  // The upstream sockets of upgraded connections (the chat sockets), which close() must end too.
  const tunnels = new Set();
  server.on('secureConnection', (socket) => {
    sockets.add(socket);
    socket.on('close', () => sockets.delete(socket));
  });

  server.on('request', (req, res) => {
    const upstream = http.request({
      host: target.host,
      port: target.port,
      method: req.method,
      path: req.url,
      headers: forwardedHeaders(req.headers),
    });
    upstream.on('response', (upstreamRes) => {
      res.writeHead(upstreamRes.statusCode, forwardedHeaders(upstreamRes.headers));
      upstreamRes.pipe(res);
    });
    upstream.on('error', (error) => {
      if (!res.headersSent) {
        res.writeHead(502, { 'content-type': 'text/plain' });
      }
      res.end(`front: upstream failed: ${error.message}`);
    });
    req.pipe(upstream);
  });

  server.on('upgrade', (req, socket, head) => {
    const upstream = net.connect({ host: target.host, port: target.port });
    upstream.setNoDelay(true);
    tunnels.add(upstream);
    upstream.on('connect', () => {
      let raw = `${req.method} ${req.url} HTTP/1.1\r\n`;
      for (let i = 0; i < req.rawHeaders.length; i += 2) {
        raw += `${req.rawHeaders[i]}: ${req.rawHeaders[i + 1]}\r\n`;
      }
      upstream.write(`${raw}\r\n`);
      if (head.length > 0) {
        upstream.write(head);
      }
      socket.pipe(upstream).pipe(socket);
    });
    // Either side's end ends the pair: a tunnel left open would hold the bench's process.
    const close = () => {
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
