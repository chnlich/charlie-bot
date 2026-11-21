const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const { readStatic, runIconModule } = require('./read_static');

const BACKEND_OPTIONS = {
  'claude-opus-5': 'CC · Opus',
  'aigw-gemini-flash': 'CC · aigw Gemini Flash',
};
const BACKEND_POOLS = [
  { id: 'human-communication', label: 'Human communication' },
  { id: 'explain', label: 'Explain' },
];

// chat/shared.js is the one module that defines sessionModelLabel; the page loads it after chat/namespace.js.
function loadSharedLabel() {
  const context = { console, BACKEND_OPTIONS, BACKEND_POOLS };
  context.globalThis = context;
  vm.createContext(context);
  runIconModule(context);
  vm.runInContext(readStatic('chat/namespace.js'), context, { filename: 'chat/namespace.js' });
  vm.runInContext(readStatic('chat/shared.js'), context, { filename: 'chat/shared.js' });
  return context;
}

const { sessionModelLabel } = loadSharedLabel();

// Each row: the arguments the surfaces pass (poolMember is the server's already-filtered value), the full label and
// the compact label. The rows are the approved function table of the pool member display plan.
const ROWS = [
  [
    'a pool member with its model reads pool → member · model',
    ['human-communication', 'claude-opus-5', 'claude-opus-5-5'],
    'Human communication → CC · Opus · claude-opus-5-5',
    'Human communication → Opus',
  ],
  [
    'a pool member without a model reads pool → member',
    ['human-communication', 'claude-opus-5', null],
    'Human communication → CC · Opus',
    'Human communication → Opus',
  ],
  [
    'a pool with no member reads the pool label alone',
    ['human-communication', null, null],
    'Human communication',
    'Human communication',
  ],
  [
    'a single-option session reads its option label',
    ['claude-opus-5', null, null],
    'CC · Opus',
    'Opus',
  ],
  [
    'a single-option session switched from a pool reads the same option label',
    ['claude-opus-5', null, null],
    'CC · Opus',
    'Opus',
  ],
  [
    'a pool whose recorded member the server filtered out reads the pool label alone',
    ['human-communication', null, null],
    'Human communication',
    'Human communication',
  ],
  [
    'a second pool with no member reads its own label',
    ['explain', null, null],
    'Explain',
    'Explain',
  ],
  [
    'a deleted pool reads its raw id',
    ['retired-pool', null, null],
    'retired-pool',
    'retired-pool',
  ],
];

for (const [name, [sessionBackend, poolMember, model], full, compact] of ROWS) {
  test(`${name}: full and compact labels`, () => {
    assert.equal(sessionModelLabel(sessionBackend, poolMember, model, 'full'), full);
    assert.equal(sessionModelLabel(sessionBackend, poolMember, model, 'compact'), compact);
  });
}

test('the longest label the header carries is 64 characters and keeps its member and model whole', () => {
  const label = sessionModelLabel('human-communication', 'aigw-gemini-flash', 'gemini-4-hydrogen', 'full');
  assert.equal(label, 'Human communication → CC · aigw Gemini Flash · gemini-4-hydrogen');
  assert.equal([...label].length, 64);
});
