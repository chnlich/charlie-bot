// ---------------------------------------------------------------------------
// The sidebar rows the page carries (filters.js, groups.js): the page sends
// the rows the first screen paints, with each group's whole row count under
// `$screen`. A group past the preview paints "Show all", and that click fetches
// the full listing from the listing route and repaints every row.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');

const {
  baseSessionContext,
  buildSidebarFilterElements,
  createChatSidebarContext,
  inlinePageTimers,
  makeSessionMeta,
  sessionColumns,
} = require('./session_context_stub');
const { createElement } = require('./dom_element_stub');

const flush = () => new Promise((resolve) => setImmediate(resolve));

// One chat page whose listing carries `initial`, with the listing route answering `full`.
function buildContext({initial, full, stored = false}) {
  const fetchRequests = [];
  const nav = createElement();
  const elements = new Map([
    ['session-list', nav],
    ['sidebar-filter-pills', createElement()],
    ...buildSidebarFilterElements(),
  ]);
  const {context} = baseSessionContext({elements});
  context.SESSION_ID = 'row-5';
  context.INITIAL_SESSIONS = initial;
  context.INITIAL_LOAD_ERRORS = [];
  context.location.search = '';
  inlinePageTimers(context);
  context.document.getElementById = (id) => elements.get(id) || null;
  context.document.querySelectorAll = () => [];
  context.document.querySelector = () => null;
  context.fetch = async (url) => {
    fetchRequests.push(url);
    return {
      ok: true,
      headers: {get: (name) => (stored && name === 'X-CharlieBot-Stored-At' ? 'stored' : null)},
      json: async () => full,
    };
  };
  createChatSidebarContext(context);
  return {context, nav, fetchRequests};
}

const rowIds = (html) => [...html.matchAll(/id="session-([^"]+)"/g)].map((match) => match[1]);

const previewRows = () => ['row-1', 'row-2', 'row-3', 'row-4', 'row-5'].map((id) => {
  return makeSessionMeta(id);
});

test('the page paints the rows it carries and Show all fetches the rest', async () => {
  const preview = previewRows();
  const full = preview.concat([makeSessionMeta('row-6'), makeSessionMeta('row-7'), makeSessionMeta('row-8')]);
  const initial = {$screen: {partial: true, counts: {'': 8}}, ...sessionColumns(preview)};
  const {context, nav, fetchRequests} = buildContext({initial, full: sessionColumns(full)});

  context.restoreSidebarFromUrl();

  const first = nav.innerHTML;
  assert.deepEqual(rowIds(first), ['row-1', 'row-2', 'row-3', 'row-4', 'row-5']);
  assert.match(first, /text-slate-400 ml-auto">8</, 'the group header counts every row of the group');
  assert.match(first, /session-group-limit-toggle/);
  assert.match(first, />Show all</);

  context.toggleSessionGroupLimit('');

  await flush();
  await flush();

  assert.deepEqual(fetchRequests, ['/api/sessions/?compact=1']);
  const after = nav.innerHTML;
  assert.deepEqual(rowIds(after), ['row-1', 'row-2', 'row-3', 'row-4', 'row-5', 'row-6', 'row-7', 'row-8']);
  assert.match(after, /text-slate-400 ml-auto">8</);
  assert.match(after, />Show less</, 'the group stays expanded after the full listing paints');
});

test('the preview paints the rows the list height holds and hides the rest behind Show all', () => {
  const rows = Array.from({length: 20}, (_, index) => makeSessionMeta(`row-${index + 1}`));
  const initial = {$screen: {partial: true, counts: {'': 20}}, ...sessionColumns(rows)};
  const {context, nav} = buildContext({initial, full: sessionColumns(rows)});
  // The sidebar's list measures 600 px: ten 52 px rows once the group's header and the Show all line take theirs.
  nav.clientHeight = 600;

  context.restoreSidebarFromUrl();

  const html = nav.innerHTML;
  const toggleAt = html.indexOf('session-group-limit-toggle');
  assert.ok(toggleAt > 0, 'the rows past the preview hide behind Show all');
  assert.deepEqual(rowIds(html.slice(0, toggleAt)), rows.slice(0, 10).map((row) => row.id));
  assert.match(html.slice(toggleAt), /session-group-limit-extra/, 'the rows past the preview stay hidden');
  assert.equal(html.split('session-group-limit-toggle').length - 1, 1, 'one Show all control stands before its rows');
});

test('the open session past the preview keeps its row and Show all inside the list height', () => {
  const rows = Array.from({length: 20}, (_, index) => makeSessionMeta(`row-${index + 1}`));
  const initial = {$screen: {partial: true, counts: {'': 20}}, ...sessionColumns(rows)};
  const {context, nav} = buildContext({initial, full: sessionColumns(rows)});
  // The same 600 px list: ten 52 px rows beside the group's header and the Show all line.
  nav.clientHeight = 600;
  context.SESSION_ID = 'row-20';

  context.restoreSidebarFromUrl();

  const html = nav.innerHTML;
  const toggleAt = html.indexOf('session-group-limit-toggle');
  assert.ok(toggleAt > 0, 'the rows past the preview hide behind Show all');
  const beforeToggle = rowIds(html.slice(0, toggleAt));
  assert.equal(beforeToggle.length, 10, 'the open row takes the last row the height holds from the preview');
  assert.deepEqual(beforeToggle.slice(0, 9), rows.slice(0, 9).map((row) => row.id));
  assert.equal(beforeToggle[9], 'row-20', 'the open row joins the preview, before the control that hides the rest');
});

test('two groups share the list height instead of each taking a screen of it', () => {
  const first = Array.from({length: 10}, (_, index) => makeSessionMeta(`g1-${index + 1}`, {group: 'g1'}));
  const second = Array.from({length: 10}, (_, index) => makeSessionMeta(`g2-${index + 1}`, {group: 'g2'}));
  const rows = first.concat(second);
  const initial = {$screen: {partial: true, counts: {g1: 10, g2: 10}}, ...sessionColumns(rows)};
  const {context, nav} = buildContext({initial, full: sessionColumns(rows)});
  // 600 px holds nine rows across both groups, four each under the five-row floor, so five each.
  nav.clientHeight = 600;

  context.restoreSidebarFromUrl();

  const html = nav.innerHTML;
  const firstToggle = html.indexOf('session-group-limit-toggle');
  assert.ok(firstToggle > 0, 'each group hides the rows past the preview');
  assert.ok(html.indexOf('id="session-g1-6"') > firstToggle, 'the first group keeps five rows before its toggle');
  const secondToggle = html.indexOf('session-group-limit-toggle', firstToggle + 1);
  assert.ok(secondToggle > 0, 'the second group carries its own toggle');
  const secondGroupRows = html.slice(html.indexOf('id="session-g2-1"'), secondToggle);
  assert.equal(rowIds(secondGroupRows).length, 5, 'the second group takes the same limit');
test('a stored page listing keeps its first-screen counts and Show all toggle', async () => {
  const preview = previewRows();
  const firstScreen = {$screen: {partial: true, counts: {'': 8}}, ...sessionColumns(preview)};
  const {context, nav, fetchRequests} = buildContext({initial: null, full: firstScreen, stored: true});
  context.INITIAL_SESSIONS = null;

  context.restoreSidebarFromUrl();
  await flush();
  await flush();

  const html = nav.innerHTML;
  assert.deepEqual(fetchRequests, ['/api/sessions/?compact=1']);
  assert.deepEqual(rowIds(html), ['row-1', 'row-2', 'row-3', 'row-4', 'row-5']);
  assert.match(html, /text-slate-400 ml-auto">8</, 'the stored first screen carries the whole group count');
  assert.match(html, /session-group-limit-toggle/);
  assert.match(html, />Show all</);
});

test('a listing the page carries whole paints every row and no toggle', () => {
  const rows = previewRows();
  const {context, nav} = buildContext({initial: sessionColumns(rows), full: sessionColumns(rows)});

  context.restoreSidebarFromUrl();

  const html = nav.innerHTML;
  assert.deepEqual(rowIds(html), ['row-1', 'row-2', 'row-3', 'row-4', 'row-5']);
  assert.doesNotMatch(html, /session-group-limit-toggle/);
});
