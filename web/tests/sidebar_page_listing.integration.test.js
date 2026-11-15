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
function buildContext({initial, full}) {
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
    return {ok: true, json: async () => full};
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

test('a listing the page carries whole paints every row and no toggle', () => {
  const rows = previewRows();
  const {context, nav} = buildContext({initial: sessionColumns(rows), full: sessionColumns(rows)});

  context.restoreSidebarFromUrl();

  const html = nav.innerHTML;
  assert.deepEqual(rowIds(html), ['row-1', 'row-2', 'row-3', 'row-4', 'row-5']);
  assert.doesNotMatch(html, /session-group-limit-toggle/);
});
