const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const { createClassList } = require('./dom_element_stub');
const { readPackageStatic } = require('./read_static');

const DIFF_PAGE_JS = readPackageStatic('features', 'diff_view', 'diff_page.js');

// The page's elements as diff.html declares them: the ids the script reads and the classes that show or hide them.
const PAGE_ELEMENTS = {
  'repo-input': {}, 'repo-list': {}, 'base-input': {}, 'head-input': {}, 'branches-list': {},
  'mode-select': {value: 'three-dot'}, 'format-select': {value: 'side-by-side'}, 'diff-form': {},
  start: {className: 'hidden'},
  'start-repos': {className: 'flex flex-wrap hidden'},
  'start-repo-list': {className: 'flex flex-wrap'},
  summary: {className: 'flex flex-wrap hidden'},
  loading: {className: 'hidden'}, error: {className: 'hidden'}, empty: {className: 'hidden'},
  'diff-output': {},
  // The form's status line: Open in code-server writes its progress here.
  status: {textContent: 'preparing code-server…'},
};

class FakeNode {
  constructor(tagName = 'DIV') {
    this.tagName = String(tagName).toUpperCase();
    this.children = [];
    this.classList = createClassList();
    this.listeners = {};
    this.textContent = '';
    this.title = '';
    this.value = '';
    this.dataset = {};
    this.style = {};
  }

  set className(value) { this.classList = createClassList(value); }

  get innerHTML() { return ''; }

  set innerHTML(value) {
    if (value !== '') throw new Error('FakeNode clears with innerHTML = "" only');
    this.children = [];
  }

  append(...nodes) { this.children.push(...nodes); }

  appendChild(node) { this.children.push(node); return node; }

  addEventListener(type, handler) { this.listeners[type] = handler; }

  focus() { this.focused = true; }

  click() { this.listeners.click(); }
}

// Loads diff_page.js into a vm context with the given URL query and API answers. Returns the page's handles.
function loadDiffPage({search = '', repos = [], manifest = null}) {
  const elements = new Map();
  for (const [id, spec] of Object.entries(PAGE_ELEMENTS)) {
    const element = Object.assign(new FakeNode(), spec);
    elements.set(id, element);
  }
  const fetches = [];
  const answers = {
    '/api/git/repos': repos,
    '/api/git/branches': ['main', 'feature'],
    '/api/git/diff/files': manifest,
  };
  const context = {
    URLSearchParams,
    console,
    CustomEvent: class CustomEvent {
      constructor(type, init = {}) {
        this.type = type;
        this.detail = init.detail;
      }
    },
    document: {
      getElementById(id) {
        if (!elements.has(id)) throw new Error(`diff_page.js reads an element diff.html does not declare: ${id}`);
        return elements.get(id);
      },
      createElement(tagName) { return new FakeNode(tagName); },
    },
    fetch: async (url) => {
      fetches.push(url);
      const path = url.split('?')[0];
      assert.ok(path in answers, `unexpected request ${url}`);
      return {ok: true, status: 200, async json() { return answers[path]; }};
    },
    history: {replaceState() {}},
    location: {search, pathname: '/diff'},
    window: {
      dispatchEvent() { return true; },
      addEventListener() {},
    },
  };
  vm.createContext(context);
  vm.runInContext(DIFF_PAGE_JS, context, {filename: 'diff_page.js'});
  return {elements, fetches};
}

// Lets the page's async start-up and comparison chain run to the end.
async function settle() {
  for (let round = 0; round < 10; round += 1) {
    await new Promise((resolve) => setImmediate(resolve));
  }
}

const MANIFEST = {
  base: 'main',
  head: 'feature',
  mode: 'three-dot',
  head_sha: 'abcdef0123456789',
  total_files: 2,
  total_additions: 5,
  total_deletions: 1,
  files: [
    {path: 'lib/app.py', status: 'M', additions: 3, deletions: 1},
    {path: 'README.md', status: 'A', additions: 2, deletions: 0},
  ],
};

test('the start guide shows without a comparison and a repository button fills Repo', async () => {
  const {elements, fetches} = loadDiffPage({
    repos: [{label: 'alpha', path: '/work/alpha'}, {label: 'beta', path: '/work/beta'}],
  });
  await settle();

  assert.equal(elements.get('start').classList.contains('hidden'), false);
  assert.equal(elements.get('start-repos').classList.contains('hidden'), false);
  const buttons = elements.get('start-repo-list').children;
  assert.deepEqual(buttons.map((button) => button.title), ['/work/alpha', '/work/beta']);

  buttons[1].click();
  assert.equal(elements.get('repo-input').value, '/work/beta');
  assert.equal(elements.get('base-input').focused, true);
  await settle();
  assert.ok(fetches.includes('/api/git/branches?repo=%2Fwork%2Fbeta'));
});

test('a comparison writes its totals above the list and leaves the form status line alone', async () => {
  const {elements} = loadDiffPage({
    search: '?repo=/work/alpha&base=main&head=feature',
    repos: [{label: 'alpha', path: '/work/alpha'}],
    manifest: MANIFEST,
  });
  await settle();

  assert.equal(elements.get('start').classList.contains('hidden'), true);
  assert.equal(elements.get('summary').classList.contains('hidden'), false);
  const [count, range] = elements.get('summary').children;
  assert.deepEqual(count.children.map((node) => node.textContent), ['+5', '-1']);
  assert.ok(range.textContent.startsWith('main...feature'));
  assert.equal(elements.get('status').textContent, 'preparing code-server…');
  assert.equal(elements.get('diff-output').children.length, 2);
});

test('a file row keeps the directory apart from the file name, with the full path as its title', async () => {
  const {elements} = loadDiffPage({
    search: '?repo=/work/alpha&base=main&head=feature',
    repos: [{label: 'alpha', path: '/work/alpha'}],
    manifest: MANIFEST,
  });
  await settle();

  const [nested, top] = elements.get('diff-output').children.map((row) => row.children[0]);
  const [, , nestedCounts, nestedPath] = nested.children;
  assert.equal(nestedPath.title, 'lib/app.py');
  assert.deepEqual(nestedPath.children.map((node) => node.textContent), ['lib/', 'app.py']);
  assert.deepEqual(nestedCounts.children.map((node) => node.textContent), ['+3', '-1']);

  const [, , , topPath] = top.children;
  assert.deepEqual(topPath.children.map((node) => node.textContent), ['README.md']);
});
