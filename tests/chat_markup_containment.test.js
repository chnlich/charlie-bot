const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const { readStatic } = require('./read_static');
const { hljsStub } = require('./hljs_stub');

// Load markdown-renderer.js in a fake environment that supplies marked, hljs,
// and a stub document. Stubbing marked.use captures the renderer the file
// registers, so each hook can be driven directly (mechanism-level, not output
// string matching). The fake marked.Renderer is a no-op constructor so the
// IIFE's `new marked.Renderer()` yields a plain object the IIFE then decorates.
function loadRenderer() {
  let captured = null;
  const context = {
    marked: {
      Renderer: function() {},
      use: function(opts) { captured = opts.renderer; },
    },
    hljs: hljsStub,
    document: {
      querySelectorAll: () => [],
    },
  };
  vm.createContext(context);
  const src = readStatic('markdown-renderer.js');
  vm.runInContext(src, context, { filename: 'markdown-renderer.js' });
  return { renderer: captured, context };
}

// Parse attribute boundaries of a single opening tag. Returns the list of
// attribute names (lower-cased) in source order. Because escaped values use
// &quot;/&#39; (no raw quote characters), a name="value" scan correctly groups
// each value, so an attacker payload buried inside a value is NOT counted as a
// standalone attribute.
function parseAttrNames(openTag) {
  const names = [];
  const re = /\s([A-Za-z_:][-A-Za-z0-9_:.]*)\s*=\s*"([^"]*)"/g;
  let m;
  while ((m = re.exec(openTag)) !== null) {
    names.push(m[1].toLowerCase());
  }
  return names;
}

function openTag(html) {
  const m = html.match(/<\w+[^>]*>/);
  assert.ok(m, `expected an opening tag in: ${html}`);
  return m[0];
}

test('renderer.html escapes raw html tokens so no tag reaches the DOM', () => {
  const { renderer } = loadRenderer();

  // v5+ object form
  const out5 = renderer.html({ text: '<b>hi</b><img src=x onerror=alert(1)>' });
  assert.doesNotMatch(out5, /<[a-zA-Z!\/][^>]*>/, 'v5 html output contains a parseable tag');
  assert.match(out5, /&lt;b&gt;/);
  assert.match(out5, /&lt;img/);

  // v4 string form
  const out4 = renderer.html('<div class="x">z</div>');
  assert.doesNotMatch(out4, /<[a-zA-Z!\/][^>]*>/, 'v4 html output contains a parseable tag');
  assert.match(out4, /&lt;div/);
  assert.match(out4, /&lt;\/div&gt;/);
});

test('renderer.link calls parseInline for link text (not raw interpolation)', () => {
  const { renderer } = loadRenderer();
  let parseInlineCalled = false;
  const stubParser = {
    parseInline(tokens) {
      parseInlineCalled = true;
      return 'PARSED';
    },
  };
  const token = {
    href: 'https://e.com',
    title: '',
    text: 'raw <b> text',
    tokens: [{ type: 'text', raw: 'raw <b> text' }],
  };
  const out = renderer.link.call({ parser: stubParser }, token);

  assert.ok(parseInlineCalled, 'parseInline was not called for link text');
  assert.match(out, />PARSED</, 'link text did not come from parseInline');
  // The raw token.text (which contains a tag) must NOT be interpolated directly.
  assert.doesNotMatch(out, /raw <b> text/, 'raw token.text was interpolated into link text');
});

test('renderer.link escapes href and title so no attribute breakout occurs', () => {
  const { renderer } = loadRenderer();
  const stubParser = { parseInline: () => 'x' };
  const token = {
    href: 'https://e.com"onmouseover="alert(1)',
    title: 't"onmouseover="alert(2)',
    text: 'x',
    tokens: [{ type: 'text', raw: 'x' }],
  };
  const out = renderer.link.call({ parser: stubParser }, token);
  const tag = openTag(out);
  const names = parseAttrNames(tag);

  assert.deepEqual(names.sort(), ['href', 'rel', 'target', 'title'].sort(),
    `unexpected attribute set: ${names.join(', ')}`);
  assert.ok(!names.includes('onmouseover'), 'onmouseover broke out as an attribute');
  // The injected payloads survive only as literal text inside the escaped values.
  const hrefVal = tag.match(/href="([^"]*)"/)[1];
  assert.match(hrefVal, /onmouseover/);
  assert.doesNotMatch(hrefVal, /"/);
});

test('renderer.link falls back to escaped token.text when tokens are missing', () => {
  const { renderer } = loadRenderer();
  let parseInlineCalled = false;
  const stubParser = {
    parseInline: () => { parseInlineCalled = true; return 'SHOULD-NOT-HAPPEN'; },
  };
  const token = { href: 'https://e.com', title: '', text: 'a <b> b', tokens: undefined };
  const out = renderer.link.call({ parser: stubParser }, token);

  assert.equal(parseInlineCalled, false, 'parseInline called without tokens');
  assert.doesNotMatch(out, /SHOULD-NOT-HAPPEN/);
  assert.match(out, /a &lt;b&gt; b/, 'fallback text was not escaped');
});

test('renderer.link preserves nested bold and inline code from parseInline', () => {
  const { renderer } = loadRenderer();
  const stubParser = {
    parseInline: () => '<strong>bold</strong> and <code>code</code>',
  };
  const token = {
    href: 'https://e.com',
    title: '',
    text: '**bold** and `code`',
    tokens: [{ type: 'strong' }, { type: 'codespan' }],
  };
  const out = renderer.link.call({ parser: stubParser }, token);

  assert.match(out, /<strong>bold<\/strong>/, 'nested bold did not survive');
  assert.match(out, /<code>code<\/code>/, 'nested inline code did not survive');
});

test('renderer.code escapes a hostile lang name so no element is created', () => {
  const { renderer } = loadRenderer();
  const out = renderer.code({ text: 'let x = 1;', lang: '<img src=x onerror=alert(1)>' });

  // No real <img> element anywhere in the block.
  assert.doesNotMatch(out, /<img[^>]*>/i, 'a real img element was emitted');
  // The lang appears inside the code-lang span as escaped literal text.
  const span = out.match(/<span class="code-lang">([\s\S]*?)<\/span>/);
  assert.ok(span, 'code-lang span missing');
  assert.doesNotMatch(span[1], /<img/i, 'lang span contains an unescaped tag');
  assert.match(span[1], /&lt;img/);
});

test('renderer.image hostile alt does not increase the attribute count', () => {
  const { renderer } = loadRenderer();

  const benign = renderer.image({ text: 'x', href: 'http://e.com/a.png', title: '' });
  const hostile = renderer.image({ text: 'x" onerror="alert(1)', href: 'http://e.com/a.png', title: '' });

  const benignNames = parseAttrNames(openTag(benign));
  const hostileNames = parseAttrNames(openTag(hostile));

  assert.equal(hostileNames.length, benignNames.length,
    `hostile alt changed attribute count: benign=${benignNames.join(',')} hostile=${hostileNames.join(',')}`);
  assert.ok(!hostileNames.includes('onerror'), 'onerror broke out as an img attribute');
  assert.deepEqual(hostileNames.sort(), benignNames.sort());
});

test('renderer.image escapes href and title attributes', () => {
  const { renderer } = loadRenderer();
  const out = renderer.image({
    text: 'x',
    href: 'http://e.com/a.png"onerror="alert(1)',
    title: 't"onerror="alert(2)',
  });
  const tag = openTag(out);
  const names = parseAttrNames(tag);

  assert.deepEqual(names.sort(), ['alt', 'src', 'title'].sort(),
    `unexpected img attribute set: ${names.join(', ')}`);
  assert.ok(!names.includes('onerror'), 'onerror broke out as an img attribute');
});

test('renderer.link and renderer.image leave benign inputs untouched', () => {
  const { renderer } = loadRenderer();
  const stubParser = { parseInline: () => 'click here' };
  const link = renderer.link.call({ parser: stubParser },
    { href: 'https://e.com', title: 'ok', text: 'click here', tokens: [{ type: 'text' }] });
  assert.match(link, /<a href="https:\/\/e\.com" target="_blank" rel="noopener noreferrer" title="ok">click here<\/a>/);

  const img = renderer.image({ text: 'pic', href: 'https://e.com/p.png', title: 'cap' });
  assert.match(img, /<img src="https:\/\/e\.com\/p\.png" alt="pic" title="cap">/);
});
