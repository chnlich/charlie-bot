const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const { escapeHtml } = require('./escape_html_stub');

const NAMESPACE_JS = fs.readFileSync(
  path.join(__dirname, '..', 'web', 'static', 'js', 'chat', 'namespace.js'),
  'utf8'
);
const ARTIFACTS_JS = fs.readFileSync(
  path.join(__dirname, '..', 'web', 'static', 'js', 'chat', 'artifacts.js'),
  'utf8'
);

function loadArtifactsScript() {
  const context = {
    SESSION_ID: 'test-session',
    escapeHtml,
    hljs: {highlight: (value) => ({value: escapeHtml(value)})},
    localStorage: {getItem: () => null, setItem: () => {}},
    window: {addEventListener: () => {}},
    console,
    URL: globalThis.URL,
  };
  vm.createContext(context);
  // namespace.js runs inside the vm so expose() assigns onto the vm's own
  // globalThis, not the outer Node global.
  vm.runInContext(NAMESPACE_JS, context, {filename: 'chat/namespace.js'});
  vm.runInContext(ARTIFACTS_JS, context, {filename: 'artifacts.js'});
  return context;
}

function countBaseTags(html) {
  const matches = html.match(/<base\b[^>]*>/gi);
  return matches ? matches.length : 0;
}

function extractInterceptorScript(html) {
  const match = html.match(/<script>\(function\(\)\{document\.addEventListener\([\s\S]*?<\/script>/);
  return match ? match[0] : '';
}

test('injectLinkBehavior injects one base tag with the artifact /files href inside the head', () => {
  const ctx = loadArtifactsScript();
  const absPath = '/home/chaoli/sessions/test-session/artifacts/plan.html';
  const input = '<html><head></head><body><p>hi</p></body></html>';
  const out = ctx.injectLinkBehavior(input, absPath);

  assert.equal(countBaseTags(out), 1, 'exactly one <base> tag');
  const baseMatch = out.match(/<base\b[^>]*>/i);
  assert.ok(baseMatch, '<base> tag present');
  assert.match(baseMatch[0], new RegExp('href="' + escapeHtml('/files' + absPath) + '"'));

  const headOpen = out.indexOf('<head>');
  const headClose = out.indexOf('</head>');
  const baseIdx = out.indexOf('<base');
  assert.ok(headOpen !== -1 && headClose !== -1, 'head tags retained');
  assert.ok(baseIdx > headOpen && baseIdx < headClose, 'base tag placed inside <head>');

  const bodyIdx = out.indexOf('</body>');
  const scriptIdx = out.lastIndexOf('<script>');
  assert.ok(scriptIdx !== -1 && scriptIdx < bodyIdx, 'interceptor script before </body>');
});

test('injectLinkBehavior does not inject a second base tag when one already exists', () => {
  const ctx = loadArtifactsScript();
  const absPath = '/home/chaoli/sessions/test-session/artifacts/plan.html';
  const input = '<html><head><base href="/other"></head><body><p>hi</p></body></html>';
  const out = ctx.injectLinkBehavior(input, absPath);

  assert.equal(countBaseTags(out), 1, 'no second base tag injected');
  const baseMatch = out.match(/<base\b[^>]*>/i);
  assert.ok(baseMatch, 'original base tag retained');
  assert.match(baseMatch[0], /href="\/other"/, 'existing base href untouched');

  const bodyIdx = out.indexOf('</body>');
  const scriptIdx = out.lastIndexOf('<script>');
  assert.ok(scriptIdx !== -1 && scriptIdx < bodyIdx, 'interceptor script still injected before </body>');
});

test('injectLinkBehavior prepends base and appends interceptor when no head or body', () => {
  const ctx = loadArtifactsScript();
  const absPath = '/tmp/report/artifacts/plot.html';
  const input = '<p>hello</p>';
  const out = ctx.injectLinkBehavior(input, absPath);

  assert.ok(out.startsWith('<base href="'), 'base tag prepended at the very start');
  assert.equal(countBaseTags(out), 1, 'exactly one base tag');
  assert.ok(out.endsWith('</script>'), 'interceptor appended at the end');
  assert.ok(out.indexOf(input) > out.indexOf('<base'), 'original fragment preserved after base');
});

test('injectLinkBehavior base href contains no cbsession viewing fragment', () => {
  const ctx = loadArtifactsScript();
  const absPath = '/home/chaoli/sessions/test-session/artifacts/plan.html';
  const out = ctx.injectLinkBehavior('<html><head></head><body></body></html>', absPath);
  const baseMatch = out.match(/<base\b[^>]*>/i);
  assert.ok(baseMatch, 'base tag present');
  assert.doesNotMatch(baseMatch[0], /cbsession/, 'base href has no #cbsession fragment');
  assert.match(baseMatch[0], new RegExp('href="' + escapeHtml('/files' + absPath) + '"'));
});

test('injectLinkBehavior interceptor contains scrollIntoView branch without location.hash assignment', () => {
  const ctx = loadArtifactsScript();
  const out = ctx.injectLinkBehavior('<html><head></head><body></body></html>', '/a/b.html');
  const script = extractInterceptorScript(out);
  assert.ok(script, 'interceptor script present');

  assert.match(script, /charAt\(0\)==="#"/, 'raw href starts-with-# branch present');
  assert.match(script, /e\.preventDefault\(\)/, 'preventDefault called for fragment links');
  assert.match(script, /scrollIntoView\(\)/, 'scrollIntoView called for fragment targets');
  assert.doesNotMatch(script, /location\.hash/, 'no location.hash assignment in interceptor');
  assert.match(script, /if\(raw==="#"\) return;/, 'bare # is a silent no-op');
  assert.match(script, /decodeURIComponent\(/, 'fragment is decoded before lookup');
  assert.match(script, /document\.getElementById\(frag\)||document\.getElementsByName\(frag\)\[0\]/,
    'falls back from getElementById to getByName');
});

test('injectLinkBehavior interceptor opens other links in a new tab via window.open', () => {
  const ctx = loadArtifactsScript();
  const out = ctx.injectLinkBehavior('<html><head></head><body></body></html>', '/a/b.html');
  const script = extractInterceptorScript(out);
  assert.ok(script, 'interceptor script present');

  assert.match(script, /window\.open\(url\.href,\s*"_blank",\s*"noopener"\)/,
    'window.open with _blank and noopener');
  assert.match(script, /new URL\(raw,\s*document\.baseURI\)/, 'resolves against document.baseURI');
  assert.match(script, /console\.warn\(/, 'parse failure logs a warning');
  assert.match(script, /e\.ctrlKey\|\|e\.metaKey\|\|e\.shiftKey\|\|e\.altKey/, 'modifier keys fall through');
  assert.match(script, /e\.button!==0/, 'non-primary buttons fall through');
  assert.match(script, /e\.defaultPrevented/, 'already-prevented clicks fall through');
  assert.match(script, /e\.target\.closest\("a\[href\]"\)/, 'anchor lookup via closest');
});
