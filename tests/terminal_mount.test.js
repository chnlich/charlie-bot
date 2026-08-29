const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const { readStatic } = require('./read_static');

const TERMINAL_MOUNT_JS = readStatic('terminal_mount.js');

function loadHelpers() {
  const constructed = [];
  function FakeTerminal(options) {
    this.options = options;
    this.addons = [];
    constructed.push(this);
  }
  FakeTerminal.prototype.loadAddon = function(addon) { this.addons.push(addon); };
  FakeTerminal.prototype.open = function(container) { this.container = container; };
  FakeTerminal.prototype.focus = function() { this.focused = true; };
  function FakeFitAddon() {}
  const context = {
    Terminal: FakeTerminal,
    FitAddon: {FitAddon: FakeFitAddon},
    wireTerminalClipboard(term) { term.clipboardWired = true; },
  };
  context.globalThis = context;
  vm.createContext(context);
  vm.runInContext(TERMINAL_MOUNT_JS, context, {filename: 'terminal_mount.js'});
  return {context, constructed};
}

function makeContainer() {
  const listeners = [];
  return {
    listeners,
    addEventListener(type, handler, opts) { listeners.push({type, handler, opts}); },
  };
}

test('createTerminal mounts one Terminal with the shared options', () => {
  const {context, constructed} = loadHelpers();
  const container = {};
  const {term, fitAddon} = context.createTerminal(container);
  assert.equal(constructed.length, 1);
  assert.equal(term, constructed[0]);
  assert.ok(fitAddon instanceof context.FitAddon.FitAddon);
  // JSON round-trip: deepStrictEqual rejects the vm realm's Object.prototype.
  assert.deepEqual(JSON.parse(JSON.stringify(term.options)), {
    cursorBlink: true,
    fontSize: 13,
    fontFamily: '"Fira Code", ui-monospace, SFMono-Regular, Menlo, Consolas, monospace',
    theme: {background: '#000000', foreground: '#e2e8f0'},
    scrollback: 50000,
    convertEol: false,
    allowProposedApi: true,
  });
  assert.equal(term.addons.length, 1);
  assert.equal(term.addons[0], fitAddon);
  assert.equal(term.container, container);
  assert.equal(term.clipboardWired, true);
  assert.equal(term.focused, true);
});

test('wireTerminalTouchScroll wires the touch lifecycle as passive listeners', () => {
  const {context} = loadHelpers();
  const container = makeContainer();
  context.wireTerminalTouchScroll(container, {scrollLines() {}});
  assert.deepEqual(
    container.listeners.map(l => l.type),
    ['touchstart', 'touchmove', 'touchend', 'touchcancel']
  );
  for (const l of container.listeners) assert.equal(l.opts.passive, true);
});

test('wireTerminalTouchScroll merges caller listener options', () => {
  const {context} = loadHelpers();
  const container = makeContainer();
  const signal = {};
  context.wireTerminalTouchScroll(container, {scrollLines() {}}, {signal});
  for (const l of container.listeners) {
    assert.equal(l.opts.passive, true);
    assert.equal(l.opts.signal, signal);
  }
});

test('touch drag scrolls by rounded tenths of the y delta', () => {
  const {context} = loadHelpers();
  const container = makeContainer();
  const scrolled = [];
  const term = {scrollLines(n) { scrolled.push(n); }};
  context.wireTerminalTouchScroll(container, term);
  const byType = Object.fromEntries(container.listeners.map(l => [l.type, l.handler]));
  byType.touchmove({changedTouches: [{screenY: 100}]});
  assert.deepEqual(scrolled, []);
  byType.touchstart({touches: [{screenY: 100}]});
  byType.touchmove({changedTouches: [{screenY: 127}]});
  byType.touchmove({changedTouches: [{screenY: 120}]});
  assert.deepEqual(scrolled, [-3, 1]);
  byType.touchend({});
  byType.touchmove({changedTouches: [{screenY: 10}]});
  assert.deepEqual(scrolled, [-3, 1]);
});

test('fitTerminalAndSendResize fits, then sends the fitted grid', () => {
  const {context} = loadHelpers();
  const fits = [];
  const sent = [];
  const fitAddon = {fit() { fits.push(1); }};
  const term = {cols: 120, rows: 30};
  context.fitTerminalAndSendResize(term, fitAddon, (cols, rows) => sent.push([cols, rows]));
  assert.equal(fits.length, 1);
  assert.deepEqual(sent, [[120, 30]]);
});

test('fitTerminalAndSendResize skips the send on a zero-sized grid', () => {
  const {context} = loadHelpers();
  const fits = [];
  const sent = [];
  const fitAddon = {fit() { fits.push(1); }};
  context.fitTerminalAndSendResize({cols: 0, rows: 0}, fitAddon, (cols, rows) => sent.push([cols, rows]));
  assert.equal(fits.length, 1);
  assert.deepEqual(sent, []);
});

test('fitTerminalAndSendResize swallows a fit failure into one warning', () => {
  const {context} = loadHelpers();
  const warnings = [];
  context.console = {warn(...args) { warnings.push(args); }};
  const sent = [];
  const fitAddon = {fit() { throw new Error('boom'); }};
  const term = {cols: 80, rows: 24};
  context.fitTerminalAndSendResize(term, fitAddon, (cols, rows) => sent.push([cols, rows]));
  assert.equal(warnings.length, 1);
  assert.deepEqual(sent, []);
});

test('scheduleAfterTerminalPaint defers across two animation frames', () => {
  const {context} = loadHelpers();
  const queue = [];
  context.requestAnimationFrame = fn => queue.push(fn);
  let ran = 0;
  context.scheduleAfterTerminalPaint(() => { ran += 1; });
  assert.equal(queue.length, 1);
  assert.equal(ran, 0);
  queue.shift()();
  assert.equal(queue.length, 1);
  assert.equal(ran, 0);
  queue.shift()();
  assert.equal(ran, 1);
});
