const assert = require('node:assert/strict');
const test = require('node:test');

const { loadSidebarStatusContext } = require('./sidebar_status_context_stub');
// ---------------------------------------------------------------------------
// updateSidebarHighlight standalone: sidebar/status.js through the shared stub
// loader. The session list's own id starts with the row-id prefix, so the
// highlight must move only the rows and never the nav.
// ---------------------------------------------------------------------------

function element(id, tagName, classes = []) {
  const set = new Set(classes);
  return {
    id,
    tagName,
    classList: {
      add: (...names) => names.forEach((name) => set.add(name)),
      remove: (...names) => names.forEach((name) => set.delete(name)),
      contains: (name) => set.has(name),
      values: () => [...set],
    },
  };
}

function buildContext() {
  const nav = element('session-list', 'NAV');
  const first = element('session-a', 'A', ['session-row', 'bg-blue-600/20', 'text-blue-300']);
  const second = element('session-b', 'A', ['session-row', 'hover:bg-slate-700/50', 'text-slate-300']);
  const all = [nav, first, second];
  // The two selectors status.js has used: the bare id prefix, which also
  // matches the nav, and the scoped session row.
  const matches = (el, selector) => {
    if (selector === '[id^="session-"]') return String(el.id).startsWith('session-');
    if (selector === '#session-list .session-row') return el.classList.contains('session-row');
    throw new Error('unexpected selector ' + selector);
  };
  const context = loadSidebarStatusContext({
    document: {
      querySelectorAll: (selector) => all.filter((el) => matches(el, selector)),
      getElementById: (id) => all.find((el) => el.id === id) || null,
    },
  });
  return { context, nav, first, second };
}

test('the highlight moves to the new session row and leaves the old one inactive', () => {
  const { context, first, second } = buildContext();

  context.updateSidebarHighlight('b');

  assert.ok(second.classList.contains('bg-blue-600/20'), 'the new row takes the active tint');
  assert.ok(second.classList.contains('text-blue-300'));
  assert.ok(!second.classList.contains('hover:bg-slate-700/50'), 'the active row drops its hover tint');
  assert.ok(!first.classList.contains('bg-blue-600/20'), 'the old row loses the active tint');
  assert.ok(!first.classList.contains('text-blue-300'), 'the old row loses the active text tone');
  assert.ok(first.classList.contains('hover:bg-slate-700/50'), 'the old row takes the inactive hover tint');
  assert.ok(first.classList.contains('text-slate-300'));
});

test('the session list never takes the row classes', () => {
  const { context, nav } = buildContext();

  context.updateSidebarHighlight('b');

  assert.ok(!nav.classList.contains('bg-blue-600/20'), 'the list must not take the active tint');
  assert.ok(!nav.classList.contains('hover:bg-slate-700/50'),
    'the hover tint on the nav paints the whole list on a row hover');
  assert.ok(!nav.classList.contains('text-slate-300'));
});
