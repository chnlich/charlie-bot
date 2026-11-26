const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const { readStatic, runIconModule } = require('./read_static');

// The page's server-rendered label table: BACKEND_LABELS[backend][member || ""] is the [full, compact] pair
// BackendsConfig::session_model_label writes. The ids are synthetic: this test reads the browser lookup, not a host.
const BACKEND_LABELS = {
  'pool-a': {
    '': ['Pool A', 'Pool A'],
    'member-a': ['Pool A → CC · Member A', 'Pool A → Member A'],
  },
  'pool-b': {
    '': ['Pool B', 'Pool B'],
  },
  'single-a': {
    '': ['CC · Single A', 'Single A'],
  },
  // The longest label in use, with synthetic ids: the member label plus the model composes to 64 characters.
  'pool-long': {
    '': ['Human communication', 'Human communication'],
    'member-long': ['Human communication → CC · aigw Gemini Flash', 'Human communication → aigw Gemini Flash'],
  },
};

// chat/shared.js is the one module that defines sessionModelLabel; the page loads it after chat/namespace.js.
function loadSharedLabel() {
  const context = { console, BACKEND_LABELS, BACKEND_OPTIONS: {}, BACKEND_POOLS: [] };
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
    ['pool-a', 'member-a', 'member-model-5-5'],
    'Pool A → CC · Member A · member-model-5-5',
    'Pool A → Member A',
  ],
  [
    'a pool member without a model reads pool → member',
    ['pool-a', 'member-a', null],
    'Pool A → CC · Member A',
    'Pool A → Member A',
  ],
  [
    'a pool with no member reads the pool label alone',
    ['pool-a', null, null],
    'Pool A',
    'Pool A',
  ],
  [
    'a single-option session reads its option label',
    ['single-a', null, null],
    'CC · Single A',
    'Single A',
  ],
  [
    'a single-option session switched from a pool reads the same option label',
    ['single-a', null, null],
    'CC · Single A',
    'Single A',
  ],
  [
    'a pool whose recorded member the server filtered out reads the pool label alone',
    ['pool-a', null, null],
    'Pool A',
    'Pool A',
  ],
  [
    'a second pool with no member reads its own label',
    ['pool-b', null, null],
    'Pool B',
    'Pool B',
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

test('the full form of a pool member takes the model after the server label; the compact form leaves it out', () => {
  assert.equal(sessionModelLabel('pool-a', 'member-a', 'member-model-5-5', 'full'),
    'Pool A → CC · Member A · member-model-5-5');
  assert.equal(sessionModelLabel('pool-a', 'member-a', 'member-model-5-5', 'compact'), 'Pool A → Member A');
  // A single option's label, and a pool without a member, keep their own form: the model belongs to a pool member.
  assert.equal(sessionModelLabel('single-a', null, 'single-model-5-5', 'full'), 'CC · Single A');
  assert.equal(sessionModelLabel('single-a', null, 'single-model-5-5', 'compact'), 'Single A');
  assert.equal(sessionModelLabel('pool-a', null, 'single-model-5-5', 'full'), 'Pool A');
});

test('the longest label the header carries is 64 characters and keeps its member and model whole', () => {
  const label = sessionModelLabel('pool-long', 'member-long', 'gemini-4-hydrogen', 'full');
  assert.equal(label, 'Human communication → CC · aigw Gemini Flash · gemini-4-hydrogen');
  assert.equal([...label].length, 64);
});
