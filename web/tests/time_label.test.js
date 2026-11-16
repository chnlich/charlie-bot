// ---------------------------------------------------------------------------
// formatTimeLabel (utils.js): the one label a reader's instants print in the sidebar rows, the status panel, the
// cron list and the chat bubbles (chat/shared.js formatBubbleTime). A home whose sessions span more than a year
// needs the year on the instants outside the current one; without it a session from last October and one from this
// March read alike.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const UTILS_JS = fs.readFileSync(path.join(__dirname, '..', 'static', 'js', 'utils.js'), 'utf8');

function loadUtils() {
  const context = {
    STATIC_ASSET_VERSION: 'v1',
    console: {error() {}},
    document: {createElement: () => ({}), head: {appendChild() {}}},
    // markdown-renderer.js defines this before utils.js runs on the page.
    registerChatMathLoader() {},
  };
  context.globalThis = context;
  vm.createContext(context);
  vm.runInContext(UTILS_JS, context, {filename: 'utils.js'});
  return context;
}

// A local instant in July, so no time zone's offset can move it into another year. toISOString is UTC, and the
// label formats back in this zone, so the day and year come back unchanged.
function localInstant(year) {
  return new Date(year, 6, 4, 10, 7).toISOString();
}

test('an instant outside the current year carries its year', () => {
  const current = new Date().getFullYear();
  const label = loadUtils().formatTimeLabel(localInstant(current - 1));
  assert.ok(label.includes('Jul 4'), `the day survives: ${label}`);
  assert.ok(label.includes(String(current - 1)), `the year is named: ${label}`);
  assert.ok(!label.includes(String(current)), `the current year is not named: ${label}`);
});

test('an instant in the current year keeps the shorter label', () => {
  const current = new Date().getFullYear();
  const label = loadUtils().formatTimeLabel(localInstant(current));
  assert.ok(label.includes('Jul 4'), `the day is named: ${label}`);
  assert.ok(!label.includes(String(current)), `no year on a current-year label: ${label}`);
});

test('the sidebar name and the label print one instant alike', () => {
  const context = loadUtils();
  const current = new Date().getFullYear();
  for (const year of [current, current - 1]) {
    const iso = localInstant(year);
    assert.equal(context.relativeTime(iso), context.formatTimeLabel(iso));
  }
});

test('an absent instant prints nothing', () => {
  const context = loadUtils();
  assert.equal(context.formatTimeLabel(''), '');
  assert.equal(context.formatTimeLabel(null), '');
});
