// ---------------------------------------------------------------------------
// Element/class-list stub and shared rig helpers for the artifact-comments vm
// tests (no jsdom): the superset of every DOM member
// web/features/artifacts/static/artifact-comments.js touches. A member the script reads but
// the stub lacks throws inside the vm and fails the loading test.
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
    // The fork lookup calls closest('.fork'): one class selector, matched against
    // this element and every ancestor. Other selector forms (tag lists, comma
    // groups) stay unanswered, as no script path that the tests drive reads them.
    closest(selector) {
      if (typeof selector !== 'string' || !selector.startsWith('.') || selector.includes(',') ||
          selector.includes(' ')) {
        return null;
      }
      const targetClass = selector.slice(1);
      let el = this;
      while (el) {
        if (String(el.className || '').split(/\s+/).includes(targetClass)) return el;
        el = el.parentElement;
      }
      return null;
    },
    appendChild(child) {
      const prior = child.parentNode;
      if (prior) {
        const ci = prior.children.indexOf(child);
        if (ci !== -1) prior.children.splice(ci, 1);
        const ni = prior.childNodes.indexOf(child);
        if (ni !== -1) prior.childNodes.splice(ni, 1);
      }
      // Real DOM semantics: only element children enter `children`; a text node
      // lives in `childNodes` alone, and CharacterData has no parentElement.
      this.childNodes.push(child);
      if (child.nodeType === 1) {
        this.children.push(child);
        child.parentElement = this;
      } else {
        child.parentElement = null;
      }
      child.parentNode = this;
      return child;
    },
    // Node.insertBefore: a null reference appends, as the DOM does.
    insertBefore(node, reference) {
      if (!reference) return this.appendChild(node);
      const prior = node.parentNode;
      if (prior) prior.removeChild(node);
      const at = this.childNodes.indexOf(reference);
      this.childNodes.splice(at, 0, node);
      if (node.nodeType === 1) {
        // `children` holds the elements only, so the node's place there counts the elements before it.
        const elementsBefore = this.childNodes.slice(0, at).filter((child) => child.nodeType === 1).length;
        this.children.splice(elementsBefore, 0, node);
        node.parentElement = this;
      }
      node.parentNode = this;
      return node;
    },
    get nextSibling() {
      if (!this.parentNode) return null;
      const siblings = this.parentNode.childNodes;
      return siblings[siblings.indexOf(this) + 1] || null;
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
    // Element.remove() semantics, routed through removeChild for the same bookkeeping.
    remove() {
      if (this.parentNode) this.parentNode.removeChild(this);
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
    // The comment layer measures layout at script load: placeColumn ->
    // measureContentRight reads document.body.querySelectorAll('*'). So the
    // stub must answer the '*' walk, not only class selectors. The heading walk
    // (previousHeading) reads tag lists like 'h1,h2,h3,h4,h5,h6', so tag tokens
    // match tagName case-insensitively.
    querySelectorAll(selector) {
      const all = [];
      const stack = this.children.slice();
      while (stack.length > 0) {
        const child = stack.shift();
        all.push(child);
        stack.push(...child.children);
      }
      if (selector === '*') return all;
      const tokens = selector.split(',').map((token) => token.trim());
      return all.filter((el) => tokens.some((token) => {
        if (token.startsWith('.')) {
          return String(el.className || '').split(/\s+/).includes(token.slice(1));
        }
        return String(el.tagName || '').toUpperCase() === token.toUpperCase();
      }));
    },
    // previousHeading and the send-order sort read document position. The stub
    // computes it from this element's tree, depth-first: the mask numbers are
    // Node's DOCUMENT_POSITION_FOLLOWING (4) and PRECEDING (2).
    compareDocumentPosition(other) {
      let root = this;
      while (root.parentElement) root = root.parentElement;
      const order = [];
      (function walk(node) {
        order.push(node);
        for (const child of node.children) walk(child);
      })(root);
      const mine = order.indexOf(this);
      const theirs = order.indexOf(other);
      if (theirs > mine) return 4;
      if (mine > theirs) return 2;
      return 0;
    },
  };
}

function findChildByClass(parent, className) {
  return parent.children.find((child) => child.className === className);
}

function dockOf(body) {
  return findChildByClass(body, '__cbc-dock');
}

// The rail's stack: the controls (shortcuts, tray, pick bar, composer, toast) sit here, not in the dock itself.
function stackOf(body) {
  return findChildByClass(dockOf(body), '__cbc-stack');
}

function clickElement(el) {
  assert.ok(el && el._listeners.click && el._listeners.click.length > 0, 'click listener exists');
  return el._listeners.click[0]({preventDefault() {}, stopPropagation() {}});
}

// Comments on a block through the real click flow: the document click opens the
// popover, the text box takes the comment, and the Add button stores it.
function addBlockComment(body, listeners, block, text) {
  const click = listeners.find((l) => l.target === 'document' && l.type === 'click').handler;
  click({target: block, preventDefault() {}, stopPropagation() {}});
  const popover = stackOf(body).children.find((c) => c.className === '__cbc-popover');
  popover.children[0].value = text;
  const actions = popover.children.find((c) => c.className === '__cbc-actions');
  clickElement(actions.children.find((c) => c.textContent === 'Add'));
}

function waitForPromises() {
  return new Promise((resolve) => setImmediate(resolve));
}

async function flushPromises(times) {
  for (let i = 0; i < times; i++) await waitForPromises();
}

module.exports = {makeElement, findChildByClass, dockOf, stackOf, clickElement, addBlockComment, waitForPromises, flushPromises};
