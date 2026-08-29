const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');
const https = require('node:https');

const { readStatic } = require('./read_static');

// Fetch the exact marked build the browser serves (web/templates/index.html).
// No version is pinned, so resolving the range today and caching it keeps this
// test stable while tracking whatever marked ships.
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

// Load the REAL markdown-renderer.js against the REAL marked in a shared vm
// context, mirroring the browser page order: marked.min.js defines the global
// marked first, then markdown-renderer.js registers its renderer + tokenizer
// via marked.use. Stubs cover only the non-marked globals (hljs, document,
// platform) that the file touches.
async function loadRenderer() {
  const markedSrc = await loadMarkedSrc();
  const context = {
    console,
    hljs: {
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

async function loadMarkedSrc() {
  try {
    return await fetchMarkedSrc();
  } catch (err) {
    // CDN unreachable in an offline sandbox; fail fast rather than silently pass.
    throw new Error(`could not fetch ${MARKED_URL}: ${err.message}`);
  }
}

function tildeCount(html) {
  return (html.match(/<del>/g) || []).length;
}

test('two lone tildes in one paragraph render literally with zero <del>', async () => {
  const marked = await loadRenderer();
  const html = marked.parse('用 ~89 ms、共 ~19 个');
  assert.equal(tildeCount(html), 0, `unexpected <del> in: ${html}`);
  assert.match(html, /用 ~89 ms、共 ~19 个/);
  assert.doesNotMatch(html, /<del>/);
});

test('a lone tilde adjacent to prose does not cross-pair into <del>', async () => {
  const marked = await loadRenderer();
  const html = marked.parse('耗时~89ms…共~19个');
  assert.equal(tildeCount(html), 0, `unexpected <del> in: ${html}`);
  assert.match(html, /耗时~89ms…共~19个/);
});

test('~~删除~~ still renders as <del>删除</del>', async () => {
  const marked = await loadRenderer();
  const html = marked.parse('~~删除~~');
  assert.match(html, /<del>删除<\/del>/);
});

test('inline code `cd ~` keeps its tilde with no backslash', async () => {
  const marked = await loadRenderer();
  const html = marked.parse('`cd ~`');
  assert.match(html, /<code>cd ~<\/code>/);
  assert.doesNotMatch(html, /\\\\/);
  assert.doesNotMatch(html, /\\~/);
});

test('fenced code block containing a tilde is byte-identical', async () => {
  const marked = await loadRenderer();
  const html = marked.parse('```\n~x~\n```');
  assert.match(html, />~x~</, 'fence body ~x~ changed or disappeared');
  assert.doesNotMatch(html, /<del>/);
  assert.doesNotMatch(html, /\\~/);
});

test('~~~x~~~ and ~~~~x~~~~ produce no <del>', async () => {
  const marked = await loadRenderer();
  const triple = marked.parse('~~~x~~~');
  const quad = marked.parse('~~~~x~~~~');
  assert.doesNotMatch(triple, /<del>/);
  assert.doesNotMatch(quad, /<del>/);
});