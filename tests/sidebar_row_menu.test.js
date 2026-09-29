// ---------------------------------------------------------------------------
// The sidebar's shared settings popover (sidebar/row-menu.js): one .row-menu
// at a time on document.body, the item/separator/danger markup and roles,
// close-then-onSelect on choose, the close paths (Escape, outside pointerdown,
// second open, anchor toggle, session-list scroll, window resize), and the
// placement -- under the anchor's right edge, clamped inside the viewport,
// flipped above near the viewport bottom. Harness mirrors
// sidebar_group_create_button.test.js: namespace.js + row-menu.js in one vm
// context over a minimal document/window stub.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const {readStatic} = require('./read_static');

// The menu root is the only element row-menu.js measures.
const MENU_RECT = {top: 0, bottom: 80, left: 0, right: 120, width: 120, height: 80};

function listenerHost() {
  const listeners = {};
  return {
    addEventListener(type, handler) { (listeners[type] = listeners[type] || []).push(handler); },
    removeEventListener(type, handler) {
      const list = listeners[type] || [];
      const index = list.indexOf(handler);
      if (index !== -1) list.splice(index, 1);
    },
    fire(type, event) { (listeners[type] || []).slice().forEach((handler) => handler(event)); },
    activeListeners(type) { return (listeners[type] || []).length; },
  };
}

function makeElement(tagName, rect) {
  return Object.assign(listenerHost(), {
    tagName: String(tagName).toUpperCase(),
    children: [],
    parentElement: null,
    className: '',
    style: {},
    type: '',
    setAttribute(name, value) { this['_attr_' + name] = String(value); },
    getAttribute(name) { return this['_attr_' + name] == null ? null : this['_attr_' + name]; },
    appendChild(child) { child.parentElement = this; this.children.push(child); return child; },
    remove() {
      if (this.parentElement) {
        const siblings = this.parentElement.children;
        const index = siblings.indexOf(this);
        if (index !== -1) siblings.splice(index, 1);
      }
      this.parentElement = null;
    },
    contains(node) {
      let cursor = node;
      while (cursor) {
        if (cursor === this) return true;
        cursor = cursor.parentElement;
      }
      return false;
    },
    getBoundingClientRect: () => rect || {top: 0, bottom: 0, left: 0, right: 0, width: 0, height: 0},
  });
}

function buildContext({innerWidth = 640, innerHeight = 800} = {}) {
  const body = makeElement('body');
  const sessionList = makeElement('div');
  const doc = Object.assign(listenerHost(), {
    body,
    sessionList,
    createElement: (tag) => makeElement(tag, MENU_RECT),
    getElementById: (id) => (id === 'session-list' ? sessionList : null),
  });
  const win = Object.assign(listenerHost(), {innerWidth, innerHeight});
  const context = {document: doc, window: win, console};
  context.globalThis = context;
  vm.createContext(context);
  // namespace.js first, as on the page: it supplies Sidebar.wire.
  vm.runInContext(readStatic('sidebar/namespace.js'), context, {filename: 'namespace.js'});
  vm.runInContext(readStatic('sidebar/row-menu.js'), context, {filename: 'row-menu.js'});
  return {context, doc, body, sessionList, win};
}

function makeAnchor(rect) {
  return makeElement('button', rect);
}

function assertClosed({body, doc, sessionList, win}) {
  assert.equal(body.children.length, 0, 'no .row-menu remains on the body');
  assert.equal(doc.activeListeners('pointerdown'), 0, 'the document pointerdown listener is gone');
  assert.equal(doc.activeListeners('keydown'), 0, 'the document keydown listener is gone');
  assert.equal(sessionList.activeListeners('scroll'), 0, 'the session-list scroll listener is gone');
  assert.equal(win.activeListeners('resize'), 0, 'the window resize listener is gone');
}

test('opening renders one .row-menu on the body; a second open replaces it', () => {
  const {context, body} = buildContext();

  context.openRowMenu(makeAnchor(), [{label: 'Rename', onSelect: () => {}}]);
  assert.equal(body.children.length, 1);
  const first = body.children[0];
  assert.equal(first.className, 'row-menu');
  assert.equal(first.getAttribute('role'), 'menu');

  context.openRowMenu(makeAnchor(), [{label: 'Archive', onSelect: () => {}}]);
  assert.equal(body.children.length, 1, 'at most one .row-menu exists');
  assert.equal(first.parentElement, null, 'the replaced menu left the body');
});

test('items render as role=menuitem buttons, separators as role=separator divs, danger gets the danger class', () => {
  const {context, body} = buildContext();

  context.openRowMenu(makeAnchor(), [
    {label: 'Rename', onSelect: () => {}},
    {separator: true},
    {label: 'Archive', onSelect: () => {}, danger: true},
  ]);

  const [item, sep, danger] = body.children[0].children;
  assert.equal(item.tagName, 'BUTTON');
  assert.equal(item.className, 'row-menu-item');
  assert.equal(item.getAttribute('role'), 'menuitem');
  assert.equal(item.textContent, 'Rename');
  assert.equal(sep.tagName, 'DIV');
  assert.equal(sep.className, 'row-menu-sep');
  assert.equal(sep.getAttribute('role'), 'separator');
  assert.equal(danger.tagName, 'BUTTON');
  assert.equal(danger.className, 'row-menu-item row-menu-item-danger');
  assert.equal(danger.getAttribute('role'), 'menuitem');
  assert.equal(danger.textContent, 'Archive');
});

test('choosing an item closes the menu, then calls its onSelect', () => {
  const {context, body, doc, sessionList, win} = buildContext();
  const events = [];

  context.openRowMenu(makeAnchor(), [{
    label: 'Rename',
    onSelect: () => events.push(['select', body.children.length]),
  }]);

  body.children[0].children[0].fire('click', {});

  assert.deepEqual(events, [['select', 0]], 'onSelect ran after the menu was already closed');
  assertClosed({body, doc, sessionList, win});
});

test('Escape closes the menu; other keys leave it open', () => {
  const {context, doc, body, sessionList, win} = buildContext();

  context.openRowMenu(makeAnchor(), [{label: 'Rename', onSelect: () => {}}]);

  doc.fire('keydown', {key: 'Enter'});
  assert.equal(body.children.length, 1, 'a non-Escape key does not close the menu');

  doc.fire('keydown', {key: 'Escape'});
  assertClosed({body, doc, sessionList, win});
});

test('pointerdown outside the menu closes it; inside the menu or on the anchor does not', () => {
  const {context, doc, body, sessionList, win} = buildContext();
  const anchor = makeAnchor();

  context.openRowMenu(anchor, [{label: 'Rename', onSelect: () => {}}]);
  const menu = body.children[0];

  doc.fire('pointerdown', {target: menu.children[0]});
  assert.equal(body.children.length, 1, 'a pointerdown on an item does not close early');
  doc.fire('pointerdown', {target: anchor});
  assert.equal(body.children.length, 1, 'the anchor pointerdown belongs to the toggle, not the outside close');

  doc.fire('pointerdown', {target: makeElement('div')});
  assertClosed({body, doc, sessionList, win});
});

test('opening from the anchor of the open menu toggles it closed', () => {
  const {context, body, doc, sessionList, win} = buildContext();
  const anchor = makeAnchor();
  const items = [{label: 'Rename', onSelect: () => {}}];

  context.openRowMenu(anchor, items);
  assert.equal(body.children.length, 1);
  context.openRowMenu(anchor, items);

  assertClosed({body, doc, sessionList, win});
});

test('scroll of #session-list and window resize close the menu', () => {
  const {context, sessionList, win, body, doc} = buildContext();
  const anchor = makeAnchor();
  const items = [{label: 'Rename', onSelect: () => {}}];

  context.openRowMenu(anchor, items);
  sessionList.fire('scroll', {});
  assertClosed({body, doc, sessionList, win});

  context.openRowMenu(anchor, items);
  win.fire('resize', {});
  assertClosed({body, doc, sessionList, win});
});

test('the menu sits under the anchor\'s right edge, clamped inside the viewport', () => {
  const {context, body} = buildContext({innerWidth: 640, innerHeight: 800});
  context.openRowMenu(makeAnchor({top: 100, bottom: 130, left: 10, right: 210}), [
    {label: 'Rename', onSelect: () => {}},
  ]);
  const menu = body.children[0];
  assert.equal(menu.style.top, '134px', '4px below the anchor bottom');
  assert.equal(menu.style.left, '90px', 'right edges aligned: 210 - 120');

  const clamped = buildContext({innerWidth: 640, innerHeight: 800});
  clamped.context.openRowMenu(makeAnchor({top: 100, bottom: 130, left: 560, right: 700}), [
    {label: 'Rename', onSelect: () => {}},
  ]);
  assert.equal(clamped.body.children[0].style.left, '512px', 'clamped to 640 - 120 - 8');
});

test('an anchor near the viewport bottom flips the menu above the anchor', () => {
  const {context, body} = buildContext({innerWidth: 640, innerHeight: 560});

  context.openRowMenu(makeAnchor({top: 500, bottom: 530, left: 10, right: 210}), [
    {label: 'Rename', onSelect: () => {}},
  ]);

  const menu = body.children[0];
  assert.equal(menu.style.top, '416px', '530 + 4 + 80 would pass 560; 500 - 80 - 4 does not');
  assert.equal(menu.style.left, '90px');
});
