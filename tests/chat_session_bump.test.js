const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const CHAT_JS = fs.readFileSync(
  path.join(__dirname, '..', 'web', 'static', 'js', 'chat.js'),
  'utf8'
);

class FakeClassList {
  constructor(initial = '') {
    this._classes = new Set(String(initial).split(/\s+/).filter(Boolean));
  }

  add(...classes) {
    for (const className of classes) this._classes.add(className);
  }

  remove(...classes) {
    for (const className of classes) this._classes.delete(className);
  }

  contains(className) {
    return this._classes.has(className);
  }

  toggle(className, force) {
    if (force === undefined) {
      if (this._classes.has(className)) {
        this._classes.delete(className);
        return false;
      }
      this._classes.add(className);
      return true;
    }
    if (force) this._classes.add(className);
    else this._classes.delete(className);
    return !!force;
  }

  toString() {
    return Array.from(this._classes).join(' ');
  }
}

class FakeElement {
  constructor(tagName = 'DIV', {id = '', className = ''} = {}) {
    this.tagName = String(tagName).toUpperCase();
    this.id = id;
    this.dataset = {};
    this.attributes = new Map();
    this.textContent = '';
    this.parentElement = null;
    this.parentNode = null;
    this.children = [];
    this.classList = new FakeClassList(className);
    this._className = className;
    this.innerHTML = '';
  }

  get className() {
    return this.classList.toString();
  }

  set className(value) {
    this._className = String(value || '');
    this.classList = new FakeClassList(this._className);
  }

  appendChild(child) {
    if (child.parentElement) {
      child.parentElement.removeChild(child);
    }
    child.parentElement = this;
    child.parentNode = this;
    this.children.push(child);
    return child;
  }

  removeChild(child) {
    const index = this.children.indexOf(child);
    if (index === -1) {
      throw new Error('child not found');
    }
    this.children.splice(index, 1);
    child.parentElement = null;
    child.parentNode = null;
    return child;
  }

  insertBefore(child, referenceChild) {
    if (child.parentElement) {
      child.parentElement.removeChild(child);
    }
    child.parentElement = this;
    child.parentNode = this;
    if (!referenceChild) {
      this.children.push(child);
      return child;
    }
    const index = this.children.indexOf(referenceChild);
    if (index === -1) {
      throw new Error('reference child not found');
    }
    this.children.splice(index, 0, child);
    return child;
  }

  remove() {
    if (this.parentElement) this.parentElement.removeChild(this);
  }

  get firstElementChild() {
    return this.children[0] || null;
  }

  get firstChild() {
    return this.children[0] || null;
  }

  get nextElementSibling() {
    if (!this.parentElement) return null;
    const siblings = this.parentElement.children;
    const index = siblings.indexOf(this);
    return index === -1 ? null : (siblings[index + 1] || null);
  }

  get previousElementSibling() {
    if (!this.parentElement) return null;
    const siblings = this.parentElement.children;
    const index = siblings.indexOf(this);
    return index <= 0 ? null : siblings[index - 1];
  }

  querySelector(selector) {
    if (!selector.startsWith('.')) {
      throw new Error(`Unsupported selector: ${selector}`);
    }
    const className = selector.slice(1);
    for (const child of this.children) {
      if (child.classList.contains(className)) {
        return child;
      }
      const nested = child.querySelector(selector);
      if (nested) return nested;
    }
    return null;
  }

  querySelectorAll(selector) {
    if (!selector.startsWith('.')) {
      throw new Error(`Unsupported selector: ${selector}`);
    }
    const className = selector.slice(1);
    const matches = [];
    for (const child of this.children) {
      if (child.classList.contains(className)) matches.push(child);
      matches.push(...child.querySelectorAll(selector));
    }
    return matches;
  }

  closest(selector) {
    if (!selector.startsWith('.')) {
      throw new Error(`Unsupported selector: ${selector}`);
    }
    const className = selector.slice(1);
    let current = this;
    while (current) {
      if (current.classList.contains(className)) {
        return current;
      }
      current = current.parentElement;
    }
    return null;
  }

  setAttribute(name, value) {
    this.attributes.set(name, String(value));
  }

  getAttribute(name) {
    return this.attributes.get(name) || null;
  }
}

function createSession(id, timeIso, timeText) {
  const session = new FakeElement('A', {id: `session-${id}`});
  const time = new FakeElement('SPAN', {className: 'session-time'});
  time.dataset.time = timeIso;
  time.textContent = timeText;
  session.appendChild(time);
  return session;
}

function loadChatContext(document) {
  const nowIso = '2026-04-02T03:04:05.000Z';
  const context = {
    SESSION_ID: 'session-a',
    document: {
      addEventListener() {},
      createElement() {
        return new FakeElement();
      },
      ...document,
    },
    console: {error: () => {}},
    relativeTime: (iso) => `relative:${iso}`,
    window: {addEventListener() {}},
    Date: class FakeDate {
      toISOString() {
        return nowIso;
      }
    },
  };

  vm.createContext(context);
  vm.runInContext(CHAT_JS, context, {filename: 'chat.js'});
  return {context, nowIso};
}

function messageElement(role, id) {
  const el = new FakeElement('DIV');
  el.dataset.messageRole = role;
  el.dataset.messageId = id;
  return el;
}

function childRoles(root) {
  return root.children.map((child) => child.dataset.messageRole || child.className);
}

test('bumpCurrentSessionToTop keeps grouped sessions inside their current group', () => {
  const nav = new FakeElement('DIV', {id: 'session-list'});
  const group = new FakeElement('DIV', {className: 'session-group'});
  const toggle = new FakeElement('DIV');
  const items = new FakeElement('DIV', {className: 'session-group-items'});
  const before = createSession('session-b', '2026-04-01T00:00:00.000Z', 'old-b');
  const current = createSession('session-a', '2026-04-01T01:00:00.000Z', 'old-a');
  const after = createSession('session-c', '2026-04-01T02:00:00.000Z', 'old-c');

  items.appendChild(before);
  items.appendChild(current);
  items.appendChild(after);
  group.appendChild(toggle);
  group.appendChild(items);
  nav.appendChild(group);

  const {context, nowIso} = loadChatContext({
    getElementById(id) {
      if (id === 'session-list') return nav;
      if (id === 'session-session-a') return current;
      return null;
    },
  });

  context.bumpCurrentSessionToTop();

  assert.equal(current.parentElement, items);
  assert.deepEqual(items.children.map((child) => child.id), [
    'session-session-a',
    'session-session-b',
    'session-session-c',
  ]);
  assert.equal(nav.firstElementChild, group);
  assert.equal(current.querySelector('.session-time').dataset.time, nowIso);
  assert.equal(current.querySelector('.session-time').textContent, `relative:${nowIso}`);
});

test('bumpCurrentSessionToTop moves flat sidebar sessions to the top-level front', () => {
  const nav = new FakeElement('DIV', {id: 'session-list'});
  const before = createSession('session-b', '2026-04-01T00:00:00.000Z', 'old-b');
  const current = createSession('session-a', '2026-04-01T01:00:00.000Z', 'old-a');
  const after = createSession('session-c', '2026-04-01T02:00:00.000Z', 'old-c');

  nav.appendChild(before);
  nav.appendChild(current);
  nav.appendChild(after);

  const {context, nowIso} = loadChatContext({
    getElementById(id) {
      if (id === 'session-list') return nav;
      if (id === 'session-session-a') return current;
      return null;
    },
  });

  context.bumpCurrentSessionToTop();

  assert.equal(current.parentElement, nav);
  assert.deepEqual(nav.children.map((child) => child.id), [
    'session-session-a',
    'session-session-b',
    'session-session-c',
  ]);
  assert.equal(current.querySelector('.session-time').dataset.time, nowIso);
  assert.equal(current.querySelector('.session-time').textContent, `relative:${nowIso}`);
});

test('applyCompactMode folds all completed turns and leaves live turn expanded', () => {
  const root = new FakeElement('DIV');
  const {context} = loadChatContext({
    getElementById() {
      return null;
    },
  });

  [
    messageElement('user', 'u1'),
    messageElement('assistant', 'a1'),
    messageElement('system', 's1'),
    messageElement('assistant', 'a2'),
    messageElement('worker_summary', 'w1'),
    messageElement('plan', 'p1'),
    messageElement('separator', 'sep1'),
    messageElement('user', 'u2'),
    messageElement('assistant', 'a3'),
    messageElement('system', 's2'),
    messageElement('assistant', 'a4'),
    messageElement('separator', 'sep2'),
    messageElement('user', 'u3'),
    messageElement('assistant', 'a5'),
    messageElement('system', 's3'),
    messageElement('assistant', 'a6'),
  ].forEach((el) => root.appendChild(el));

  context.applyCompactMode(root);

  assert.deepEqual(childRoles(root), [
    'user',
    'turn-fold-bar',
    'turn-fold-content space-y-3 hidden',
    'assistant',
    'worker_summary',
    'plan',
    'separator',
    'user',
    'turn-fold-bar',
    'turn-fold-content space-y-3 hidden',
    'assistant',
    'separator',
    'user',
    'assistant',
    'system',
    'assistant',
  ]);
  assert.equal(root.querySelectorAll('.turn-fold-bar').length, 2);
  assert.deepEqual(root.children[2].children.map((child) => child.dataset.messageId), ['a1', 's1']);
  assert.equal(root.children[2].classList.contains('hidden'), true);
  assert.deepEqual(root.children[9].children.map((child) => child.dataset.messageId), ['a3', 's2']);
  assert.equal(root.children[9].classList.contains('hidden'), true);
  assert.deepEqual(root.children.slice(12).map((child) => child.dataset.messageId), ['u3', 'a5', 's3', 'a6']);

  context.applyCompactMode(root);

  assert.equal(root.querySelectorAll('.turn-fold-bar').length, 2);
  assert.deepEqual(root.children[2].children.map((child) => child.dataset.messageId), ['a1', 's1']);
  assert.deepEqual(root.children[9].children.map((child) => child.dataset.messageId), ['a3', 's2']);
  assert.deepEqual(root.children.slice(12).map((child) => child.dataset.messageId), ['u3', 'a5', 's3', 'a6']);
});

test('turn fold bar toggles its single intermediate span', () => {
  const root = new FakeElement('DIV');
  const {context} = loadChatContext({
    getElementById() {
      return null;
    },
  });

  [
    messageElement('user', 'u1'),
    messageElement('assistant', 'a1'),
    messageElement('assistant', 'a2'),
    messageElement('separator', 'sep1'),
  ].forEach((el) => root.appendChild(el));

  context.applyCompactMode(root);
  const bar = root.querySelector('.turn-fold-bar');
  const content = root.querySelector('.turn-fold-content');

  assert.equal(content.classList.contains('hidden'), true);
  assert.equal(bar.getAttribute('aria-expanded'), 'false');

  context.toggleTurnFold(bar);

  assert.equal(content.classList.contains('hidden'), false);
  assert.equal(bar.getAttribute('aria-expanded'), 'true');
});

test('top-bar compact action expands and collapses all completed turns', () => {
  const root = new FakeElement('DIV');
  const compactButton = new FakeElement('BUTTON');
  const {context} = loadChatContext({
    getElementById(id) {
      if (id === 'messages') return root;
      if (id === 'compact-mode-toggle') return compactButton;
      return null;
    },
  });

  [
    messageElement('user', 'u1'),
    messageElement('assistant', 'a1'),
    messageElement('assistant', 'a2'),
    messageElement('separator', 'sep1'),
    messageElement('user', 'u2'),
    messageElement('assistant', 'a3'),
    messageElement('assistant', 'a4'),
    messageElement('separator', 'sep2'),
  ].forEach((el) => root.appendChild(el));

  context.applyCompactMode(root);
  assert.equal(root.querySelectorAll('.turn-fold-content').length, 2);
  assert.equal(root.querySelectorAll('.turn-fold-content').every((content) => content.classList.contains('hidden')), true);
  assert.equal(compactButton.textContent, 'Expand all');
  assert.equal(compactButton.getAttribute('title'), 'Expand collapsed turns');

  context.toggleCompactMode();
  assert.equal(root.querySelectorAll('.turn-fold-content').length, 2);
  assert.equal(root.querySelectorAll('.turn-fold-content').every((content) => !content.classList.contains('hidden')), true);
  assert.equal(compactButton.textContent, 'Compact');
  assert.equal(compactButton.getAttribute('title'), 'Collapse completed turns');

  context.toggleCompactMode();
  assert.equal(root.querySelectorAll('.turn-fold-content').length, 2);
  assert.equal(root.querySelectorAll('.turn-fold-content').every((content) => content.classList.contains('hidden')), true);
  assert.equal(compactButton.textContent, 'Expand all');
  assert.equal(compactButton.getAttribute('title'), 'Expand collapsed turns');
});

test('applyCompactMode waits for a paginated turn head before folding a leading partial turn', () => {
  const root = new FakeElement('DIV');
  const {context} = loadChatContext({
    getElementById() {
      return null;
    },
  });

  const firstAssistant = messageElement('assistant', 'a1');
  [
    firstAssistant,
    messageElement('system', 's1'),
    messageElement('assistant', 'a2'),
    messageElement('separator', 'sep1'),
  ].forEach((el) => root.appendChild(el));

  context.applyCompactMode(root);

  assert.equal(root.querySelectorAll('.turn-fold-bar').length, 0);

  root.insertBefore(messageElement('user', 'u1'), firstAssistant);
  context.applyCompactMode(root);

  assert.equal(root.querySelectorAll('.turn-fold-bar').length, 1);
  assert.deepEqual(root.querySelector('.turn-fold-content').children.map((child) => child.dataset.messageId), ['a1', 's1']);
  assert.equal(root.querySelector('.turn-fold-content').classList.contains('hidden'), true);
});
