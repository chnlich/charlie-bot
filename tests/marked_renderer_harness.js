// ---------------------------------------------------------------------------
// Marked harness shared by the chat markdown vm tests
// (chat_single_tilde_literal.test.js, chat_url_ascii_boundary.test.js).
// ---------------------------------------------------------------------------
const vm = require('node:vm');
const https = require('node:https');

const { readStatic } = require('./read_static');

// Fetch the exact marked build the browser serves (web/templates/index.html).
// No version is pinned, so resolving the range today and caching it keeps the
// tests stable while tracking whatever marked ships.
const MARKED_URL = 'https://cdn.jsdelivr.net/npm/marked/marked.min.js';

let markedSrcCache = null;
function fetchMarkedSrc() {
  if (markedSrcCache) return Promise.resolve(markedSrcCache);
  return new Promise((resolve, reject) => {
    https.get(MARKED_URL, (res) => {
      let data = '';
      res.setEncoding('utf8');
      res.on('data', (chunk) => { data += chunk; });
      res.on('end', () => {
        markedSrcCache = data;
        resolve(data);
      });
    }).on('error', reject);
  });
}

async function loadMarkedSrc() {
  try {
    return await fetchMarkedSrc();
  } catch (err) {
    // CDN unreachable in an offline sandbox; fail fast rather than silently pass.
    throw new Error(`could not fetch ${MARKED_URL}: ${err.message}`);
  }
}

// Load the REAL markdown-renderer.js against the REAL marked in a shared vm
// context, mirroring the browser page order: marked.min.js defines the global
// marked first, then markdown-renderer.js registers its renderer + tokenizer
// via marked.use. Stubs cover only the non-marked globals (hljs, document,
// platform) that the file touches; a caller-passed hljs replaces the stub so
// tests can count or shape highlight calls.
async function loadRenderer(hljs) {
  const markedSrc = await loadMarkedSrc();
  const context = {
    console,
    hljs: hljs || {
      getLanguage: () => null,
      highlightAuto: (s) => ({ value: String(s) }),
      highlight: (s) => ({ value: String(s) }),
    },
    document: { querySelectorAll: () => [] },
    platform: {},
  };
  vm.createContext(context);
  vm.runInContext(markedSrc, context, { filename: 'marked.min.js' });
  const src = readStatic('markdown-renderer.js');
  vm.runInContext(src, context, { filename: 'markdown-renderer.js' });
  return context.marked;
}

// The same marked build with no repo renderer loaded, so its tokenizer is
// pristine stock: the control for tokenizer comparisons against loadRenderer().
async function loadStockMarked() {
  const markedSrc = await loadMarkedSrc();
  const context = { console };
  vm.createContext(context);
  vm.runInContext(markedSrc, context, { filename: 'marked.min.js' });
  return context.marked;
}

module.exports = { loadRenderer, loadStockMarked };
