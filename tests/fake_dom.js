// ---------------------------------------------------------------------------
// Minimal fake DOM shared by the node --test render tests. `innerHTML` on any
// element is the concatenation of its own literal string content plus its
// children's `innerHTML` -- this mirrors real DOM semantics closely enough for
// both `renderMessagesIntoContainer` (which replaces content wholesale via
// `.innerHTML =`) and `_appendRenderedMessage` (which appends discrete child
// wrappers), without needing an HTML parser.
// ---------------------------------------------------------------------------
function escapeForFakeDom(str) {
  return String(str).replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;');
}

class FakeElement {
  constructor(tag = 'DIV') {
    this.tagName = String(tag).toUpperCase();
    this.children = [];
    this.parentElement = null;
    this.dataset = {};
    const classSet = new Set();
    this.classList = {
      add(...c) { c.forEach((x) => classSet.add(x)); },
      remove(...c) { c.forEach((x) => classSet.delete(x)); },
      toggle(c, force) {
        if (force === undefined) {
          if (classSet.has(c)) { classSet.delete(c); return false; }
          classSet.add(c);
          return true;
        }
        if (force) classSet.add(c); else classSet.delete(c);
        return !!force;
      },
      contains(c) { return classSet.has(c); },
    };
    this._html = '';
    this._text = '';
    this.scrollTop = 0;
    this.scrollHeight = 0;
    this.clientHeight = 0;
  }

  get innerHTML() {
    return this._html + this.children.map((c) => c.innerHTML).join('');
  }

  set innerHTML(html) {
    this._html = html;
    this.children = [];
  }

  get textContent() {
    return this._text;
  }

  set textContent(value) {
    this._text = String(value || '');
    this.innerHTML = escapeForFakeDom(this._text);
  }

  get firstElementChild() {
    return this.children[0] || null;
  }

  appendChild(child) {
    child.parentElement = this;
    this.children.push(child);
    return child;
  }

  insertBefore(child, ref) {
    child.parentElement = this;
    if (!ref) {
      this.children.push(child);
      return child;
    }
    const idx = this.children.indexOf(ref);
    this.children.splice(idx === -1 ? this.children.length : idx, 0, child);
    return child;
  }

  removeChild(child) {
    const idx = this.children.indexOf(child);
    if (idx !== -1) this.children.splice(idx, 1);
    return child;
  }

  remove() {
    if (this.parentElement) this.parentElement.removeChild(this);
  }

  querySelector() {
    return null;
  }

  querySelectorAll() {
    return [];
  }
}

module.exports = { escapeForFakeDom, FakeElement };
