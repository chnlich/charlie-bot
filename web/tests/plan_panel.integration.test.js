const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');
const { escapeHtml } = require('./escape_html_stub');

const { readPackageStatic } = require('./read_static');

const { SESSIONS_ROOT } = require('./sessions_root_stub');

const PLAN_PANEL_JS = readPackageStatic('features', 'artifacts', 'plan-panel.js');

// ---------------------------------------------------------------------------
// Minimal DOM shim used both as the `DOMParser` injected into the plan-panel
// vm context (so `_renderActionBar` can parse fetched plan HTML) and directly
// in the template contract test. It implements the small subset of the DOM
// surface that `parseOpenForks` and the action-bar path rely on. That subset is
// querySelector/querySelectorAll with `tag.class` selectors, cloneNode(deep),
// child.remove(), and a live textContent computed from child nodes.
// ---------------------------------------------------------------------------

const VOID_TAGS = new Set([
  'AREA', 'BASE', 'BR', 'COL', 'EMBED', 'HR', 'IMG', 'INPUT',
  'LINK', 'META', 'PARAM', 'SOURCE', 'TRACK', 'WBR',
]);
const RAW_TEXT_TAGS = new Set(['SCRIPT', 'STYLE']);

class MinimalElement {
  constructor(tagName, attrs) {
    this.tagName = String(tagName).toUpperCase();
    this.attributes = attrs || {};
    this.children = [];
    this.parentNode = null;
  }
  get textContent() {
    let text = '';
    for (const child of this.children) {
      if (typeof child === 'string') text += child;
      else text += child.textContent;
    }
    return text;
  }
  cloneNode(deep) {
    const clone = new MinimalElement(this.tagName, {...this.attributes});
    if (deep) {
      for (const child of this.children) {
        if (typeof child === 'string') {
          clone.children.push(child);
        } else {
          const c = child.cloneNode(true);
          c.parentNode = clone;
          clone.children.push(c);
        }
      }
    }
    return clone;
  }
  remove() {
    if (!this.parentNode) return;
    const idx = this.parentNode.children.indexOf(this);
    if (idx >= 0) this.parentNode.children.splice(idx, 1);
    this.parentNode = null;
  }
  querySelector(selector) {
    const matches = this._all(selector);
    return matches.length ? matches[0] : null;
  }
  querySelectorAll(selector) {
    return this._all(selector);
  }
  _all(selector) {
    const out = [];
    for (const child of this.children) {
      if (typeof child === 'string') continue;
      if (_matchesSelector(child, selector)) out.push(child);
      for (const n of child._all(selector)) out.push(n);
    }
    return out;
  }
}

function _matchesSelector(el, selector) {
  const m = /^([a-zA-Z][\w-]*)?(?:\.([a-zA-Z_][\w-]*))?$/.exec(selector || '');
  if (!m) return false;
  const wantTag = m[1] ? m[1].toUpperCase() : null;
  const wantClass = m[2] || null;
  if (wantTag && el.tagName !== wantTag) return false;
  if (wantClass) {
    const cls = (el.attributes.class || '').split(/\s+/);
    if (!cls.includes(wantClass)) return false;
  }
  return true;
}

function parseHTML(html) {
  const root = new MinimalElement('#document', {});
  const stack = [root];
  let i = 0;
  while (i < html.length) {
    if (html[i] === '<') {
      if (html.substr(i, 4) === '<!--') {
        const end = html.indexOf('-->', i + 4);
        if (end === -1) break;
        i = end + 3;
        continue;
      }
      if (html[i + 1] === '!') {
        const close = html.indexOf('>', i);
        if (close === -1) break;
        i = close + 1;
        continue;
      }
      if (html[i + 1] === '/') {
        const close = html.indexOf('>', i);
        if (close === -1) break;
        const tag = html.slice(i + 2, close).trim().toUpperCase();
        for (let j = stack.length - 1; j > 0; j--) {
          if (stack[j].tagName === tag) { stack.length = j; break; }
        }
        i = close + 1;
        continue;
      }
      const close = html.indexOf('>', i);
      if (close === -1) break;
      let inner = html.slice(i + 1, close);
      const selfClosing = inner.endsWith('/');
      if (selfClosing) inner = inner.slice(0, -1).trim();
      const sp = inner.search(/\s/);
      let tagName, attrStr;
      if (sp === -1) { tagName = inner.toUpperCase(); attrStr = ''; }
      else { tagName = inner.slice(0, sp).toUpperCase(); attrStr = inner.slice(sp + 1); }
      const attrs = {};
      const are = /([a-zA-Z_:][-a-zA-Z0-9_:.]*)(?:\s*=\s*"([^"]*)")?/g;
      let am;
      while ((am = are.exec(attrStr)) !== null) {
        attrs[am[1].toLowerCase()] = am[2] != null ? am[2] : '';
      }
      const el = new MinimalElement(tagName, attrs);
      el.parentNode = stack[stack.length - 1];
      stack[stack.length - 1].children.push(el);
      i = close + 1;
      if (selfClosing || VOID_TAGS.has(tagName)) {
        // no children
      } else if (RAW_TEXT_TAGS.has(tagName)) {
        const closeTag = '</' + tagName.toLowerCase() + '>';
        const end = html.indexOf(closeTag, i);
        if (end === -1) {
          if (i < html.length) el.children.push(html.slice(i));
          break;
        }
        if (end > i) el.children.push(html.slice(i, end));
        i = end + closeTag.length;
      } else {
        stack.push(el);
      }
    } else {
      let next = html.indexOf('<', i);
      if (next === -1) next = html.length;
      const text = html.slice(i, next).replace(/\s+/g, ' ');
      if (text.trim()) stack[stack.length - 1].children.push(text);
      i = next;
    }
  }
  return root;
}

class DOMParser {
  parseFromString(html, mimeType) {
    return parseHTML(html);
  }
}

function loadPlanPanelScript(opts = {}) {
  const noop = () => {};
  const document = opts.document || {
    readyState: 'loading',
    addEventListener: noop,
    getElementById: () => null,
    createElement: () => ({
      _listeners: {},
      addEventListener(ev, fn) { this._listeners[ev] = fn; },
      appendChild() {},
      set type(v) { this._type = v; },
      get type() { return this._type; },
    }),
  };
  const context = {
    SESSION_ID: opts.sessionId || 'test-session',
    SESSIONS_ROOT: opts.sessionsRoot || SESSIONS_ROOT,
    console: {error: noop, log: noop, warn: noop},
    document,
    window: {addEventListener() {}, SESSIONS_ROOT: opts.sessionsRoot || SESSIONS_ROOT},
    navigator: {onLine: true},
    localStorage: {getItem: () => null, setItem: noop},
    fetch: opts.fetch || (async () => {
      throw new Error('fetch should not run during script load');
    }),
    DOMParser,
    escapeHtmlAttr: (value) => escapeHtml(value == null ? '' : String(value)),
    // The core entries the panel registers with at load; web/tests/page_registries.test.js covers them.
    registerTab: noop,
    registerWsConnectHandler: noop,
    registerWsMessageHandler: noop,
  };
  vm.createContext(context);
  vm.runInContext(PLAN_PANEL_JS, context, {filename: 'plan-panel.js'});
  const planPanel = vm.runInContext('planPanel', context);
  return {context, planPanel};
}

// ---------------------------------------------------------------------------
// Item 6: template contract — parseOpenForks against prompts/plan_template.html
// ---------------------------------------------------------------------------

test('plan_template.html contract: open forks extracted, no question starts with its number, resolved excluded', () => {
  const {planPanel} = loadPlanPanelScript();
  const templatePath = path.join(__dirname, '..', '..', 'prompts', 'plan_template.html');
  const html = fs.readFileSync(templatePath, 'utf8');
  const doc = new DOMParser().parseFromString(html, 'text/html');
  const forks = planPanel.parseOpenForks(doc);
  assert.ok(forks.length >= 1, 'template has at least one open fork');
  for (const f of forks) {
    assert.ok(f.n && f.question, 'each open fork has a number and a question');
    assert.equal(String(f.question).startsWith(String(f.n)), false,
      'question text must not start with its fork number');
  }
  const ns = forks.map((f) => String(f.n));
  assert.ok(ns.includes('1'), 'open fork #1 (scope question) is present');
  assert.equal(ns.includes('2'), false, 'resolved fork #2 is excluded');
});
