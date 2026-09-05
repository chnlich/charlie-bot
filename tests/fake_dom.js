// ---------------------------------------------------------------------------
// Minimal fake DOM shared by the node --test render tests. `innerHTML` on any
// element is the concatenation of its own literal string content plus its
// children's `innerHTML` -- this mirrors real DOM semantics closely enough for
// both `renderMessagesIntoContainer` (which replaces content wholesale via
// `.innerHTML =`) and `_appendRenderedMessage` (which appends discrete child
// wrappers), without needing an HTML parser. The classList and text-escape
// primitives are the ones shared with dom_element_stub/escape_html_stub.
// ---------------------------------------------------------------------------
const { createClassList } = require('./dom_element_stub');
const { escapeHtmlText } = require('./escape_html_stub');

class FakeElement {
  constructor(tag = 'DIV') {
    this.tagName = String(tag).toUpperCase();
    this.children = [];
    this.parentElement = null;
    this.dataset = {};
    this.classList = createClassList();
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
    this.innerHTML = escapeHtmlText(this._text);
  }

  get firstElementChild() {
    return this.children[0] || null;
  }

  appendChild(child) {
    child.parentElement = this;
    this.children.push(child);
    return child;
  }

  insertAdjacentHTML(position, html) {
    if (position !== 'beforeend') throw new Error(`FakeElement only fakes beforeend, got ${position}`);
    // The real call leaves existing nodes untouched; the getter concatenates _html back.
    this._html += html;
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

module.exports = { FakeElement };
