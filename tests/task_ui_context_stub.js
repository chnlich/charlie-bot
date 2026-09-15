// ---------------------------------------------------------------------------
// vm-context harness for the v2 task UI modules (sidebar/session-tree.js,
// task-panel.js, task-context.js, task-runs-panel.js): a compact fake DOM
// covering exactly the document/element members those modules touch, a fetch
// recorder the tests answer per-URL, and the shared sidebar namespace loaded
// first (the page's script order).
// ---------------------------------------------------------------------------
const vm = require('node:vm');
const {readStatic} = require('./read_static');
const {escapeHtmlText} = require('./escape_html_stub');

const NAMESPACE_JS = readStatic('sidebar/namespace.js');

function matchesPart(el, part) {
  let rest = part;
  const tagMatch = rest.match(/^[a-zA-Z][a-zA-Z0-9-]*/);
  if (tagMatch) {
    if (el.tagName !== tagMatch[0].toUpperCase()) return false;
    rest = rest.slice(tagMatch[0].length);
  }
  const attrRe = /\[([a-zA-Z-]+)(?:="([^"]*)")?\]/g;
  let m;
  const classes = [];
  let id = null;
  const classRe = /\.([a-zA-Z0-9_-]+)/g;
  while ((m = classRe.exec(rest)) !== null) classes.push(m[1]);
  const idRe = /#([a-zA-Z0-9_-]+)/;
  const idMatch = rest.match(idRe);
  if (idMatch) id = idMatch[1];
  while ((m = attrRe.exec(rest)) !== null) {
    const value = el.getAttribute(m[1]);
    if (value === undefined || value === null) return false;
    if (m[2] !== undefined && String(value) !== m[2]) return false;
  }
  if (id && el.id !== id) return false;
  for (const c of classes) {
    if (!el.classes.has(c)) return false;
  }
  return true;
}

// CSS descendant-combinator matching: matchChain records el when it matches
// parts[0] (whole chain done) and otherwise continues the remaining parts
// among el's descendants; matchChainDeep lets the remaining chain start at el
// or anywhere below it.
function matchChain(el, parts, out) {
  if (!matchesPart(el, parts[0])) return;
  if (parts.length === 1) { out.push(el); return; }
  for (const child of el.children) matchChainDeep(child, parts.slice(1), out);
}

function matchChainDeep(el, parts, out) {
  matchChain(el, parts, out);
  for (const child of el.children) matchChainDeep(child, parts, out);
}

function matchDescendants(root, parts, out) {
  for (const child of root.children) matchChainDeep(child, parts, out);
}

function camelToDash(name) {
  return 'data-' + String(name).replace(/[A-Z]/g, (m) => '-' + m.toLowerCase());
}

class FakeElement {
  constructor(tag, doc) {
    this.tagName = String(tag).toUpperCase();
    this.children = [];
    this.parent = null;
    this._doc = doc;
    const self = this;
    this.dataset = new Proxy({}, {
      get(_t, key) { return self._attrs[camelToDash(key)]; },
      set(_t, key, value) { self._attrs[camelToDash(key)] = String(value); return true; },
      has(_t, key) { return camelToDash(key) in self._attrs; },
      deleteProperty(_t, key) { delete self._attrs[camelToDash(key)]; return true; },
    });
    this.style = {};
    this.classes = new Set();
    this._text = '';
    this._attrs = {};
    this.id = '';
    this.tabIndex = 0;
    this._listeners = {};
    this.value = '';
    this.checked = false;
    this.disabled = false;
    this.selected = false;
    this.options = [];
    this._html = '';
  }
  get className() { return [...this.classes].join(' '); }
  set className(v) { this.classes = new Set(String(v || '').split(/\s+/).filter(Boolean)); }
  get classList() {
    const self = this;
    return {
      add: (...c) => c.forEach((x) => x && self.classes.add(x)),
      remove: (...c) => c.forEach((x) => self.classes.delete(x)),
      contains: (c) => self.classes.has(c),
      toggle: (c, f) => {
        if (f === undefined) { if (self.classes.has(c)) { self.classes.delete(c); return false; } self.classes.add(c); return true; }
        if (f) self.classes.add(c); else self.classes.delete(c);
        return !!f;
      },
      toString: () => [...self.classes].join(' '),
    };
  }
  setAttribute(name, value) {
    if (name === 'class') { this.className = value; return; }
    this._attrs[name] = String(value);
    if (name === 'id') this.id = String(value);
  }
  getAttribute(name) {
    if (name === 'class') return this.className;
    if (name === 'id') return this.id || null;
    return this._attrs[name] !== undefined ? this._attrs[name] : null;
  }
  removeAttribute(name) { delete this._attrs[name]; }
  appendChild(child) {
    if (child.parent) child.parent.removeChild(child);
    child.parent = this;
    this.children.push(child);
    if (child.id) this._doc.register(child);
    if (child.tagName === 'OPTION') {
      this.options.push(child);
      if (child.selected || !this.value) this.value = child.value;
    }
    return child;
  }
  prepend(child) {
    if (child.parent) child.parent.removeChild(child);
    child.parent = this;
    this.children.unshift(child);
    if (child.id) this._doc.register(child);
    if (child.tagName === 'OPTION') {
      this.options.unshift(child);
      if (child.selected || !this.value) this.value = child.value;
    }
    return child;
  }
  insertBefore(child, ref) {
    if (child.parent) child.parent.removeChild(child);
    child.parent = this;
    const i = this.children.indexOf(ref);
    if (i === -1) this.children.push(child);
    else this.children.splice(i, 0, child);
    if (child.id) this._doc.register(child);
    return child;
  }
  after(child) {
    if (!this.parent) throw new Error('after() on an unparented element');
    if (child.parent) child.parent.removeChild(child);
    const i = this.parent.children.indexOf(this);
    child.parent = this.parent;
    this.parent.children.splice(i + 1, 0, child);
    if (child.id) this._doc.register(child);
    return child;
  }
  remove() {
    if (this.parent) {
      const i = this.parent.children.indexOf(this);
      if (i >= 0) this.parent.children.splice(i, 1);
      this.parent = null;
    }
    this._doc.deregister(this);
  }
  // Reflecting properties the task UI modules set directly (real DOM
  // semantics: input.type, a.href, etc. are attributes).
  get type() { return this._attrs.type; }
  set type(v) { this._attrs.type = String(v); }
  get name() { return this._attrs.name; }
  set name(v) { this._attrs.name = String(v); }
  get href() { return this._attrs.href; }
  set href(v) { this._attrs.href = String(v); }
  get title() { return this._attrs.title; }
  set title(v) { this._attrs.title = String(v); }
  get placeholder() { return this._attrs.placeholder; }
  set placeholder(v) { this._attrs.placeholder = String(v); }
  get firstElementChild() { return this.children[0] || null; }
  get parentNode() { return this.parent; }
  get parentElement() { return this.parent; }
  set textContent(v) {
    this._text = String(v === null || v === undefined ? '' : v);
    this.children = [];
    this._html = escapeHtmlText(this._text);
  }
  get textContent() {
    return this._text + this.children.map((c) => c.textContent).join('');
  }
  get innerHTML() { return this._html + this.children.map((c) => c.innerHTML).join(''); }
  set innerHTML(html) { this._html = String(html); this.children = []; }
  addEventListener(type, handler) { (this._listeners[type] = this._listeners[type] || []).push(handler); }
  removeEventListener(type, handler) {
    this._listeners[type] = (this._listeners[type] || []).filter((h) => h !== handler);
  }
  dispatch(type, event) {
    for (const h of (this._listeners[type] || []).slice()) h(event || {});
  }
  querySelector(sel) { return this.querySelectorAll(sel)[0] || null; }
  querySelectorAll(sel) {
    const parts = String(sel).trim().split(/\s+/);
    const out = [];
    matchDescendants(this, parts, out);
    return out;
  }
  closest(sel) {
    const parts = String(sel).trim().split(/\s+/);
    let cur = this;
    while (cur) {
      if (matchesPart(cur, parts[0])) return cur;
      cur = cur.parent;
    }
    return null;
  }
  focus() { this._doc.focused = this; }
  scrollIntoView() { this._doc.scrolledTo = this; }
  click() { this.dispatch('click', {target: this, closest: (s) => this.closest(s), stopPropagation: () => {}}); }
}

class FakeDocument {
  constructor() {
    this.byId = new Map();
    this.body = this.createElement('body');
    this.focused = null;
    this.scrolledTo = null;
  }
  register(el) { if (el.id) this.byId.set(el.id, el); }
  deregister(el) {
    if (this.byId.get(el.id) === el) this.byId.delete(el.id);
  }
  createElement(tag) { return new FakeElement(tag, this); }
  createDocumentFragment() {
    const frag = new FakeElement('div', this);
    frag._isFragment = true;
    return frag;
  }
  getElementById(id) { return this.byId.get(id) || null; }
  querySelectorAll(sel) {
    const parts = String(sel).trim().split(/\s+/);
    const out = [];
    for (const root of [...this.body.children, ...[...this.byId.values()].filter((e) => !e.parent)]) {
      if (matchesPart(root, parts[0])) out.push(root);
      matchDescendants(root, parts, out);
    }
    return [...new Set(out)].filter((el) => {
      // scope the id-anchored selectors to the anchor's subtree
      if (parts[0].startsWith('#')) {
        const anchor = this.byId.get(parts[0].slice(1));
        if (!anchor) return false;
        let cur = el;
        while (cur) { if (cur === anchor) return true; cur = cur.parent; }
      }
      return true;
    });
  }
  querySelector(sel) { return this.querySelectorAll(sel)[0] || null; }
  addEventListener() {}
  removeEventListener() {}
}

// Load the sidebar namespace then one task-UI module, in the page's order.
function loadModules(context, files) {
  context.globalThis = context;
  vm.createContext(context);
  vm.runInContext(NAMESPACE_JS, context, {filename: 'sidebar/namespace.js'});
  for (const file of files) {
    vm.runInContext(readStatic(file), context, {filename: file});
  }
}

function buildContext(overrides = {}) {
  const doc = new FakeDocument();
  const fetchCalls = [];
  const fetchHandlers = [];
  const context = {
    console: {error: () => {}, log: () => {}, warn: () => {}},
    document: doc,
    localStorage: {store: new Map(), getItem(k) { return this.store.has(k) ? this.store.get(k) : null; }, setItem(k, v) { this.store.set(k, String(v)); }, removeItem(k) { this.store.delete(k); }},
    location: {href: '', protocol: 'http:', host: 'localhost:8000', search: '', origin: 'http://localhost:8000'},
    history: {pushState: () => {}},
    URLSearchParams,
    AbortController,
    crypto: {randomUUID: () => 'req-' + Math.random().toString(16).slice(2)},
    setTimeout: (fn) => { fn(); return 0; },
    clearTimeout: () => {},
    JSON_HEADERS: {'Content-Type': 'application/json'},
    BACKEND_OPTIONS: {'fake-backend': 'Fake', 'other-backend': 'Other'},
    BACKEND_TYPES: {},
    escapeHtml: escapeHtmlText,
    escapeHtmlAttr: escapeHtmlText,
    showToast: (msg) => { context._toasts.push(msg); },
    switchSession: async (id) => { context._switchCalls.push(id); },
    switchTab: (tab) => { context._tabCalls.push(tab); },
    switchSidebarFilter: (f) => { context.currentFilter = f; },
    _toasts: [],
    _switchCalls: [],
    _tabCalls: [],
    fetch: async (url, opts) => {
      fetchCalls.push({url, opts: opts || {}});
      for (const handler of fetchHandlers) {
        const res = handler(url, opts || {});
        if (res !== undefined) return res;
      }
      throw new Error('no fetch handler for ' + url);
    },
  };
  context.currentFilter = overrides.currentFilter || 'tasks';
  context.SESSION_ID = 'SESSION_ID';
  Object.defineProperty(context, 'SESSION_ID', {
    get() { return context._sessionId; },
    set(v) { context._sessionId = v; },
    configurable: true,
  });
  context._sessionId = overrides.sessionId !== undefined ? overrides.sessionId : 'root-1';
  Object.assign(context, overrides);
  context.fetchCalls = fetchCalls;
  context.fetchHandlers = fetchHandlers;
  context.__doc = doc;
  return context;
}

function jsonResponse(body, status = 200) {
  return {ok: status >= 200 && status < 300, status, json: async () => body};
}

function row(overrides = {}) {
  return Object.assign({
    id: 'node-' + Math.random().toString(16).slice(2, 8),
    name: 'Task',
    profile: 'manager',
    task_parent_id: null,
    task_state: 'open',
    work_state: 'idle',
    archived: false,
    child_count: 0,
    open_descendant_count: 0,
    attention_descendant_count: 0,
  }, overrides);
}

module.exports = {buildContext, loadModules, jsonResponse, row, FakeDocument};
