// ---------------------------------------------------------------------------
// DOM-element stub for the node --test vm harnesses (no jsdom): the superset
// of every DOM member the loaded chat/sidebar bundle touches, so a member the
// script reads but the stub lacks throws inside the vm and fails the loading
// test. addEventListener records handlers on `_listeners`; tests that
// dispatch nothing pay nothing for the record.
// ---------------------------------------------------------------------------

function createClassList(initial = '') {
  const names = new Set(String(initial).split(/\s+/).filter(Boolean));
  return {
    add: (...items) => items.forEach((item) => { if (item) names.add(item); }),
    remove: (...items) => items.forEach((item) => names.delete(item)),
    contains: (item) => names.has(item),
    toggle: (item, force) => {
      if (force === undefined) {
        if (names.has(item)) { names.delete(item); return false; }
        names.add(item); return true;
      }
      if (force) names.add(item); else names.delete(item);
      return !!force;
    },
    toString: () => Array.from(names).join(' '),
  };
}

function createElement(overrides = {}) {
  let innerHTML = overrides.innerHTML || '';
  const element = {
    tagName: overrides.tagName || 'DIV',
    value: overrides.value || '',
    textContent: overrides.textContent || '',
    checked: overrides.checked || false,
    disabled: overrides.disabled || false,
    readOnly: overrides.readOnly || false,
    dataset: overrides.dataset || {},
    style: overrides.style || {},
    id: overrides.id || '',
    children: [],
    options: [],
    parentNode: null,
    classList: createClassList(overrides.className || ''),
    appendChild(child) {
      if (child && child.parentNode) child.parentNode.removeChild(child);
      if (child) child.parentNode = this;
      this.children.push(child);
      if (child && child.tagName === 'OPTION') {
        this.options.push(child);
        if (child.selected || !this.value) this.value = child.value;
      }
      return child;
    },
    prepend(child) {
      if (child && child.parentNode) child.parentNode.removeChild(child);
      if (child) child.parentNode = this;
      this.children.unshift(child);
      return child;
    },
    insertBefore(child, referenceChild) {
      if (child && child.parentNode) child.parentNode.removeChild(child);
      if (child) child.parentNode = this;
      if (!referenceChild) { this.children.push(child); return child; }
      const index = this.children.indexOf(referenceChild);
      if (index === -1) throw new Error('reference child not found');
      this.children.splice(index, 0, child);
      return child;
    },
    removeChild(child) {
      const index = this.children.indexOf(child);
      if (index === -1) throw new Error('child not found');
      this.children.splice(index, 1);
      if (child) child.parentNode = null;
      return child;
    },
    after(child) {
      if (!this.parentNode) return;
      if (child && child.parentNode) child.parentNode.removeChild(child);
      if (child) child.parentNode = this.parentNode;
      const index = this.parentNode.children.indexOf(this);
      this.parentNode.children.splice(index + 1, 0, child);
    },
    remove() {
      if (this.parentNode) this.parentNode.removeChild(this);
      this.removed = true;
    },
    focus() {},
    addEventListener(type, handler) {
      if (!this._listeners) this._listeners = {};
      if (!this._listeners[type]) this._listeners[type] = [];
      this._listeners[type].push(handler);
    },
    setAttribute(name, value) { this[name] = value; },
    getAttribute(name) { return this[name]; },
    querySelectorAll: () => [],
    querySelector: () => null,
  };

  Object.defineProperty(element, 'firstElementChild', {
    get() { return element.children[0] || null; },
  });
  Object.defineProperty(element, 'lastChild', {
    get() { return element.children[element.children.length - 1] || null; },
  });
  Object.defineProperty(element, 'innerHTML', {
    get() { return innerHTML; },
    set(value) { innerHTML = value; element.children = []; element.options = []; },
  });

  return Object.assign(element, overrides);
}

module.exports = { createElement };
