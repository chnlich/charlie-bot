// ---------------------------------------------------------------------------
// The sidebar's "Move under…" picker (sidebar/session-move.js) and its row menu
// item (sidebar/session-menus.js): one test per acceptance item 1-7 of the move
// plan. The harness runs groups.js (through sidebar_groups_context_stub.js),
// then session-actions.js for the modal chrome, session-menus.js and
// session-move.js in one vm context over a small document and fetch stub. The
// Settings buttons are opened through the onclick strings groups.js renders.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const {readStatic} = require('./read_static');
const {loadGroups} = require('./sidebar_groups_context_stub');

const PLAN_FOOTER = 'After the move, this session follows the rules set by its new parent and the sessions above it, ' +
  'and its prompt names the new parent\'s goal. When this changes its prompt, a live conversation starts over on its ' +
  'next turn.';
const MOVE_LABEL = 'Move under…';

class FakeElement {
  constructor(tagName) {
    this.tagName = String(tagName).toUpperCase();
    this.id = '';
    this.className = '';
    this.children = [];
    this.dataset = {};
    this.style = {};
    this.listeners = {};
    this.parentElement = null;
    this.textContent = '';
    this.value = '';
    this.placeholder = '';
    this._innerHTML = '';
  }

  get innerHTML() { return this._innerHTML; }

  set innerHTML(html) {
    this._innerHTML = html;
    if (html === '') this.children = [];
  }

  appendChild(child) {
    child.parentElement = this;
    this.children.push(child);
    return child;
  }

  remove() {
    if (this.parentElement) {
      this.parentElement.children = this.parentElement.children.filter((child) => child !== this);
    }
    this.parentElement = null;
  }

  addEventListener(type, handler) {
    this.listeners[type] = (this.listeners[type] || []).concat([handler]);
  }

  removeEventListener(type, handler) {
    this.listeners[type] = (this.listeners[type] || []).filter((known) => known !== handler);
  }

  fire(type, event = {}) {
    for (const handler of this.listeners[type] || []) handler({target: this, ...event});
  }

  click() { this.fire('click'); }

  focus() {}
}

function fakeDocument() {
  const document = new FakeElement('#document');
  document.body = new FakeElement('body');
  document.readyState = 'complete';
  document.createElement = (tag) => new FakeElement(tag);
  document.getElementById = () => null;
  return document;
}

// Answers each "METHOD url" with its {status, body}; every call is recorded in order.
function fakeFetch(answers) {
  const calls = [];
  const fetch = async (url, options = {}) => {
    const method = options.method || 'GET';
    calls.push({method, url, body: options.body});
    const answer = answers[`${method} ${url}`];
    if (!answer) throw new Error(`no fake answer for ${method} ${url}`);
    return {ok: answer.status >= 200 && answer.status < 300, status: answer.status, json: async () => answer.body};
  };
  return {fetch, calls};
}

function row(id, profile, parent = null, name = `Session ${id}`) {
  return {id, name, profile, task_parent_id: parent, updated_at: '2026-11-15T10:00:00Z'};
}

// The Workspace list, which holds the thread sessions too. B is the moved session, under P. D and G are its
// descendants: G is a thread session under D, and a walk over the rows finds it. C is a worker. T and E are a thread
// session and its child.
const WORKSPACE_ROWS = [
  row('P', 'manager'),
  row('B', 'manager', 'P'),
  row('D', 'manager', 'B'),
  row('C', 'worker', 'P'),
  row('A', 'manager'),
  row('T', 'manager'),
  row('E', 'manager', 'T'),
  row('G', 'manager', 'D'),
];

// The Workspace answer in the route's `compact=move` column form: one cell array per field, in row order. The ids are
// not canonical UUIDs, so each id cell is `~` and its text, which is the form the route writes for such a home.
function columnForm(rows) {
  return {
    $encoded: {id: 'uuid-b64'},
    id: rows.map((entry) => `~${entry.id}`),
    name: rows.map((entry) => entry.name),
    profile: rows.map((entry) => entry.profile),
    task_parent_id: rows.map((entry) => entry.task_parent_id ?? null),
  };
}

const LIST_ANSWERS = {
  'GET /api/sessions/?compact=move': {status: 200, body: columnForm(WORKSPACE_ROWS)},
};

function buildContext(answers) {
  const context = loadGroups();
  vm.runInContext(readStatic('sidebar/session-actions.js'), context, {filename: 'session-actions.js'});
  vm.runInContext(readStatic('sidebar/session-menus.js'), context, {filename: 'session-menus.js'});
  vm.runInContext(readStatic('sidebar/session-move.js'), context, {filename: 'session-move.js'});
  const document = fakeDocument();
  const nav = new FakeElement('div');
  document.getElementById = (id) => (id === 'session-list' ? nav : null);
  const {fetch, calls} = fakeFetch(answers);
  Object.assign(context, {
    document,
    fetch,
    JSON_HEADERS: {'Content-Type': 'application/json'},
    window: {matchMedia: () => ({matches: false})},
    updateRelativeTimes: () => {},
  });
  return {context, document, nav, calls};
}

// The open picker's parts, or null when no picker is open. The dialog's children are the header, the search box, the
// choice list, the error box and the footer, in that order.
function pickerParts(document) {
  const overlay = document.body.children.find((el) => el.id === 'session-move-overlay');
  if (!overlay) return null;
  const [header, search, list, errors, footer] = overlay.children[0].children;
  return {overlay, title: header.children[0], search, list, errors, footer};
}

function choiceLabels(document) {
  return pickerParts(document).list.children.map((button) => button.textContent);
}

function choiceButton(document, label) {
  const button = pickerParts(document).list.children.find((el) => el.textContent === label);
  assert.ok(button, `the picker lists ${label}`);
  return button;
}

async function settle() {
  await new Promise((resolve) => setImmediate(resolve));
}

// The item labels of a row's Settings menu, opened through the Settings button groups.js renders for the row under
// `filter`. Null when the row renders no Settings button.
function settingsMenuLabels(context, nav, filter, session) {
  context.renderSessionList([session], filter);
  const button = nav.innerHTML.match(/<button[^>]*title="Settings"[^>]*>/);
  if (!button) return null;
  const anchor = {dataset: {}};
  for (const [, key, value] of button[0].matchAll(/data-([a-z-]+)="([^"]*)"/g)) {
    anchor.dataset[key.replace(/-([a-z])/g, (_, letter) => letter.toUpperCase())] = value;
  }
  const onclick = button[0].match(/onclick="([^"]+)"/)[1];
  let items = null;
  context.openRowMenu = (_anchor, menuItems) => { items = menuItems; };
  context.__anchor = anchor;
  context.__event = {preventDefault() {}, stopPropagation() {}};
  vm.runInContext(`(function (event) { ${onclick} }).call(__anchor, __event)`, context);
  return items.filter((item) => !item.separator).map((item) => item.label);
}

test('acceptance 1: a manager row offers Move under… in every view; worker and archived rows do not', () => {
  const {context, nav} = buildContext(LIST_ANSWERS);
  for (const filter of ['all', 'starred', 'search', 'threads']) {
    const labels = settingsMenuLabels(context, nav, filter, row('m1', 'manager'));
    assert.ok(labels.includes(MOVE_LABEL), `a manager row under ${filter} offers ${MOVE_LABEL}`);
  }
  assert.equal(settingsMenuLabels(context, nav, 'all', row('w1', 'worker', 'm1')), null,
    'a worker row renders no Settings button');
  const archived = {...row('a1', 'manager'), status: 'archived'};
  const archivedLabels = settingsMenuLabels(context, nav, 'archived', archived);
  assert.ok(archivedLabels.length > 0, 'the archived row has a Settings menu');
  assert.ok(!archivedLabels.includes(MOVE_LABEL), 'an archived row does not offer the move');
});

test('acceptance 2: candidates come from the Workspace list, less the moved session, its descendants and parent', async () => {
  const {context, document, calls} = buildContext(LIST_ANSWERS);
  const expected = ['Top level', 'Session A', 'Session T', 'Session E'];
  for (const view of ['all', 'starred', 'threads', 'search']) {
    context.currentFilter = view;
    await context.openSessionMovePicker('B');
    assert.deepEqual(choiceLabels(document), expected, `the candidates opened from ${view}`);
  }
  assert.ok(calls.length > 0 && calls.every((call) => call.url === '/api/sessions/?compact=move'),
      'the Workspace list is the one list the picker reads');

  await context.openSessionMovePicker('B');
  const parts = pickerParts(document);
  parts.search.value = 't';
  parts.search.fire('input');
  assert.deepEqual(choiceLabels(document), ['Top level', 'Session T'], 'the search filters the candidates by name');
  // CHOICE_CLASS elides a long label, so each choice carries its full label as its hover text.
  assert.equal(choiceButton(document, 'Session T').title, 'Session T', 'a choice names itself on hover');
  // The header's title block holds the heading, then the quoted subject, whose hover text is the name in its quotes.
  const subject = parts.title.children[1];
  assert.equal(subject.title, subject.textContent.slice(1, -1), 'the subject carries the moved session name on hover');
});

test('acceptance 3: one pick sends one PATCH whose body holds only task_parent_id', async () => {
  const {context, document, calls} = buildContext({
    ...LIST_ANSWERS,
    'PATCH /api/sessions/B': {status: 200, body: {}},
  });
  await context.openSessionMovePicker('B');
  const choice = choiceButton(document, 'Session A');
  choice.click();
  choice.click();
  await settle();
  const patches = calls.filter((call) => call.method === 'PATCH');
  assert.equal(patches.length, 1, 'the second click joins the first pick, so one PATCH leaves');
  assert.equal(patches[0].url, '/api/sessions/B');
  assert.deepEqual(JSON.parse(patches[0].body), {task_parent_id: 'A'});
});

test('acceptance 4: a 200 closes the dialog and the row refresh renders under the new parent', async () => {
  const {context, document, nav, calls} = buildContext({
    ...LIST_ANSWERS,
    'PATCH /api/sessions/B': {status: 200, body: {}},
  });
  context.renderSessionList([row('A', 'manager'), row('B', 'manager', 'P')], 'all');
  await context.openSessionMovePicker('B');
  choiceButton(document, 'Session A').click();
  await settle();
  assert.equal(pickerParts(document), null, 'the dialog closes');

  const movedRow = row('B', 'manager', 'A');
  assert.equal(context.mergeRowsIntoRenderedList([movedRow], []), true, 'the existing row refresh path repaints');
  const parentChildren = nav.innerHTML.indexOf('data-tree-children="A"');
  const movedSession = nav.innerHTML.indexOf('id="session-B"');
  assert.ok(parentChildren >= 0 && movedSession > parentChildren, 'the refreshed row is nested below its new parent');

  const patchAt = calls.findIndex((call) => call.method === 'PATCH');
  assert.equal(calls.slice(patchAt + 1).length, 0, 'no list is read after the move');
});

test('acceptance 5: a 409 keeps the dialog open and lists each blocker verbatim', async () => {
  const blockers = ['D: run r-1 is active', 'the target has a closed parent'];
  const {context, document, calls} = buildContext({
    ...LIST_ANSWERS,
    'PATCH /api/sessions/B': {status: 409, body: {detail: {message: 'refused', blockers}}},
  });
  await context.openSessionMovePicker('B');
  choiceButton(document, 'Session A').click();
  await settle();
  const parts = pickerParts(document);
  assert.ok(parts, 'the dialog stays open');
  assert.deepEqual(parts.errors.children.map((line) => line.textContent), blockers);
  choiceButton(document, 'Session A').click();
  await settle();
  assert.equal(calls.filter((call) => call.method === 'PATCH').length, 2, 'a refused pick can be picked again');
});

test('acceptance 6: Top level leads for a session with a parent and sends null; not for a top-level one', async () => {
  const {context, document, calls} = buildContext({
    ...LIST_ANSWERS,
    'PATCH /api/sessions/B': {status: 200, body: {}},
  });
  await context.openSessionMovePicker('B');
  assert.equal(choiceLabels(document)[0], 'Top level', 'the first item of a session with a parent');
  choiceButton(document, 'Top level').click();
  await settle();
  const patch = calls.find((call) => call.method === 'PATCH');
  assert.deepEqual(JSON.parse(patch.body), {task_parent_id: null});

  await context.openSessionMovePicker('A');
  assert.ok(!choiceLabels(document).includes('Top level'), 'a top-level session has no Top level item');
});

test('acceptance 7: the consequence text is on screen before any pick, word for word', async () => {
  const {context, document} = buildContext(LIST_ANSWERS);
  const opening = context.openSessionMovePicker('B');
  assert.equal(pickerParts(document).footer.textContent, PLAN_FOOTER, 'the footer shows before the list loads');
  await opening;
  assert.equal(pickerParts(document).footer.textContent, PLAN_FOOTER, 'the footer stays after the list loads');
});
