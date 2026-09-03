const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const { readStatic } = require('./read_static');
const { loadSidebarStatusContext } = require('./sidebar_status_context_stub');
const { loadSidebarWorkersContext } = require('./sidebar_workers_context_stub');

// ---------------------------------------------------------------------------
// A DOM node that counts every write to `class`/`title`, mirroring the real
// browser: `classList.add`/`remove`/`toggle` count as a write on every call,
// not only when the resulting string differs, because Chromium still queues
// a mutation record for a redundant add/toggle. This is what makes a removed
// write-guard show up as a nonzero count below. getAttribute/setAttribute on
// 'class'/'title' are counted the same way: SVG nodes take class updates
// through the attribute because SVGElement.className is a read-only
// SVGAnimatedString.
// ---------------------------------------------------------------------------
function makeCountingElement(initialClasses, initialTitle) {
  const counts = { classWrites: 0, titleWrites: 0 };
  const classSet = new Set(initialClasses || []);
  let titleValue = initialTitle || '';
  let classNameValue = Array.from(classSet).join(' ');

  const node = {
    dataset: {},
    counts,
    get className() { return classNameValue; },
    set className(v) {
      counts.classWrites++;
      classNameValue = String(v);
      classSet.clear();
      classNameValue.split(/\s+/).filter(Boolean).forEach((c) => classSet.add(c));
    },
    get title() { return titleValue; },
    set title(v) { counts.titleWrites++; titleValue = v; },
    getAttribute(name) {
      if (name === 'class') return classNameValue;
      if (name === 'title') return titleValue;
      return null;
    },
    setAttribute(name, v) {
      if (name === 'class') {
        counts.classWrites++;
        classNameValue = String(v);
        classSet.clear();
        classNameValue.split(/\s+/).filter(Boolean).forEach((c) => classSet.add(c));
      } else if (name === 'title') {
        counts.titleWrites++;
        titleValue = v;
      }
    },
    classList: {
      contains(c) { return classSet.has(c); },
      add(...cs) {
        counts.classWrites++;
        cs.forEach((c) => classSet.add(c));
        classNameValue = Array.from(classSet).join(' ');
      },
      remove(...cs) {
        counts.classWrites++;
        cs.forEach((c) => classSet.delete(c));
        classNameValue = Array.from(classSet).join(' ');
      },
      toggle(c, force) {
        counts.classWrites++;
        if (force === undefined) {
          if (classSet.has(c)) classSet.delete(c); else classSet.add(c);
        } else if (force) {
          classSet.add(c);
        } else {
          classSet.delete(c);
        }
        classNameValue = Array.from(classSet).join(' ');
        return classSet.has(c);
      },
    },
    querySelector: () => null,
  };
  return node;
}

// ---------------------------------------------------------------------------
// scroll.js: showScrollToBottom / hideScrollToBottom
// ---------------------------------------------------------------------------
function loadScrollContext(elements) {
  const context = {
    document: {
      getElementById: (id) => elements.get(id) || null,
      addEventListener: () => {},
    },
    console: { error: () => {}, log: () => {} },
  };
  vm.createContext(context);
  vm.runInContext(readStatic('chat/namespace.js'), context, { filename: 'chat/namespace.js' });
  vm.runInContext(readStatic('chat/scroll.js'), context, { filename: 'chat/scroll.js' });
  return context;
}

test('showScrollToBottom writes nothing when the button is already visible', () => {
  const btn = makeCountingElement([], '');
  const context = loadScrollContext(new Map([['scroll-to-bottom', btn]]));
  context.showScrollToBottom();
  assert.equal(btn.counts.classWrites, 0);
  assert.equal(btn.classList.contains('hidden'), false);
});

test('showScrollToBottom writes exactly once when the button needs to become visible', () => {
  const btn = makeCountingElement(['hidden'], '');
  const context = loadScrollContext(new Map([['scroll-to-bottom', btn]]));
  context.showScrollToBottom();
  assert.equal(btn.counts.classWrites, 1);
  assert.equal(btn.classList.contains('hidden'), false);
});

test('hideScrollToBottom writes nothing when the button is already hidden', () => {
  const btn = makeCountingElement(['hidden'], '');
  const context = loadScrollContext(new Map([['scroll-to-bottom', btn]]));
  context.hideScrollToBottom();
  assert.equal(btn.counts.classWrites, 0);
  assert.equal(btn.classList.contains('hidden'), true);
});

test('hideScrollToBottom writes exactly once when the button needs to become hidden', () => {
  const btn = makeCountingElement([], '');
  const context = loadScrollContext(new Map([['scroll-to-bottom', btn]]));
  context.hideScrollToBottom();
  assert.equal(btn.counts.classWrites, 1);
  assert.equal(btn.classList.contains('hidden'), true);
});

// ---------------------------------------------------------------------------
// sidebar/status.js: refreshTuiDots
// ---------------------------------------------------------------------------
test('refreshTuiDots writes nothing when every dot already matches its live status', () => {
  const dot = makeCountingElement(['tui-status-dot', 'w-2', 'h-2', 'rounded-full', 'flex-shrink-0', 'running'], 'Claude idle');
  dot.dataset.sessionId = 's1';
  const context = loadSidebarStatusContext({
    document: {
      getElementById: () => null,
      querySelectorAll: (sel) => (sel === '.tui-status-dot[data-session-id]' ? [dot] : []),
    },
  });
  context.TuiStatusMap = { s1: { running: true, busy: false } };
  context.refreshTuiDots();
  assert.equal(dot.counts.classWrites, 0);
  assert.equal(dot.counts.titleWrites, 0);
});

test('refreshTuiDots writes class and title when the dot status actually changed', () => {
  const dot = makeCountingElement(['tui-status-dot', 'w-2', 'h-2', 'rounded-full', 'flex-shrink-0'], '');
  dot.dataset.sessionId = 's1';
  const context = loadSidebarStatusContext({
    document: {
      getElementById: () => null,
      querySelectorAll: (sel) => (sel === '.tui-status-dot[data-session-id]' ? [dot] : []),
    },
  });
  context.TuiStatusMap = { s1: { running: true, busy: true } };
  context.refreshTuiDots();
  assert.ok(dot.counts.classWrites > 0);
  assert.ok(dot.counts.titleWrites > 0);
  assert.equal(dot.classList.contains('running'), true);
  assert.equal(dot.classList.contains('busy'), true);
  assert.equal(dot.title, 'Claude busy');
});

// ---------------------------------------------------------------------------
// sidebar/workers.js: updateTriggerStatus
// ---------------------------------------------------------------------------
function loadWorkersContext(elements) {
  return loadSidebarWorkersContext({
    document: {
      getElementById: (id) => elements.get(id) || null,
    },
  });
}

test('updateTriggerStatus writes nothing to icon.className when the status is unchanged', () => {
  const icon = makeCountingElement(['w-4', 'h-4', 'flex-shrink-0', 'text-amber-400'], '');
  const context = loadWorkersContext(new Map([['trigger-dot-t1', icon]]));
  context.updateTriggerStatus('t1', 'pending');
  assert.equal(icon.counts.classWrites, 0);
});

test('updateTriggerStatus writes icon.className exactly once when the status actually changed', () => {
  const icon = makeCountingElement(['w-4', 'h-4', 'flex-shrink-0', 'text-slate-500'], '');
  const context = loadWorkersContext(new Map([['trigger-dot-t1', icon]]));
  context.updateTriggerStatus('t1', 'pending');
  assert.equal(icon.counts.classWrites, 1);
  assert.equal(icon.classList.contains('text-amber-400'), true);
});
