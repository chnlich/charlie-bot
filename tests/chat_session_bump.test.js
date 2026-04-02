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
}

class FakeElement {
  constructor(tagName = 'DIV', {id = '', className = ''} = {}) {
    this.tagName = String(tagName).toUpperCase();
    this.id = id;
    this.dataset = {};
    this.textContent = '';
    this.parentElement = null;
    this.children = [];
    this.classList = new FakeClassList(className);
  }

  appendChild(child) {
    if (child.parentElement) {
      child.parentElement.removeChild(child);
    }
    child.parentElement = this;
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
    return child;
  }

  insertBefore(child, referenceChild) {
    if (child.parentElement) {
      child.parentElement.removeChild(child);
    }
    child.parentElement = this;
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

  get firstElementChild() {
    return this.children[0] || null;
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
        let text = '';
        return {
          set textContent(value) {
            text = String(value);
          },
          get innerHTML() {
            return text;
          },
        };
      },
      ...document,
    },
    console: {error: () => {}},
    relativeTime: (iso) => `relative:${iso}`,
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
