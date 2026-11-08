// The page's external assets. The page loads these from public CDNs. The bench keeps a copy of each answer, as the CDN
// sends it: the body's bytes, brotli-compressed as the CDN compresses them, and the headers that matter to a browser's
// cache and to Resource Timing. Two paths use the copy. The other scenarios answer the same URLs from the decoded body
// through the browser's request interception, so the assets never cross the shaped link. The open scenario serves the
// raw bytes from the CDN stand-in (cdn.mjs) through the link, so the CDN's bytes cross it as the CDN's would.

import crypto from 'node:crypto';
import fs from 'node:fs';
import https from 'node:https';
import path from 'node:path';
import zlib from 'node:zlib';

const JS = 'application/javascript';
const CSS = 'text/css';

// The external URLs of web/templates/index.html, as the browser requests them.
export const ASSET_URLS = Object.freeze([
  ['https://cdn.jsdelivr.net/npm/marked/marked.min.js', JS],
  ['https://cdn.jsdelivr.net/npm/katex@0.16.21/dist/katex.min.css', CSS],
  ['https://cdn.jsdelivr.net/npm/katex@0.16.21/dist/katex.min.js', JS],
  ['https://cdn.jsdelivr.net/npm/katex@0.16.21/dist/contrib/auto-render.min.js', JS],
  ['https://cdn.jsdelivr.net/npm/xterm@5.3.0/css/xterm.css', CSS],
  ['https://cdn.jsdelivr.net/npm/xterm@5.3.0/lib/xterm.js', JS],
  ['https://cdn.jsdelivr.net/npm/xterm-addon-fit@0.8.0/lib/xterm-addon-fit.js', JS],
  ['https://cdnjs.cloudflare.com/ajax/libs/highlight.js/11.9.0/highlight.min.js', JS],
  ['https://cdnjs.cloudflare.com/ajax/libs/highlight.js/11.9.0/styles/github-dark.min.css', CSS],
]);

// The webfont stylesheet is answered empty: the bench measures the chat, not the fonts.
const FONT_STYLESHEET_URL = 'https://fonts.googleapis.com/css2?family=Fira+Code:wght@400;500&display=swap';

// The request patterns the browser intercepts. Anything else on these hosts is not in the page.
export const INTERCEPT_PATTERNS = Object.freeze([
  'https://cdn.jsdelivr.net/*',
  'https://cdnjs.cloudflare.com/*',
  'https://fonts.googleapis.com/*',
]);

// The headers a copy keeps: the ones that decide a browser's cache, its encoding and its Resource Timing. jsDelivr
// and cdnjs send Timing-Allow-Origin, so a browser reports the transferred size of their files.
const REPLAYED_HEADERS = ['cache-control', 'content-type', 'content-encoding', 'etag', 'last-modified',
  'timing-allow-origin'];

function sha256(bytes) {
  return crypto.createHash('sha256').update(bytes).digest('hex');
}

// The answer of `url` as the CDN sends it to a browser that accepts brotli: the body's bytes unchanged, and the
// replayed headers.
function downloadRaw(url) {
  return new Promise((resolve, reject) => {
    https.get(url, { headers: { 'accept-encoding': 'br' } }, (response) => {
      if (response.statusCode !== 200) {
        response.resume();
        reject(new Error(`fetching ${url} failed: HTTP ${response.statusCode}`));
        return;
      }
      const chunks = [];
      response.on('data', (chunk) => chunks.push(chunk));
      response.on('end', () => {
        const headers = Object.fromEntries(REPLAYED_HEADERS.filter((name) => response.headers[name] !== undefined)
          .map((name) => [name, response.headers[name]]));
        resolve({ raw: Buffer.concat(chunks), headers });
      });
      response.on('error', reject);
    }).on('error', reject);
  });
}

// The body a browser sees: the raw bytes, decoded by the content encoding the copy carries.
function decodedBody(raw, headers) {
  const encoding = headers['content-encoding'];
  if (encoding === undefined) {
    return raw;
  }
  if (encoding !== 'br') {
    throw new Error(`a copy with content-encoding ${encoding} is not supported`);
  }
  return zlib.brotliDecompressSync(raw);
}

// Reads each asset from `dir`, downloading the ones it lacks: the raw bytes in `<sha256 of url>` and their headers in
// `<sha256 of url>.json`. Returns Map url -> {body, raw, headers, type, sha256}, where `body` is the decoded body and
// `sha256` its hash. A download that fails stops the run: a page without its markdown renderer would measure a
// different page.
export async function loadAssets(dir) {
  fs.mkdirSync(dir, { recursive: true });
  const assets = new Map();
  for (const [url, type] of ASSET_URLS) {
    const base = path.join(dir, sha256(url));
    if (!fs.existsSync(`${base}.json`)) {
      const { raw, headers } = await downloadRaw(url);
      fs.writeFileSync(base, raw);
      fs.writeFileSync(`${base}.json`, JSON.stringify(headers));
    }
    const raw = fs.readFileSync(base);
    const headers = JSON.parse(fs.readFileSync(`${base}.json`, 'utf8'));
    const body = decodedBody(raw, headers);
    assets.set(url, { body, raw, headers, type, sha256: sha256(body) });
  }
  const empty = Buffer.from('/* the bench serves no webfonts */\n');
  const fontHeaders = { 'content-type': CSS, 'cache-control': 'private, max-age=86400' };
  assets.set(FONT_STYLESHEET_URL, {
    body: empty,
    raw: empty,
    headers: fontHeaders,
    type: CSS,
    sha256: sha256(empty),
  });
  return assets;
}
