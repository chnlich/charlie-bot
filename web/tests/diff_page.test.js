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
    this.attributes = {};
    this.innerHTMLValue = '';
    this.textContent = '';
    this.title = '';
    this.value = '';
    this.dataset = {};
    this.style = {};
    this.parentNode = null;
  }

  set className(value) { this.classList = createClassList(value); }

  get innerHTML() { return this.innerHTMLValue; }

  set innerHTML(value) {
    this.innerHTMLValue = String(value);
    if (this.innerHTMLValue === '') this.children = [];
  }

  append(...nodes) { nodes.forEach((node) => this.appendChild(node)); }

  appendChild(node) {
    if (node.parentNode) node.parentNode.removeChild(node);
    node.parentNode = this;
    this.children.push(node);
    return node;
  }

  insertBefore(node, reference) {
    if (node.parentNode) node.parentNode.removeChild(node);
    if (!reference) return this.appendChild(node);
    if (reference.parentNode !== this) throw new Error('reference child is not this node\'s child');
    node.parentNode = this;
    this.children.splice(this.children.indexOf(reference), 0, node);
    return node;
  }

  remove() {
    if (!this.parentNode) return;
    this.parentNode.removeChild(this);
  }

  removeChild(node) {
    const index = this.children.indexOf(node);
    if (index === -1) throw new Error('node is not this element\'s child');
    this.children.splice(index, 1);
    node.parentNode = null;
    return node;
  }

  get nextSibling() {
    if (!this.parentNode) return null;
    return this.parentNode.children[this.parentNode.children.indexOf(this) + 1] || null;
  }

  setAttribute(name, value) { this.attributes[name] = String(value); }

  getAttribute(name) { return this.attributes[name] ?? null; }

  dispatchEvent() { return true; }

  addEventListener(type, handler) { this.listeners[type] = handler; }

  focus() { this.focused = true; }

  click() { this.listeners.click(); }

  matches(selector) {
    const match = selector.match(/^([a-zA-Z]+)?((?:\.[\w-]+)+)?$/);
    assert.ok(match, `the fake DOM only reads tag and class selectors, got ${selector}`);
    const [, tag, classes] = match;
    if (tag && this.tagName.toLowerCase() !== tag.toLowerCase()) return false;
    return (classes || '').slice(1).split('.').filter(Boolean).every((name) => this.classList.contains(name));
  }

  querySelectorAll(selector) {
    const found = [];
    const walk = (node) => node.children.forEach((child) => {
      if (child.matches(selector)) found.push(child);
      walk(child);
    });
    walk(this);
    return found;
  }

  querySelector(selector) { return this.querySelectorAll(selector)[0] || null; }
}

// A rendered diff2html file: one wrapper, two side tables for side-by-side or one table for line-by-line. Rows
// carry the classes the page's own markup rules produce, so the expand controls land in them as in the browser.
function infoRow(header, unified) {
  const row = new FakeNode('TR');
  const gutter = new FakeNode('TD');
  gutter.className = unified ? 'd2h-code-linenumber d2h-info' : 'd2h-code-side-linenumber d2h-info';
  const cell = new FakeNode('TD');
  cell.className = 'd2h-info';
  const line = new FakeNode('DIV');
  line.className = unified ? 'd2h-code-line' : 'd2h-code-side-line';
  line.textContent = header === undefined ? ' ' : header;
  cell.appendChild(line);
  row.append(gutter, cell);
  return row;
}

function sideContentRow(number, text) {
  const row = new FakeNode('TR');
  const numberCell = new FakeNode('TD');
  numberCell.className = 'd2h-code-side-linenumber d2h-cntx';
  numberCell.textContent = String(number);
  const codeCell = new FakeNode('TD');
  codeCell.className = 'd2h-cntx';
  const line = new FakeNode('DIV');
  line.className = 'd2h-code-side-line';
  const code = new FakeNode('SPAN');
  code.className = 'd2h-code-line-ctn hljs js';
  code.textContent = text;
  line.appendChild(code);
  codeCell.appendChild(line);
  row.append(numberCell, codeCell);
  return row;
}

function unifiedContentRow(oldNumber, newNumber, text) {
  const row = new FakeNode('TR');
  const numberCell = new FakeNode('TD');
  numberCell.className = 'd2h-code-linenumber d2h-cntx';
  const num1 = new FakeNode('DIV');
  num1.className = 'line-num1';
  num1.textContent = oldNumber === null ? '' : String(oldNumber);
  const num2 = new FakeNode('DIV');
  num2.className = 'line-num2';
  num2.textContent = newNumber === null ? '' : String(newNumber);
  numberCell.append(num1, num2);
  const codeCell = new FakeNode('TD');
  codeCell.className = 'd2h-cntx';
  const line = new FakeNode('DIV');
  line.className = 'd2h-code-line';
  const code = new FakeNode('SPAN');
  code.className = 'd2h-code-line-ctn hljs js';
  code.textContent = text;
  line.appendChild(code);
  codeCell.appendChild(line);
  row.append(numberCell, codeCell);
  return row;
}

function buildTable(unified, rows) {
  const table = new FakeNode('TABLE');
  if (unified) table.className = 'd2h-diff-table';
  const tbody = new FakeNode('TBODY');
  table.appendChild(tbody);
  rows.forEach((row) => tbody.appendChild(row));
  return table;
}

// The side-by-side tree of the fixture diff: two tables with the same row indices, as diff2html renders them.
function sideBySideWrapper() {
  const wrapper = new FakeNode('DIV');
  wrapper.className = 'd2h-file-wrapper';
  wrapper.setAttribute('data-lang', 'js');
  const oldTable = buildTable(false, [
    infoRow('@@ -61,4 +61,4 @@'),
    sideContentRow(61, 'ctx61'), sideContentRow(62, 'old62'), sideContentRow(63, 'ctx63'), sideContentRow(64, 'ctx64'),
    infoRow('@@ -115,4 +115,4 @@'),
    sideContentRow(115, 'ctx115'), sideContentRow(116, 'old116'), sideContentRow(117, 'ctx117'),
    sideContentRow(118, 'ctx118'),
  ]);
  const newTable = buildTable(false, [
    infoRow(),
    sideContentRow(61, 'ctx61'), sideContentRow(62, 'new62'), sideContentRow(63, 'ctx63'), sideContentRow(64, 'ctx64'),
    infoRow(),
    sideContentRow(115, 'ctx115'), sideContentRow(116, 'new116'), sideContentRow(117, 'ctx117'),
    sideContentRow(118, 'ctx118'),
  ]);
  [oldTable, newTable].forEach((table) => {
    const sideDiff = new FakeNode('DIV');
    sideDiff.className = 'd2h-file-side-diff';
    sideDiff.appendChild(table);
    wrapper.appendChild(sideDiff);
  });
  return wrapper;
}

// The line-by-line tree of the same diff: one table whose rows carry both numbers.
function unifiedWrapper() {
  const wrapper = new FakeNode('DIV');
  wrapper.className = 'd2h-file-wrapper';
  wrapper.setAttribute('data-lang', 'js');
  const table = buildTable(true, [
    infoRow('@@ -61,4 +61,4 @@', true),
    unifiedContentRow(61, 61, 'ctx61'), unifiedContentRow(62, null, 'old62'),
    unifiedContentRow(null, 62, 'new62'), unifiedContentRow(63, 63, 'ctx63'), unifiedContentRow(64, 64, 'ctx64'),
    infoRow('@@ -115,4 +115,4 @@', true),
    unifiedContentRow(115, 115, 'ctx115'), unifiedContentRow(116, null, 'old116'),
    unifiedContentRow(null, 116, 'new116'), unifiedContentRow(117, 117, 'ctx117'), unifiedContentRow(118, 118, 'ctx118'),
  ]);
  const fileDiff = new FakeNode('DIV');
  fileDiff.className = 'd2h-file-diff';
  fileDiff.appendChild(table);
  wrapper.appendChild(fileDiff);
  return wrapper;
}

const FIXTURE_DIFF = [
  'diff --git a/lib/app.py b/lib/app.py',
  'index 1111111..2222222 100644',
  '--- a/lib/app.py',
  '+++ b/lib/app.py',
  '@@ -61,4 +61,4 @@',
  ' ctx61',
  '-old62',
  '+new62',
  ' ctx63',
  ' ctx64',
  '@@ -115,4 +115,4 @@',
  ' ctx115',
  '-old116',
  '+new116',
  ' ctx117',
  ' ctx118',
].join('\n');
const FIXTURE_TOTAL = 140;

const FILE_DIFF_ANSWER = {
  diff: FIXTURE_DIFF,
  path: 'lib/app.py',
  size_bytes: FIXTURE_DIFF.length,
  base_sha: '1111111111111111111111111111111111111111',
  head_sha: '2222222222222222222222222222222222222222',
  base_total: FIXTURE_TOTAL,
  head_total: FIXTURE_TOTAL,
};

// Loads diff_page.js into a vm context with the given URL query and API answers. Returns the page's handles.
// phone sets the media query the page reads for its starting format. fileDiff answers /api/git/diff/file and
// lines answers /api/git/diff/lines; tree is the diff2html DOM the Diff2HtmlUI stub renders on expand. A
// manifestFailure answers /api/git/diff/files with `{status, body}` instead of the manifest.
function loadDiffPage({search = '', repos = [], manifest = null, manifestFailure = null, phone = false,
  fileDiff = null, lines = null, tree = null} = {}) {
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
    Diff2HtmlUI: class {
      constructor(target) { this.target = target; }
      draw() { this.target.innerHTML = ''; this.target.appendChild(tree); }
      highlightCode() {}
    },
    document: {
      head: new FakeNode('HEAD'),
      getElementById(id) {
        if (!elements.has(id)) throw new Error(`diff_page.js reads an element diff.html does not declare: ${id}`);
        return elements.get(id);
      },
      createElement(tagName) { return new FakeNode(tagName); },
      createTextNode(text) { return {textContent: text}; },
    },
    fetch: async (url) => {
      fetches.push(url);
      const path = url.split('?')[0];
      if (path === '/api/git/diff/files' && manifestFailure !== null) {
        return {ok: false, status: manifestFailure.status, async json() { return manifestFailure.body; }};
      }
      if (path === '/api/git/diff/file' && fileDiff !== null) {
        return {ok: true, status: 200, async json() { return fileDiff; }};
      }
      if (path === '/api/git/diff/lines' && lines !== null) {
        return lines(url);
      }
      assert.ok(path in answers, `unexpected request ${url}`);
      return {ok: true, status: 200, async json() { return answers[path]; }};
    },
    history: {replaceState() {}},
    location: {search, pathname: '/diff'},
    window: {
      dispatchEvent() { return true; },
      addEventListener() {},
      matchMedia() { return {matches: phone}; },
    },
  };
  vm.createContext(context);
  vm.runInContext(DIFF_PAGE_JS, context, {filename: 'diff_page.js'});
  return {elements, fetches, page: context.window.__cbdiffPage, output: elements.get('diff-output')};
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

// Expands the file row of the manifest's first file and waits for the render.
async function expandFirstFile({tree, lines} = {}) {
  const page = loadDiffPage({
    search: '?repo=/work/alpha&base=main&head=feature',
    repos: [{label: 'alpha', path: '/work/alpha'}],
    manifest: MANIFEST,
    fileDiff: FILE_DIFF_ANSWER,
    lines,
    tree,
  });
  await settle();
  const row = page.output.children[0];
  row.children[0].click();
  await settle();
  return page;
}

// The first rendered table of the expanded file, and its strip rows in DOM order.
function firstTable(page) {
  const body = page.output.children[0].children[1];
  const sideDiff = body.querySelectorAll('.d2h-file-side-diff')[0];
  return sideDiff ? sideDiff.querySelector('table') : body.querySelector('.d2h-diff-table');
}

function stripRows(page) {
  return [...firstTable(page).querySelectorAll('tr.cbdx-gap-row')];
}

// The last rendered table: the right pane of a side-by-side diff, or the one line-by-line table.
function lastTable(page) {
  const body = page.output.children[0].children[1];
  const sideDiffs = body.querySelectorAll('.d2h-file-side-diff');
  return sideDiffs.length ? sideDiffs[sideDiffs.length - 1].querySelector('table') : firstTable(page);
}

function lastStripRows(page) {
  return [...lastTable(page).querySelectorAll('tr.cbdx-gap-row')];
}

// The gap count of the gap at index. One gap draws one set of controls, so the last strip carries the count.
function gapCount(page, index) {
  return stripCell(lastStripRows(page)[index]).querySelector('.cbdx-gap-count');
}

function stripCell(stripRow) {
  return stripRow.children[0];
}

function stripButton(stripRow, label) {
  const button = stripCell(stripRow).querySelectorAll('button.cbdx-expand').find((node) => node.textContent === label);
  assert.ok(button, `the strip carries a ${label} button`);
  return button;
}

// The content rows of the first table: their number-cell text in DOM order, info and strip rows left out.
function contentNumbers(page, tableIndex = 0) {
  const body = page.output.children[0].children[1];
  const table = tableIndex === 0 ? firstTable(page)
    : body.querySelectorAll('.d2h-file-side-diff')[tableIndex].querySelector('table');
  return [...table.querySelectorAll('tr')].filter((row) => !row.matches('tr.cbdx-gap-row') &&
    !row.children[0].classList.contains('d2h-info'))
    .map((row) => String(row.children[0].textContent).trim());
}

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

test('Compare sends the Base and Head refs the form starts with', async () => {
  const {elements, fetches} = loadDiffPage({
    search: '?repo=/work/alpha',
    repos: [{label: 'alpha', path: '/work/alpha'}],
    manifest: MANIFEST,
  });
  await settle();

  assert.equal(elements.get('base-input').value, 'main');
  assert.equal(elements.get('head-input').value, 'HEAD');
  elements.get('diff-form').listeners.submit({preventDefault() {}});
  await settle();

  assert.equal(elements.get('error').classList.contains('hidden'), true);
  assert.ok(fetches.some((url) =>
    url.startsWith('/api/git/diff/files?') && url.includes('base=main') && url.includes('head=HEAD')));
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

test('an unresolved ref reads in the form\'s words and keeps git\'s line as the title', async () => {
  const baseDetail = "fatal: ambiguous argument 'no-such-base^{commit}': unknown revision or path not in the "
    + "working tree. Use '--' to separate paths from revisions, like this: 'git <command> [<revision>...] -- "
    + "[<file>...]'";
  const base = loadDiffPage({
    search: '?repo=/work/alpha&base=no-such-base&head=feature',
    repos: [{label: 'alpha', path: '/work/alpha'}],
    manifestFailure: {status: 500, body: {detail: baseDetail}},
  });
  await settle();
  const baseError = base.elements.get('error');
  assert.equal(baseError.classList.contains('hidden'), false);
  assert.equal(baseError.textContent, "Base 'no-such-base' is not a commit in this repository.");
  assert.equal(baseError.title, baseDetail);

  const headDetail = "fatal: ambiguous argument 'no-such-head^{commit}': unknown revision or path not in the "
    + 'working tree.';
  const head = loadDiffPage({
    search: '?repo=/work/alpha&base=main&head=no-such-head',
    repos: [{label: 'alpha', path: '/work/alpha'}],
    manifestFailure: {status: 500, body: {detail: headDetail}},
  });
  await settle();
  const headError = head.elements.get('error');
  assert.equal(headError.textContent, "Head 'no-such-head' is not a commit in this repository.");
  assert.equal(headError.title, headDetail);
});

test('another failure keeps the server\'s detail in the box and as its title', async () => {
  const {elements} = loadDiffPage({
    search: '?repo=/work/plain&base=main&head=feature',
    repos: [{label: 'plain', path: '/work/plain'}],
    manifestFailure: {status: 400, body: {detail: 'Not a git repo: /work/plain'}},
  });
  await settle();

  const error = elements.get('error');
  assert.equal(error.textContent, 'Not a git repo: /work/plain');
  assert.equal(error.title, 'Not a git repo: /work/plain');
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

test('a move row names the path the move came from', async () => {
  const {elements} = loadDiffPage({
    search: '?repo=/work/alpha&base=main&head=feature',
    repos: [{label: 'alpha', path: '/work/alpha'}],
    manifest: {
      ...MANIFEST,
      files: [
        {path: 'lib/counter.py', status: 'R', additions: 4, deletions: 0, old_path: 'lib/old_name.py'},
        {path: 'added_copy.py', status: 'C', additions: 2, deletions: 0, old_path: 'lib/app.py'},
      ],
    },
  });
  await settle();

  const [renamed, copied] = elements.get('diff-output').children.map((row) => row.children[0]);
  const renamedPath = renamed.children[3];
  assert.deepEqual(renamedPath.children.map((node) => node.textContent),
    ['lib/', 'counter.py', 'renamed from lib/old_name.py']);
  assert.equal(renamedPath.children[2].title, 'renamed from lib/old_name.py');

  const copiedPath = copied.children[3];
  assert.deepEqual(copiedPath.children.map((node) => node.textContent),
    ['added_copy.py', 'copied from lib/app.py']);
});

test('a phone starts line-by-line, a wider window side-by-side, and the URL names its own format', async () => {
  const phone = loadDiffPage({phone: true});
  await settle();
  assert.equal(phone.elements.get('format-select').value, 'line-by-line');

  const desktop = loadDiffPage({});
  await settle();
  assert.equal(desktop.elements.get('format-select').value, 'side-by-side');

  const named = loadDiffPage({phone: true, search: '?outputFormat=side-by-side'});
  await settle();
  assert.equal(named.elements.get('format-select').value, 'side-by-side');
});

test('parseHunks reads the headers git writes, omitted counts included', () => {
  const {page} = loadDiffPage({});
  // The hunks come from the vm context's realm, so the comparison reads their fields as rows.
  const hunks = [...page.parseHunks(`@@ -61,4 +61,4 @@\n@@ -115 +115 @@`)];
  assert.deepEqual(hunks.map((hunk) => [hunk.a, hunk.b, hunk.c, hunk.d]),
    [[61, 4, 61, 4], [115, 1, 115, 1]]);
});

test('hiddenGaps names the top, between-hunk and bottom runs with both sides\' numbers', () => {
  const {page} = loadDiffPage({});
  const hunks = page.parseHunks(FIXTURE_DIFF);
  const gaps = [...page.hiddenGaps(hunks, FIXTURE_TOTAL, FIXTURE_TOTAL)];
  assert.deepEqual(gaps.map((gap) => [gap.oldLo, gap.oldHi, gap.newLo, gap.newHi, gap.hunkIndex]), [
    [1, 60, 1, 60, 0],
    [65, 114, 65, 114, 1],
    [119, 140, 119, 140, 2],
  ]);
});

test('hiddenGaps finds no gap when a file is added or deleted whole', () => {
  const {page} = loadDiffPage({});
  const added = [...page.parseHunks('@@ -0,0 +1,500 @@')];
  assert.deepEqual([...page.hiddenGaps(added, 0, 500)], []);
  const deleted = [...page.parseHunks('@@ -1,500 +0,0 @@')];
  assert.deepEqual([...page.hiddenGaps(deleted, 500, 0)], []);
});

test('revealRange cuts 20 lines from each edge and hands a short gap over whole', () => {
  const {page} = loadDiffPage({});
  const gap = {oldLo: 1, oldHi: 60, newLo: 1, newHi: 60};
  assert.deepEqual([...page.revealRange(gap, 'up')], [41, 60]);
  assert.deepEqual([...page.revealRange(gap, 'down')], [1, 20]);
  const short = {oldLo: 1, oldHi: 12, newLo: 1, newHi: 12};
  assert.deepEqual([...page.revealRange(short, 'down')], [1, 12]);
  assert.deepEqual([...page.revealRange(short, 'whole')], [1, 12]);
});

test('an expanded file carries a strip at the top, between-hunk and bottom gap', async () => {
  const page = await expandFirstFile({tree: sideBySideWrapper(), lines: () => { throw new Error('unused'); }});

  const rows = stripRows(page);
  assert.equal(rows.length, 3);
  const top = stripCell(rows[0]).querySelectorAll('button.cbdx-expand').map((button) => button.textContent);
  assert.deepEqual(top, ['↑ Expand up', '↓ Expand down', 'Whole file']);
  assert.equal(gapCount(page, 0).textContent, '60 hidden lines');
  assert.equal(gapCount(page, 1).textContent, '50 hidden lines');
  assert.equal(gapCount(page, 2).textContent, '22 hidden lines');
  // One gap draws one set of controls: the second pane's strips keep their rows and carry no buttons.
  const rightRows = lastStripRows(page);
  assert.equal(rightRows.length, 3, 'both panes carry a strip row for every gap');
  assert.deepEqual(rightRows.map((row) => stripCell(row).querySelectorAll('button.cbdx-expand').length), [0, 0, 0]);
  // Both panes' cells take the stack class, so a narrow pane stacks its controls without moving the other
  // pane's lines. Every gap here outlives one click, so each strip carries the three-button height too.
  const stackClasses = [...rows, ...rightRows].map((row) => row.querySelector('.cbdx-gap-cell').classList.toString());
  assert.deepEqual(stackClasses, Array(6).fill('cbdx-gap-cell cbdx-gap-stack cbdx-gap-stack-three'));
  // The strips sit before their hunk's header row, the bottom strip at the table's end.
  const positions = firstTable(page).querySelectorAll('tr').map((row) => row.matches('tr.cbdx-gap-row') ? 'strip'
    : row.children[0].classList.contains('d2h-info') ? 'info' : 'row');
  assert.deepEqual(positions, ['strip', 'info', 'row', 'row', 'row', 'row', 'strip', 'info', 'row', 'row', 'row',
    'row', 'strip']);
});

test('expand down and expand up fill the top gap in line order with both sides\' numbers', async () => {
  const lines = (url) => {
    const params = new URLSearchParams(url.split('?')[1]);
    // The read pins the revisions the served diff was built between.
    assert.equal(params.get('base'), FILE_DIFF_ANSWER.base_sha);
    assert.equal(params.get('head'), FILE_DIFF_ANSWER.head_sha);
    const side = params.get('side');
    const start = Number(params.get('start'));
    const end = Math.min(Number(params.get('end')), FIXTURE_TOTAL);
    const linesOut = [];
    for (let n = start; n <= end; n += 1) linesOut.push({line: n, text: `${side} text ${n}`});
    return {ok: true, status: 200, async json() {
      return {path: 'lib/app.py', side, start, end, total: FIXTURE_TOTAL, lines: linesOut};
    }};
  };
  const page = await expandFirstFile({tree: sideBySideWrapper(), lines});

  stripButton(stripRows(page)[0], '↓ Expand down').click();
  await settle();
  let numbers = contentNumbers(page);
  assert.deepEqual(numbers.slice(0, 22), [...Array.from({length: 20}, (_, i) => String(i + 1)), '61', '62']);
  assert.equal(gapCount(page, 0).textContent, '40 hidden lines');

  stripButton(stripRows(page)[0], '↑ Expand up').click();
  await settle();
  numbers = contentNumbers(page);
  // The two revealed blocks keep line order, the second sitting against the hunk header.
  assert.deepEqual(numbers.slice(0, 42), [
    ...Array.from({length: 20}, (_, i) => String(i + 1)),
    ...Array.from({length: 20}, (_, i) => String(i + 41)),
    '61', '62',
  ]);
  // The new side's table carries the same rows at the same indices.
  const newNumbers = contentNumbers(page, 1);
  assert.deepEqual(newNumbers.slice(0, 42), numbers.slice(0, 42));
  const revealed = firstTable(page).querySelectorAll('tr')[1];
  assert.equal(String(revealed.children[0].textContent).trim(), '1');
  assert.equal(revealed.children[1].querySelector('.d2h-code-line-ctn').textContent, 'base text 1');
  const revealedNew = page.output.children[0].children[1].querySelectorAll('.d2h-file-side-diff')[1]
    .querySelector('table').querySelectorAll('tr')[1];
  assert.equal(revealedNew.children[1].querySelector('.d2h-code-line-ctn').textContent, 'head text 1');
  // The gap shrank to the one-click size, so its strip draws one direction button and drops the taller
  // three-button height from both panes' cells.
  assert.equal(gapCount(page, 0).textContent, '20 hidden lines');
  const cells = [stripRows(page)[0], lastStripRows(page)[0]]
    .map((row) => row.querySelector('.cbdx-gap-cell').classList);
  assert.deepEqual(cells.map((cls) => [cls.contains('cbdx-gap-stack'), cls.contains('cbdx-gap-stack-three')]),
    [[true, false], [true, false]]);
});

test('the between-hunk and bottom gaps expand toward their own edges', async () => {
  const lines = (url) => {
    const params = new URLSearchParams(url.split('?')[1]);
    const side = params.get('side');
    const start = Number(params.get('start'));
    const end = Math.min(Number(params.get('end')), FIXTURE_TOTAL);
    const linesOut = [];
    for (let n = start; n <= end; n += 1) linesOut.push({line: n, text: `${side} text ${n}`});
    return {ok: true, status: 200, async json() {
      return {path: 'lib/app.py', side, start, end, total: FIXTURE_TOTAL, lines: linesOut};
    }};
  };
  const page = await expandFirstFile({tree: sideBySideWrapper(), lines});

  // The middle gap expands up against the second hunk's header and down against the first hunk's tail.
  stripButton(stripRows(page)[1], '↑ Expand up').click();
  await settle();
  let numbers = contentNumbers(page);
  assert.deepEqual(numbers.slice(4, 24), Array.from({length: 20}, (_, i) => String(i + 95)));

  stripButton(stripRows(page)[1], '↓ Expand down').click();
  await settle();
  numbers = contentNumbers(page);
  assert.deepEqual(numbers.slice(4, 24), Array.from({length: 20}, (_, i) => String(i + 65)));
  assert.deepEqual(numbers.slice(24, 26), ['95', '96'], 'the up block keeps its place above the second hunk');

  // The bottom gap expands down from the last hunk's tail; the two lines left above then expand whole.
  stripButton(stripRows(page)[2], '↓ Expand down').click();
  await settle();
  numbers = contentNumbers(page);
  assert.deepEqual(numbers.slice(-20), Array.from({length: 20}, (_, i) => String(i + 119)));

  stripButton(stripRows(page)[2], 'Expand 2 lines').click();
  await settle();
  numbers = contentNumbers(page);
  assert.deepEqual(numbers.slice(-22), Array.from({length: 22}, (_, i) => String(i + 119)));
});

test('a short gap expands whole in one click and its strip goes away', async () => {
  // A diff whose top gap holds 12 lines and whose bottom gap holds 4.
  const diff = [
    'diff --git a/lib/app.py b/lib/app.py',
    'index 1111111..2222222 100644',
    '--- a/lib/app.py',
    '+++ b/lib/app.py',
    '@@ -13,4 +13,4 @@',
    ' ctx13',
    '-old14',
    '+new14',
    ' ctx15',
    ' ctx16',
  ].join('\n');
  const lines = (url) => {
    const params = new URLSearchParams(url.split('?')[1]);
    const side = params.get('side');
    const start = Number(params.get('start'));
    const end = Math.min(Number(params.get('end')), 20);
    const linesOut = [];
    for (let n = start; n <= end; n += 1) linesOut.push({line: n, text: `${side} text ${n}`});
    return {ok: true, status: 200, async json() {
      return {path: 'lib/app.py', side, start, end, total: 20, lines: linesOut};
    }};
  };
  const shortWrapper = (() => {
    const wrapper = new FakeNode('DIV');
    wrapper.className = 'd2h-file-wrapper';
    wrapper.setAttribute('data-lang', 'js');
    const oldTable = buildTable(false, [
      infoRow('@@ -13,4 +13,4 @@'),
      sideContentRow(13, 'ctx13'), sideContentRow(14, 'old14'), sideContentRow(15, 'ctx15'),
      sideContentRow(16, 'ctx16'),
    ]);
    const newTable = buildTable(false, [
      infoRow(),
      sideContentRow(13, 'ctx13'), sideContentRow(14, 'new14'), sideContentRow(15, 'ctx15'),
      sideContentRow(16, 'ctx16'),
    ]);
    [oldTable, newTable].forEach((table) => {
      const sideDiff = new FakeNode('DIV');
      sideDiff.className = 'd2h-file-side-diff';
      sideDiff.appendChild(table);
      wrapper.appendChild(sideDiff);
    });
    return wrapper;
  })();

  const short = loadDiffPage({
    search: '?repo=/work/alpha&base=main&head=feature',
    repos: [{label: 'alpha', path: '/work/alpha'}],
    manifest: MANIFEST,
    fileDiff: {...FILE_DIFF_ANSWER, diff, base_total: 20, head_total: 20},
    lines,
    tree: shortWrapper,
  });
  await settle();
  short.output.children[0].children[0].click();
  await settle();

  const rows = stripRows(short);
  assert.equal(rows.length, 2);
  assert.equal(gapCount(short, 0).textContent, '12 hidden lines');
  assert.deepEqual(
    stripCell(rows[0]).querySelectorAll('button.cbdx-expand').map((button) => button.textContent),
    ['Expand 12 lines', 'Whole file']);
  stripButton(rows[0], 'Expand 12 lines').click();
  await settle();
  assert.deepEqual(contentNumbers(short).slice(0, 14),
    [...Array.from({length: 12}, (_, i) => String(i + 1)), '13', '14']);
  assert.equal(stripRows(short).length, 1, 'the finished gap loses its strip');
});

test('Whole file reveals every line of both sides once', async () => {
  const lines = (url) => {
    const params = new URLSearchParams(url.split('?')[1]);
    const side = params.get('side');
    const start = Number(params.get('start'));
    const end = Math.min(Number(params.get('end')), FIXTURE_TOTAL);
    const linesOut = [];
    for (let n = start; n <= end; n += 1) linesOut.push({line: n, text: `${side} text ${n}`});
    return {ok: true, status: 200, async json() {
      return {path: 'lib/app.py', side, start, end, total: FIXTURE_TOTAL, lines: linesOut};
    }};
  };
  const page = await expandFirstFile({tree: sideBySideWrapper(), lines});

  // A mixed start: one partial expand, then the whole file from another strip.
  stripButton(stripRows(page)[0], '↓ Expand down').click();
  await settle();
  stripButton(stripRows(page)[2], 'Whole file').click();
  await settle();

  assert.equal(stripRows(page).length, 0, 'every gap is exhausted, every strip is gone');
  for (const side of [0, 1]) {
    const numbers = contentNumbers(page, side).map(Number);
    assert.deepEqual(numbers, Array.from({length: FIXTURE_TOTAL}, (_, i) => i + 1));
  }
  // With the strips gone, the table opens on the revealed lines the whole file pulled in.
  const first = firstTable(page).querySelectorAll('tr')[0];
  assert.equal(first.children[1].querySelector('.d2h-code-line-ctn').textContent, 'base text 1');
});

test('a failed line read reports its error in the strip and reveals nothing', async () => {
  const lines = (url) => {
    if (url.includes('side=base')) {
      return {ok: false, status: 404, async json() {
        return {detail: 'nope.txt does not exist at 1111111'};
      }};
    }
    return {ok: true, status: 200, async json() {
      return {path: 'lib/app.py', side: 'head', start: 1, end: 20, total: FIXTURE_TOTAL,
        lines: Array.from({length: 20}, (_, i) => ({line: i + 1, text: `head text ${i + 1}`}))};
    }};
  };
  const page = await expandFirstFile({tree: sideBySideWrapper(), lines});

  stripButton(stripRows(page)[0], '↓ Expand down').click();
  await settle();
  const error = stripCell(stripRows(page)[0]).querySelector('.cbdx-gap-error');
  assert.match(error.textContent, /Failed to load lines: nope\.txt does not exist/);
  assert.deepEqual(contentNumbers(page).slice(0, 2), ['61', '62'], 'no revealed rows joined the table');
});

test('the line-by-line format reveals rows carrying both sides\' numbers', async () => {
  const lines = (url) => {
    const params = new URLSearchParams(url.split('?')[1]);
    const side = params.get('side');
    const start = Number(params.get('start'));
    const end = Math.min(Number(params.get('end')), FIXTURE_TOTAL);
    const linesOut = [];
    for (let n = start; n <= end; n += 1) linesOut.push({line: n, text: `${side} text ${n}`});
    return {ok: true, status: 200, async json() {
      return {path: 'lib/app.py', side, start, end, total: FIXTURE_TOTAL, lines: linesOut};
    }};
  };
  const page = await expandFirstFile({tree: unifiedWrapper(), lines});

  stripButton(stripRows(page)[0], '↓ Expand down').click();
  await settle();
  const table = firstTable(page);
  const revealed = table.querySelectorAll('tr')[1];
  assert.equal(String(revealed.querySelector('.line-num1').textContent), '1');
  assert.equal(String(revealed.querySelector('.line-num2').textContent), '1');
  assert.equal(revealed.querySelector('.d2h-code-line-ctn').textContent, 'head text 1');
});
