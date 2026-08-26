// ---------------------------------------------------------------------------
// Element/class-list stub for the artifact-comments vm tests (no jsdom): the
// superset of every DOM member web/static/js/artifact-comments.js touches, so
// a member the script reads but the stub lacks throws inside the vm and fails
// the loading test. Until tests/artifact_comments.test.js switches its inline
// copy over to this module, every edit here must be mirrored into that file's
// makeClassList/makeElement/findChildByClass/dockOf.
// ---------------------------------------------------------------------------

const assert = require('node:assert/strict');

function makeClassList() {
  const classes = new Set();
  return {
    add(...tokens) { for (const t of tokens) classes.add(t); },
    remove(...tokens) { for (const t of tokens) classes.delete(t); },
    contains(token) { return classes.has(token); },
    toggle(token, force) {
      if (force === true) { classes.add(token); return true; }
      if (force === false) { classes.delete(token); return false; }
      if (classes.has(token)) { classes.delete(token); return false; }
      classes.add(token); return true;
    },
  };
}

function makeElement() {
  return {
    _textContent: '',
    _innerHTML: '',
    _listeners: {},
    attributes: {},
    style: {},
    children: [],
    childNodes: [],
    nodeType: 1,
    tagName: 'DIV',
    className: '',
    parentNode: null,
    parentElement: null,
    display: 'block',
    offsetHeight: 40,
    offsetWidth: 300,
    get textContent() {
      if (this.childNodes.length === 0) return this._textContent;
      return this.childNodes.map((node) => node.textContent || '').join('');
    },
    set textContent(value) {
      this._textContent = String(value);
      this.childNodes = [];
      this.children = [];
      this._innerHTML = '';
    },
    get innerHTML() {
      return this._innerHTML;
    },
    set innerHTML(value) {
      this._innerHTML = String(value);
      if (this._innerHTML === '') {
        this.children = [];
        this.childNodes = [];
      }
    },
    get innerText() {
      return this.textContent;
    },
    classList: makeClassList(),
    closest() { return null; },
    appendChild(child) {
      const prior = child.parentNode;
      if (prior) {
        const ci = prior.children.indexOf(child);
        if (ci !== -1) prior.children.splice(ci, 1);
        const ni = prior.childNodes.indexOf(child);
        if (ni !== -1) prior.childNodes.splice(ni, 1);
      }
      this.children.push(child);
      this.childNodes.push(child);
      child.parentNode = this;
      child.parentElement = this;
      return child;
    },
    replaceChild(next, prev) {
      const index = this.children.indexOf(prev);
      assert.notEqual(index, -1, 'replaceChild target exists');
      this.children[index] = next;
      const nodeIndex = this.childNodes.indexOf(prev);
      if (nodeIndex !== -1) this.childNodes[nodeIndex] = next;
      next.parentNode = this;
      next.parentElement = this;
      prev.parentNode = null;
      prev.parentElement = null;
      return prev;
    },
    removeChild(child) {
      const index = this.children.indexOf(child);
      if (index !== -1) this.children.splice(index, 1);
      const nodeIndex = this.childNodes.indexOf(child);
      if (nodeIndex !== -1) this.childNodes.splice(nodeIndex, 1);
      child.parentNode = null;
      child.parentElement = null;
      return child;
    },
    addEventListener(type, handler) {
      if (!this._listeners[type]) this._listeners[type] = [];
      this._listeners[type].push(handler);
    },
    setAttribute(name, value = '') {
      this.attributes[name] = String(value);
    },
    focus() {},
    select() {},
    getBoundingClientRect() {
      return {left: 0, top: 0, right: 0, bottom: 0, width: 0, height: 0};
    },
    scrollIntoView() {},
    querySelector(selector) {
      if (!selector.startsWith('.')) return null;
      const targetClass = selector.slice(1);
      const stack = this.children.slice();
      while (stack.length > 0) {
        const child = stack.shift();
        const classes = String(child.className || '').split(/\s+/);
        if (classes.includes(targetClass)) return child;
        stack.push(...child.children);
      }
      return null;
    },
    // The comment layer measures layout at script load (placeColumn ->
    // measureContentRight reads document.body.querySelectorAll('*')), so the
    // stub must answer the '*' walk, not only class selectors.
    querySelectorAll(selector) {
      const all = [];
      const stack = this.children.slice();
      while (stack.length > 0) {
        const child = stack.shift();
        all.push(child);
        stack.push(...child.children);
      }
      if (selector === '*') return all;
      if (!selector.startsWith('.')) return [];
      const targetClass = selector.slice(1);
      return all.filter((el) => String(el.className || '').split(/\s+/).includes(targetClass));
    },
  };
}

function findChildByClass(parent, className) {
  return parent.children.find((child) => child.className === className);
}

function dockOf(body) {
  return findChildByClass(body, '__cbc-dock');
}

module.exports = {makeElement, findChildByClass, dockOf};
