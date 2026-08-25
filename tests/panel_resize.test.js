// ---------------------------------------------------------------------------
// initPanelResize (utils.js) single-sources the right-edge drag-resize logic
// shared by the LaTeX and backlog panels. These tests drive the real sources
// through a vm fake DOM: saved-width restore, drag clamping, pct persistence,
// listener detach, and the LaTeX PDF interaction hooks.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

function readStatic(...parts) {
  return fs.readFileSync(path.join(__dirname, '..', 'web', 'static', 'js', ...parts), 'utf8');
}

function fakeEl(id, width) {
  const listeners = new Map();
  const classes = new Set();
  return {
    id,
    style: {},
    offsetWidth: width,
    parentElement: null,
    classList: {
      add: (c) => classes.add(c),
      remove: (c) => classes.delete(c),
      contains: (c) => classes.has(c),
    },
    addEventListener(type, fn) { listeners.set(type, fn); },
    _fire(type, event) { listeners.get(type)(event); },
  };
}

function fakeDocument() {
  const elements = new Map();
  const docListeners = new Map();
  const body = fakeEl('body', 0);
  return {
    body,
    elements,
    add(id, el) { elements.set(id, el); return el; },
    getElementById(id) { return elements.get(id) || null; },
    addEventListener(type, fn) {
      if (!docListeners.has(type)) docListeners.set(type, new Set());
      docListeners.get(type).add(fn);
    },
    removeEventListener(type, fn) { docListeners.get(type)?.delete(fn); },
    dispatch(type, event) {
      [...(docListeners.get(type) || [])].forEach((fn) => fn(event));
    },
  };
}

function loadPanelContext() {
  const document = fakeDocument();
  const store = new Map();
  const context = {
    document,
    localStorage: {
      getItem: (k) => (store.has(k) ? store.get(k) : null),
      setItem: (k, v) => store.set(k, String(v)),
    },
    _store: store,
  };
  vm.createContext(context);
  vm.runInContext(readStatic('utils.js'), context, { filename: 'utils.js' });
  vm.runInContext(readStatic('latex-panel.js'), context, { filename: 'latex-panel.js' });
  vm.runInContext(readStatic('backlog-panel.js'), context, { filename: 'backlog-panel.js' });
  return context;
}

// A 1000px container holding a 300px panel plus its resize handle.
function panelFixture(document, handleId, panelId) {
  const panel = document.add(panelId, fakeEl(panelId, 300));
  panel.parentElement = fakeEl('container', 1000);
  return { panel, handle: document.add(handleId, fakeEl(handleId, 0)) };
}

test('backlog resize: restores saved width, resizes with clamp, persists pct, detaches', () => {
  const ctx = loadPanelContext();
  const { panel, handle } = panelFixture(ctx.document, 'backlog-resize-handle', 'backlog-panel');
  ctx._store.set('backlog-panel-pct', '33');

  ctx.initBacklogResize();
  assert.equal(panel.style.width, '33%');

  let prevented = false;
  handle._fire('mousedown', { preventDefault: () => { prevented = true; }, clientX: 500 });
  assert.equal(prevented, true);
  assert.equal(handle.classList.contains('active'), true);
  assert.equal(ctx.document.body.classList.contains('resizing'), true);

  ctx.document.dispatch('mousemove', { clientX: 450 }); // drag left 50px -> 350px
  assert.equal(panel.style.width, '350px');
  ctx.document.dispatch('mousemove', { clientX: 0 }); // 800px is the 80% ceiling
  assert.equal(panel.style.width, '800px');
  ctx.document.dispatch('mousemove', { clientX: 5000 }); // 200px is the 20% floor
  assert.equal(panel.style.width, '200px');

  panel.offsetWidth = 200; // the browser's layout result for the last mousemove
  ctx.document.dispatch('mouseup');
  assert.equal(handle.classList.contains('active'), false);
  assert.equal(ctx.document.body.classList.contains('resizing'), false);
  assert.equal(ctx._store.get('backlog-panel-pct'), '20.0');
  assert.equal(panel.style.width, '20.0%');

  ctx.document.dispatch('mousemove', { clientX: 100 }); // listeners detached on mouseup
  assert.equal(panel.style.width, '20.0%');
});

test('latex resize: disables PDF pointer events during drag, reloads PDF on release', () => {
  const ctx = loadPanelContext();
  const { panel, handle } = panelFixture(ctx.document, 'latex-resize-handle', 'latex-panel');
  const pdf = ctx.document.add('latex-pdf-canvas-container', fakeEl('pdf', 0));
  const calls = [];
  ctx.loadLatexPdf = (...args) => { calls.push(args); };
  vm.runInContext("latexView = 'pdf'", ctx);

  ctx.initLatexResize();
  handle._fire('mousedown', { preventDefault: () => {}, clientX: 500 });
  assert.equal(pdf.style.pointerEvents, 'none');
  ctx.document.dispatch('mousemove', { clientX: 400 });
  assert.equal(panel.style.width, '400px');
  panel.offsetWidth = 400;
  ctx.document.dispatch('mouseup');
  assert.equal(pdf.style.pointerEvents, '');
  assert.equal(ctx._store.get('latex-panel-pct'), '40.0');
  assert.deepEqual(calls, [[true]]); // pdf.js re-renders against the final width
});

test('latex resize in tex view: flags pdfNeedsReload instead of reloading', () => {
  const ctx = loadPanelContext();
  const { panel, handle } = panelFixture(ctx.document, 'latex-resize-handle', 'latex-panel');
  ctx.document.add('latex-pdf-canvas-container', fakeEl('pdf', 0));
  const calls = [];
  ctx.loadLatexPdf = (...args) => { calls.push(args); };
  vm.runInContext("latexView = 'tex'", ctx);

  ctx.initLatexResize();
  handle._fire('mousedown', { preventDefault: () => {}, clientX: 500 });
  panel.offsetWidth = 300;
  ctx.document.dispatch('mouseup');
  assert.deepEqual(calls, []);
  assert.equal(vm.runInContext('pdfNeedsReload', ctx), true);
});

test('initPanelResize is a no-op when the handle or panel is absent', () => {
  const ctx = loadPanelContext();
  ctx.initBacklogResize(); // neither handle nor panel exists
  ctx.document.add('backlog-resize-handle', fakeEl('h', 0)); // handle without panel
  ctx.initBacklogResize();
});
