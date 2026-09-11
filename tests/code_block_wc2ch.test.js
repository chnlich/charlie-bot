// ---------------------------------------------------------------------------
// Wide-char 2ch boxing (wrapWideChars in markdown-renderer.js): the range
// table's membership, the single-pass tag/text scan, the five render mounts,
// and the session-645 incident fixture. Byte-identity claims are exact: a
// block without W/F chars renders byte-identically to the identity wrap, and
// every pre's textContent survives the wrap unchanged.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const { readStatic } = require('./read_static');
const { FakeElement } = require('./fake_dom');
const { loadRendererContext } = require('./marked_renderer_harness');
const { buildRendererContext } = require('./renderer_vm_context');
const { loadChatRenderingModules } = require('./chat_rendering_context_stub');

// Fake marked with the surface markdown-renderer.js touches at load, plus the
// parse the (real, loaded) renderProseMarkdown takes when a chat-surface test
// renders a message body.
const FAKE_MARKED_SRC = `
globalThis.marked = {
  Renderer: function() { return {}; },
  use() {},
  parse: (s) => '<p>' + String(s || '') + '</p>',
};`;

// A shaped hljs: escapes like the real highlight.js and wraps `const` in a
// keyword span, so wrap-inside-token nesting is exercised on hljs-shaped
// bytes.
function esc(s) {
  return String(s)
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
    .replace(/\bconst\b/g, '<span class="hljs-keyword">const</span>');
}
const shapedHljs = {
  getLanguage: () => null,
  highlightAuto: (s) => ({ value: esc(s) }),
  highlight: (s) => ({ value: esc(s) }),
};

// markdown-renderer.js plus the chat surfaces (namespace, shared, rendering)
// in one context: wrapWideChars is the REAL one here, so the tool-output and
// raw-backend mounts are exercised end to end.
function loadChatSurfaceContext() {
  const elements = new Map();
  const context = {
    console: { error() {}, warn() {}, log() {} },
    document: {
      getElementById(id) {
        if (!elements.has(id)) elements.set(id, new FakeElement());
        return elements.get(id);
      },
      createElement(tag) { return new FakeElement(tag); },
      querySelector() { return null; },
      querySelectorAll: () => [],
    },
    platform: {},
    CSS: { escape: (v) => String(v) },
    SESSION_ID: 'sess-1',
    fetch: () => Promise.resolve({ ok: true }),
    _elements: elements,
  };
  vm.createContext(context);
  vm.runInContext('Math.random = () => 0.5', context);
  vm.runInContext(FAKE_MARKED_SRC, context, { filename: 'marked-fake.js' });
  vm.runInContext(readStatic('markdown-renderer.js'), context, { filename: 'markdown-renderer.js' });
  loadChatRenderingModules(context);
  return context;
}

// The <pre><code> inner HTML of the first code block in a rendered page.
function preHtml(html) {
  const m = /<pre><code class="hljs"[^>]*>([\s\S]*?)<\/code><\/pre>/.exec(html);
  assert.ok(m, 'no code block pre in: ' + html.slice(0, 200));
  return m[1];
}

// DOM-free textContent: the spans are layout-only, so stripping tags and
// decoding the escapers' entities recovers the code text byte for byte.
function textContent(html) {
  return String(html).replace(/<[^>]*>/g, '')
    .replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&quot;/g, '"')
    .replace(/&#39;/g, "'").replace(/&amp;/g, '&');
}

function unwrapForBaseline(context) {
  // Global lookup at call time: codeBlockHtml and the flush re-read
  // wrapWideChars from the context, so an identity stub reconstructs
  // today's (pre-change) bytes for comparison.
  context.wrapWideChars = (html) => html;
}

test('markdown-renderer.js parses whole with the inline W/F range table', async () => {
  const context = await loadRendererContext();
  assert.equal(typeof context.wrapWideChars, 'function');
  assert.equal(typeof context.wc2chIsWide, 'function');
  const ranges = context.WC2CH_RANGES;
  assert.ok(Array.isArray(ranges) && ranges.length > 0);
  for (let i = 0; i < ranges.length; i++) {
    assert.ok(Array.isArray(ranges[i]) && ranges[i].length === 2, 'range entry shape');
    assert.ok(ranges[i][0] <= ranges[i][1], 'range entry ordered');
    if (i > 0) assert.ok(ranges[i - 1][1] < ranges[i][0], 'table sorted, non-overlapping');
  }
});

test('W/F table sampling is pinned to the Unicode 16.0 ranges', async () => {
  const context = await loadRendererContext();
  // 291 W/F ranges in Unicode 16.0.0's EastAsianWidth.txt — a different
  // Unicode version regenerates a different count and fails here on purpose.
  assert.equal(context.WC2CH_RANGES.length, 291, 'Unicode 16.0.0 W/F range count');
  const is = context.wc2chIsWide;
  // The contract's samples: hanzi, ideographic full stop, fullwidth letter.
  for (const cp of [0x4E2D, 0x4E00, 0x3002, 0xFF21]) {
    assert.ok(is(cp), 'U+' + cp.toString(16).toUpperCase() + ' is W/F');
  }
  // Ambiguous-width chars stay out: box drawing, plus/minus, middle dot,
  // arrows and the geometric shapes CJK diagrams draw rails with.
  for (const cp of [0x2500, 0x2502, 0x00B1, 0x00B7, 0x2192, 0x25BC, 0x2022, 0x2605]) {
    assert.ok(!is(cp), 'U+' + cp.toString(16).toUpperCase() + ' is not W/F');
  }
  // Range boundaries pinned from the 16.0 table: inside vs one past each edge.
  const edges = [
    [0x115F, true], [0x1160, false],   // Hangul jamo block ends
    [0x231A, true], [0x2319, false],   // WATCH is W
    [0x231B, true], [0x231C, false],
    [0xFF01, true], [0xFF00, false],   // fullwidth block starts
    [0x2FFFD, true], [0x2FFFE, false], // plane 2 default-W ceiling
    [0x30000, true], [0x2FFFF, false], // plane 3 default-W floor
    [0x3FFFD, true], [0x3FFFE, false],
    [0xFE0F, false],                   // variation selector: Mn, not W/F
    [0x1F7F0, true], [0x1F700, false], // emoji vs alchemical symbols
    [0x1F7E0, true], [0x1F7EC, false],
  ];
  for (const [cp, expected] of edges) {
    assert.equal(is(cp), expected, 'U+' + cp.toString(16).toUpperCase() + ' classified ' + expected);
  }
});

test('tags pass through verbatim; only text segments wrap', async () => {
  const context = await loadRendererContext();
  const w = context.wrapWideChars;
  const out = w('<span class="hljs-keyword">const</span> \u4e2d');
  assert.equal(out, '<span class="hljs-keyword">const</span> <span class="wc2ch">\u4e2d</span>');
  // A wide char inside a tag (attribute value) is never touched.
  assert.equal(w('<span data-x="\u4e2d">x</span>'), '<span data-x="\u4e2d">x</span>');
  // A bare '<' that never closes stays text and its neighbors still wrap.
  assert.equal(w('a < b \u4e2d'), 'a < b <span class="wc2ch">\u4e2d</span>');
});

test('entities survive untouched', async () => {
  const context = await loadRendererContext();
  const out = context.wrapWideChars('&amp;&lt;&gt;&quot;&#39;\u4e2d\u6587');
  assert.equal(out, '&amp;&lt;&gt;&quot;&#39;<span class="wc2ch">\u4e2d</span><span class="wc2ch">\u6587</span>');
});

test('wrap lands inside hljs token spans', async () => {
  const context = await loadRendererContext(shapedHljs);
  // Direct: the wc2ch box nests inside the token's span.
  assert.equal(
    context.wrapWideChars('<span class="hljs-string">"\u4e2d\u6587"</span>'),
    '<span class="hljs-string">"<span class="wc2ch">\u4e2d</span><span class="wc2ch">\u6587</span>"</span>');
  // Through renderer.code: the keyword span's ASCII bytes are untouched and
  // the CJK operand is boxed inside the highlighted output.
  const html = context.marked.parse('```js\nconst \u4e2d = 1;\n```');
  const pre = preHtml(html);
  assert.match(pre, /<span class="hljs-keyword">const<\/span>/);
  assert.match(pre, /<span class="wc2ch">\u4e2d<\/span>/);
  assert.equal(textContent(pre), 'const \u4e2d = 1;');
});

test('ambiguous-width chars stay unwrapped', async () => {
  const context = await loadRendererContext();
  const ambiguous = '\u2500\u2502\u250c\u2510\u2514\u2518\u251c\u2524\u252c\u2534\u253c\u00b1\u00b7\u2192\u25bc\u2022';
  assert.equal(context.wrapWideChars(ambiguous), ambiguous);
});

test('surrogate-pair emoji box whole', async () => {
  const context = await loadRendererContext();
  const out = context.wrapWideChars('\u{1F600}\u{20000}');
  assert.equal(out, '<span class="wc2ch">\u{1F600}</span><span class="wc2ch">\u{20000}</span>');
  // No split pairs: every wc2ch span carries exactly one code point.
  for (const m of out.matchAll(/<span class="wc2ch">([\s\S]*?)<\/span>/g)) {
    assert.equal([...m[1]].length, 1, 'one code point per box: ' + JSON.stringify(m[1]));
  }
});

test("combining marks and U+FE0F attach to the wide char's box", async () => {
  const context = await loadRendererContext();
  const w = context.wrapWideChars;
  assert.equal(w('\u4e2d\u0301'), '<span class="wc2ch">\u4e2d\u0301</span>');
  assert.equal(w('\u4e2d\u0301\uFE0F'), '<span class="wc2ch">\u4e2d\u0301\uFE0F</span>');
  assert.equal(w('\u2615\uFE0F'), '<span class="wc2ch">\u2615\uFE0F</span>');
  // Marks and FE0F not led by a wide char stay bare text.
  assert.equal(w('a\u0301\uFE0F'), 'a\u0301\uFE0F');
  // An astral combining mark joins its (astral) wide char's box.
  assert.equal(w('\u{20000}\u{1D165}'), '<span class="wc2ch">\u{20000}\u{1D165}</span>');
});

test('blocks without wide chars render byte-identical to the unwrapped bytes', async () => {
  const context = await loadRendererContext(shapedHljs);
  const body = 'prose\n\n```js\nconst x = 1; // rail \u2500\u2502 \u00b1 keep\nconst s = "a<b>&c";\n```\n\ntail';
  const wrapped = context.marked.parse(body);
  unwrapForBaseline(context);
  const baseline = context.marked.parse(body);
  assert.equal(wrapped, baseline);
});

test('the flush writes the memo settled bytes (wrapped) to the live DOM', async () => {
  const markedSrc = `
let codeRenderer = null;
globalThis.marked = {
  Renderer: function() { return {}; },
  use(opts) { if (opts.renderer && opts.renderer.code) codeRenderer = opts.renderer.code; },
  parse: (s) => '<pre>' + codeRenderer({ text: s, lang: '', raw: '' }) + '</pre>',
};`;
  const context = buildRendererContext({ withTimers: true });
  vm.createContext(context);
  vm.runInContext(markedSrc, context, { filename: 'marked-code-fake.js' });
  vm.runInContext(readStatic('markdown-renderer.js'), context, { filename: 'markdown-renderer.js' });
  const els = [];
  context.document = {
    querySelectorAll(sel) {
      const m = /data-hl="(\d+)"/.exec(sel);
      if (!m) return [];
      const el = { innerHTML: '', removed: false, removeAttribute() { this.removed = true; } };
      els.push(el);
      return [el];
    },
  };
  const text = 'rail \u2500\u2502 stays, \u4e2d\u6587 boxes';
  context.renderProseMarkdown(text);
  assert.equal(els.length, 0); // deferred: nothing written before the flush
  context.__runTimers();
  assert.equal(els.length, 1);
  const settled = context.renderProseMarkdown(text);
  const direct = context.marked.parse(context.fixNestedFences(text));
  assert.equal(settled, direct, 'memo settled bytes stay identical to the direct render');
  const settledInner = /<code class="hljs">([\s\S]*)<\/code>/.exec(settled)[1];
  assert.equal(els[0].innerHTML, settledInner, 'DOM write is byte-identical to the memo entry');
  assert.match(els[0].innerHTML, /<span class="wc2ch">\u4e2d<\/span>/);
  assert.match(els[0].innerHTML, /\u2500\u2502/); // ambiguous rails stay bare
  assert.equal(els[0].removed, true);
});

test("copyCode's textContent source is byte-identical through the wrap", async () => {
  const context = await loadRendererContext(shapedHljs);
  const code = 'const \u4e2d\u6587 = "a<b>&c"; // \u2500\u2502 \u00b1 \u{1F600}';
  const html = context.marked.parse('```js\n' + code + '\n```');
  assert.equal(textContent(preHtml(html)), code);
});

// --- mounts ③④⑤: tool output, raw backend output, worker dashboard pre ---

test('tool output pre and raw backend block wrap through rendering.js', () => {
  const context = loadChatSurfaceContext();
  const out = 'step \u4e2d\u6587 done\nrail \u2500\u2502 end';
  const html = context.renderMessage({
    role: 'assistant', content: '', tools: [{ name: 'Bash', input: {}, output: out }],
  }, 'sess-1');
  assert.match(html, /<span class="wc2ch">\u4e2d<\/span>/);
  assert.match(html, /whitespace-pre-wrap break-all/);
  assert.equal((html.match(/<span class="wc2ch">/g) || []).length, 2);
  const preOut = /<pre class="mt-1[^>]*>([\s\S]*?)<\/pre>/.exec(html)[1];
  assert.equal(textContent(preOut), out);

  const raw = '<tool_call> payload \u4e2d';
  const rawHtml = context.renderMessage({ role: 'assistant', content: raw }, 'sess-1');
  assert.match(rawHtml, /<span class="wc2ch">\u4e2d<\/span>/);
  assert.match(rawHtml, /literal text/);
});

test('worker dashboard tool_result pre wraps; the attach-command block does not', () => {
  const context = loadChatSurfaceContext();
  vm.runInContext(readStatic('workers.js'), context, { filename: 'workers.js' });
  context.renderThreadEvents('t1', [
    { type: 'tool_result', tool_name: 'Bash', content: '\u4e2d\u6587 \u2500\u2502 rail', timestamp: 't' },
  ]);
  const events = context._elements.get('thread-events-t1').innerHTML;
  assert.match(events, /<span class="wc2ch">\u4e2d<\/span>/);
  assert.equal((events.match(/<span class="wc2ch">/g) || []).length, 2);
  assert.equal(textContent(/<pre[^>]*>([\s\S]*?)<\/pre>/.exec(events)[1]), '\u4e2d\u6587 \u2500\u2502 rail');

  context.renderAttachCommand('t1', { attach_command: 'tmux attach -t \u4e2d' });
  const attach = context._elements.get('thread-attach-t1').innerHTML;
  assert.match(attach, /terminal/);
  assert.doesNotMatch(attach, /wc2ch/); // attach block: Latin command line, untouched
});

// --- the session-645 incident fixture ---

// The MoEMLP data-flow box diagram whose right edge drifted in session 645,
// embedded verbatim from that session's chat_events.jsonl (the test never
// reads session directories). The author laid it out on the terminal
// convention "wide char = 2 columns, everything else = 1".
const DIAGRAM = `                              【输入张量 (Inputs)】
   x: [27334, 2048] (无 Batch 纯点云)    cond.values: [2, 1024]    cu_seqlens: [0, 20000, 27334]
          │                                      │                         │
          │  ┌───────────────────────────────────┘                         │
          │  │                                                             │
          ▼  ▼                                                             │
  ┌──────────────────────────────────────────────────────────────────┐     │
  │ 【步骤 1: Double-Router 双流打分与无 Batch 广播】                 │     │
  │                                                                  │     │
  │  Token 自身打分: x @ W_router.T [2048, 32]       → [27334, 32]   │     │
  │  样本时步打分  : cond @ W_cond.T [1024, 32]      → [2, 32]       │     │
  │                      │                                           │     │
  │                      ▼ 按 cu_seqlens.diff() 展开 ────────────────┼─────┘
  │                 repeat_interleave([20000, 7334]) → [27334, 32]   │
  │                      │                                           │
  │                      ▼ 逐元素相加 (+)                            │
  │                 综合 Logits: [27334, 32]                          │
  └──────────────────────┬───────────────────────────────────────────┘
                         │
                         ▼
  ┌──────────────────────────────────────────────────────────────────┐
  │ 【步骤 2: tk.moe_route (Sigmoid Top-8 路由决策 + 动态偏置平衡)】  │
  │                                                                  │
  │  • 输入: logits [27334, 32] + route_bias [32]                    │
  │  • 输出: 选中专家索引 indices [27334, 8]                          │
  │          归一化权重   weights [27334, 8]                          │
  └──────────────────────┬───────────────────────────────────────────┘
                         │
       ┌─────────────────┴──────────────────────────────┐
       │ (路由索引与权重)                                │ (原始 Token 直通)
       ▼                                                ▼
┌──────────────────────────────────────┐     ┌──────────────────────────────────────┐
│ 【步骤 3 - 轨道 A: 32 选 8 路由专家】 │     │ 【步骤 3 - 轨道 B: 1 个常开共享专家】 │
│ (Grouped GEMM 稀疏映射)              │     │ (全通直连，沉淀通用几何先验)          │
│                                      │     │                                      │
│ A1. w1 升维 (gate + up):             │     │ B1. w1_shared 升维 (gate + up):      │
│     [27334×8, 2048] @ w1             │     │     [27334, 2048] @ w1_shared        │
│     → [27334×8, 1536]                │     │     → [27334, 3072]                  │
│                                      │     │                                      │
│ A2. SwiGLU 门控激活:                 │     │ B2. SwiGLU 门控激活:                 │
│     1536 切两半激活后相乘             │     │     3072 切两半激活后相乘            │
│     → [27334×8, 768] (单专家隐藏宽)  │     │     → [27334, 1536] (2倍宽共享隐藏)  │
│                                      │     │                                      │
│ A3. w2 降维 (down 投影):             │     │ B3. w2_shared 降维 (down 投影):      │
│     [27334×8, 768] @ w2              │     │     [27334, 1536] @ w2_shared        │
│     → [27334×8, 2048]                │     │     → [27334, 2048]                  │
│                                      │     │                                      │
│ A4. 8 专家加权求和:                  │     │ B4. 直通输出:                        │
│     out_k * weights [27334, 8, 1]    │     │     ★ 无需路由打分，权重恒为 1.0     │
│     sum(dim=1) → out_routed:         │     │     out_shared:                      │
│     [27334, 2048]                    │     │     [27334, 2048]                    │
└──────────────────┬───────────────────┘     └──────────────────┬───────────────────┘
                   │                                            │
                   └─────────────────────┬──────────────────────┘
                                         ▼
  ┌──────────────────────────────────────────────────────────────────┐
  │ 【步骤 4: 合并前馈输出并加残差 (Residual Addition)】              │
  │                                                                  │
  │  Output = out_routed [27334, 2048]                               │
  │         + out_shared [27334, 2048]                               │
  │         + x (输入残差) [27334, 2048]                              │
  │                                                                  │
  │  最终输出张量: [27334, 2048] (完全在单卡显存内闭环，零跨卡通信)  │
  └──────────────────────────────────────────────────────────────────┘`;

// Per-line box-width sum: each bare char 1ch, each wc2ch box 2ch, tags 0.
function lineBoxWidths(html) {
  const widths = [];
  let w = 0;
  let inWideBox = false;
  const scan = (text) => {
    for (const ch of text) {
      if (ch === '\n') { widths.push(w); w = 0; }
      else if (!inWideBox) w += 1;
    }
  };
  const re = /<[^>]*>/g;
  let last = 0;
  for (let m = re.exec(html); m; m = re.exec(html)) {
    scan(html.slice(last, m.index));
    last = m.index + m[0].length;
    if (m[0] === '<span class="wc2ch">') { inWideBox = true; w += 2; }
    else if (m[0] === '</span>') inWideBox = false;
  }
  scan(html.slice(last));
  widths.push(w); // the last line carries no trailing newline
  return widths;
}

// The authored layout: the same 1ch/2ch model applied to the source itself.
function authoredLineWidths(text, isWide) {
  return text.split('\n').map((line) => {
    let w = 0;
    for (const ch of line) w += isWide(ch.codePointAt(0)) ? 2 : 1;
    return w;
  });
}

// The browser today: the CJK fallback glyph advances ~5/3 of a Latin column
// (1em vs Fira Code's 0.6em '0'), so a wide char renders 5/3 wide.
function failingLineWidths(text, isWide) {
  return text.split('\n').map((line) => {
    let w = 0;
    for (const ch of line) w += isWide(ch.codePointAt(0)) ? 5 / 3 : 1;
    return w;
  });
}

// Wide-char count of a line recovered from the authored (W/F=2) and failing
// (W/F=5/3) sums: authored = ascii + 2*wide, failing = ascii + (5/3)*wide,
// so wide = (authored - failing) * 3.
function wideCountOf(authored, failing) {
  return Math.round((authored - failing) * 3);
}

function vectorToString(name, v) {
  return name + ' [' + v.map((x) => (Number.isInteger(x) ? x : x.toFixed(2))).join(', ') + ']';
}

test('incident fixture: per-line box-width sums (1ch/2ch) render exactly the authored layout, and textContent is byte-identical', async () => {
  const context = await loadRendererContext();
  const isWide = context.wc2chIsWide;
  const html = context.marked.parse('```\n' + DIAGRAM + '\n```');
  const pre = preHtml(html);

  // The equality metric this test asserts: per-line box-width sums. The
  // fixed render's vector must equal the authored layout's vector computed
  // from the source under the same 1ch/2ch model — every line renders at
  // exactly the column count its author typed, so every edge that is
  // collinear in the source stays collinear in the browser.
  const fixed = lineBoxWidths(pre);
  const authored = authoredLineWidths(DIAGRAM, isWide);
  assert.equal(fixed.length, authored.length, 'line count preserved');
  assert.deepEqual(fixed, authored);

  // The alignment structure is preserved exactly: two lines render equal
  // widths exactly when the author laid them out equal. (The verbatim
  // diagram carries the author's own +1/+2 hand-padding on 10 of its 64
  // rows — the debug session measured the title row's extra column — so
  // "every line equal to every line" is false for the source itself and
  // would assert hand-drawing noise, not the renderer. What the fix
  // guarantees, and this test asserts, is render == authored per line.)
  for (let i = 0; i < fixed.length; i++) {
    for (let j = i + 1; j < fixed.length; j++) {
      assert.equal(
        fixed[i] === fixed[j], authored[i] === authored[j],
        'line-width equality structure changed at lines ' + i + ',' + j);
    }
  }

  // Every wide char sits in exactly one 2ch box; nothing else is boxed.
  const boxes = [...pre.matchAll(/<span class="wc2ch">([\s\S]*?)<\/span>/g)].map((m) => m[1]);
  let wideChars = 0;
  for (const ch of DIAGRAM) if (isWide(ch.codePointAt(0))) wideChars++;
  assert.equal(boxes.length, wideChars, 'one box per wide char');
  for (const box of boxes) {
    assert.ok([...box].length >= 1 && isWide(box.codePointAt(0)),
      'box leads with a wide char: ' + JSON.stringify(box));
  }

  // textContent of the pre is byte-identical to the embedded source.
  assert.equal(textContent(pre), DIAGRAM);

  // Verbose re-run: print the failing (browser today) and fixed line-width
  // vectors. The 步骤-1 title row (index 7, 12 wide chars) shows the
  // incident: 73.00 rendered columns vs 77 authored — the debug session's
  // 12 x 0.333-column (~30.7px) shortfall, plus that row's own +1 author
  // offset.
  const failing = failingLineWidths(DIAGRAM, isWide);
  if (process.env.WC2CH_VERBOSE) {
    console.log(vectorToString('fixed   (1ch/2ch boxes)   ', fixed));
    console.log(vectorToString('failing (wide char = 5/3ch)', failing));
  }
  // The failing render under-widths each wide char by exactly 1/3 column;
  // assert the per-line regain so the printed vectors stay tied to the
  // metric: fixed - failing == wideCount/3 columns on every line.
  for (let i = 0; i < authored.length; i++) {
    const wide = wideCountOf(authored[i], failing[i]);
    // 5/3 sums carry float dust; the product is integral to 1e-9.
    assert.ok(Math.abs((fixed[i] - failing[i]) * 3 - wide) < 1e-9,
      'line ' + i + ': the fix regains exactly wideCount/3 columns');
  }
});
