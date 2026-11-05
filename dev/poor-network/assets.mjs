// The page's external assets. The page loads these from public CDNs. The bench answers the same URLs from a local copy
// through the browser's request interception, so the assets never cross the shaped link, and a page that renders its
// markdown in the bench renders it the same way it does in production.

import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';

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

// Reads each asset from `dir`, downloading the ones it lacks. Returns Map url -> {body, type, sha256}. A download that
// fails stops the run: a page without its markdown renderer would measure a different page.
export async function loadAssets(dir) {
  fs.mkdirSync(dir, { recursive: true });
  const assets = new Map();
  for (const [url, type] of ASSET_URLS) {
    const file = path.join(dir, crypto.createHash('sha256').update(url).digest('hex'));
    if (!fs.existsSync(file)) {
      const response = await fetch(url);
      if (!response.ok) {
        throw new Error(`fetching ${url} failed: HTTP ${response.status}`);
      }
      fs.writeFileSync(file, Buffer.from(await response.arrayBuffer()));
    }
    const body = fs.readFileSync(file);
    assets.set(url, { body, type, sha256: crypto.createHash('sha256').update(body).digest('hex') });
  }
  const empty = Buffer.from('/* the bench serves no webfonts */\n');
  assets.set(FONT_STYLESHEET_URL, {
    body: empty,
    type: CSS,
    sha256: crypto.createHash('sha256').update(empty).digest('hex'),
  });
  return assets;
}
