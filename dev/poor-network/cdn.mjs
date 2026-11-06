// The CDN stand-in of the open scenario. It is a TLS server that answers the page's CDN URLs from the copies
// assets.mjs keeps: the raw bytes, with the headers the CDN sent. The proxy routes the CDN hosts' CONNECTs here, so the
// CDN's bytes cross the shaped link as the CharlieBot server's do. The other scenarios answer the same URLs in the
// browser instead.

import http2 from 'node:http2';

// The hosts whose CONNECTs the proxy routes to the stand-in. Every CDN URL of the page names one of them.
export const CDN_HOSTS = Object.freeze(['cdn.jsdelivr.net', 'cdnjs.cloudflare.com', 'fonts.googleapis.com']);

// The proxy's routes to a stand-in on `port`: each CDN host's CONNECT target.
export function cdnRoutes(port) {
  return Object.fromEntries(CDN_HOSTS.map((host) => [`${host}:443`, { host: '127.0.0.1', port }]));
}

// Serves the copies in `assets` (Map url -> {raw, body, headers}) over TLS on loopback, as HTTP/2 and HTTP/1.1. A
// brotli copy goes out as its raw bytes to a browser that accepts brotli, and as the decoded body to any other client.
// Returns {port, close}.
export async function startCdn({ key, cert, assets }) {
  const server = http2.createSecureServer({ key, cert, allowHTTP1: true });
  const sockets = new Set();
  server.on('secureConnection', (socket) => {
    sockets.add(socket);
    socket.on('close', () => sockets.delete(socket));
  });

  server.on('request', (req, res) => {
    const authority = req.headers[':authority'] ?? req.headers.host;
    const asset = assets.get(`https://${authority}${req.url}`);
    if (!asset) {
      res.writeHead(404, { 'content-type': 'text/plain' });
      res.end(`no copy of https://${authority}${req.url}`);
      return;
    }
    const acceptsBrotli = /\bbr\b/.test(req.headers['accept-encoding'] ?? '');
    const encoded = asset.headers['content-encoding'] === 'br' && acceptsBrotli;
    const body = encoded ? asset.raw : asset.body;
    const headers = { ...asset.headers, 'content-length': body.length };
    if (!encoded) {
      delete headers['content-encoding'];
    }
    res.writeHead(200, headers);
    res.end(body);
  });

  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  return {
    port: server.address().port,
    close: async () => {
      for (const socket of sockets) {
        socket.destroy();
      }
      await new Promise((resolve) => server.close(resolve));
    },
  };
}
