// ---------------------------------------------------------------------------
// The page's sidebar column encodings (filters.js): the page carries the
// sidebar rows as one array per field and declares, under `$encoded`, the two
// columns whose cells are not their field's value. A `uuid-b64` column holds
// the session ids' 16 bytes in base64url — as one string in row order, or as one
// cell per row when a home's ids are not all canonical UUIDs (a `~` id travels
// as its text) — and a `delta` cell holds the first value as it stands and every
// later value as its difference from the row before. Expansion restores the row
// objects the sidebar reads.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');

const {buildSidebarIndicatorContext, makeSessionMeta} = require('./session_context_stub');

const UUID = 'e870f19c-96c6-40e7-8da7-71f02b2622e3';
const UUID_CELL = Buffer.from(UUID.replace(/-/g, ''), 'hex').toString('base64url');

const flush = () => new Promise((resolve) => setImmediate(resolve));

const rowIds = (html) => [...html.matchAll(/id="session-([^"]+)"/g)].map((match) => match[1]);

test('the declared encodings expand to the row values the sidebar reads', () => {
  const {context} = buildSidebarIndicatorContext(['x']);
  const rows = context.Sidebar.expandSessionColumns({
    $encoded: {id: 'uuid-b64', updated_at: 'delta'},
    backend: ['mock-backend', 'mock-backend'],
    id: [UUID_CELL, '~imported-chat'],
    name: ['first', 'second'],
    profile: ['manager', null],
    updated_at: [29566074, 1],
  });

  // The rows are built inside the harness context's realm, so compare their JSON form.
  assert.deepEqual(JSON.parse(JSON.stringify(rows)), [
    {backend: 'mock-backend', id: UUID, name: 'first', profile: 'manager', updated_at: 29566074},
    {backend: 'mock-backend', id: 'imported-chat', name: 'second', updated_at: 29566075},
  ]);
});

test('a column the page declares no encoding for expands as its value', () => {
  const {context} = buildSidebarIndicatorContext(['x']);
  const rows = context.Sidebar.expandSessionColumns({
    id: ['plain-id'],
    updated_at: ['2026-04-02T04:00:00Z'],
  });

  assert.deepEqual(JSON.parse(JSON.stringify(rows)), [{id: 'plain-id', updated_at: '2026-04-02T04:00:00Z'}]);
});

test('a string id column expands every 16-byte id it carries in row order', () => {
  const {context} = buildSidebarIndicatorContext(['x']);
  const second = '994a3ec8-c930-4c2c-9009-5b75d17b823d';
  const blob = Buffer.concat([
    Buffer.from(UUID.replace(/-/g, ''), 'hex'),
    Buffer.from(second.replace(/-/g, ''), 'hex'),
  ]).toString('base64url');
  const rows = context.Sidebar.expandSessionColumns({
    $encoded: {id: 'uuid-b64', updated_at: 'delta'},
    id: blob,
    name: ['first', 'second'],
    updated_at: [29566074, 1],
  });

  assert.deepEqual(JSON.parse(JSON.stringify(rows)), [
    {id: UUID, name: 'first', updated_at: 29566074},
    {id: second, name: 'second', updated_at: 29566075},
  ]);
});

test('an encoded id of another length fails instead of entering the sidebar', () => {
  const {context} = buildSidebarIndicatorContext(['x']);
  assert.throws(
    () => context.Sidebar.expandSessionColumns({$encoded: {id: 'uuid-b64'}, id: ['not-sixteen-bytes']}),
    /Encoded session id/);
  assert.throws(
    () => context.Sidebar.expandSessionColumns({$encoded: {id: 'uuid-b64'}, name: ['a', 'b'], id: UUID_CELL}),
    /Encoded session id blob/);
});

test('the Workspace fetch asks for the compact listing and paints its rows', async () => {
  const {context, nav} = buildSidebarIndicatorContext(['x']);
  const urls = [];
  context.fetch = async (url) => {
    urls.push(url);
    return {
      ok: true,
      json: async () => ({
        $encoded: {id: 'uuid-b64', updated_at: 'delta'},
        backend: ['mock-backend'],
        id: UUID_CELL,
        name: ['first'],
        profile: ['manager'],
        updated_at: [29566074],
      }),
    };
  };

  context.Sidebar.switchSidebarFilter('all');
  await flush();
  await flush();

  assert.deepEqual(urls, ['/api/sessions/?compact=1']);
  assert.deepEqual(rowIds(nav.innerHTML), [UUID]);
});

test('a filter that answers with a row array paints the same list', async () => {
  const {context, nav} = buildSidebarIndicatorContext(['x']);
  context.fetch = async () => ({ok: true, json: async () => [makeSessionMeta(UUID)]});

  context.Sidebar.switchSidebarFilter('all');
  await flush();
  await flush();

  assert.deepEqual(rowIds(nav.innerHTML), [UUID]);
});
