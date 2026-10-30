// ---------------------------------------------------------------------------
// Shared harness for the chat turn-layout suites: the fake DOM the vm loader
// runs the chat bundle in (FakeElement with deterministic layout), the chat
// context loader, the DOM-path mount rig, and the turn-engine drive train
// (queueable timers, engine mount, invariant readers). The suites keep their
// own expectations; everything here only builds inputs and reads the DOM back.
// ---------------------------------------------------------------------------

const assert = require('node:assert/strict');
const vm = require('node:vm');

const {chatModules, runStaticModules} = require('./read_static');
const { createClassList } = require('./dom_element_stub');
const { escapeHtmlText } = require('./escape_html_stub');

const ELEMENT_NODE = 1;
const TEXT_NODE = 3;

// Height model behind FakeElement.getBoundingClientRect — see the getter.
const FAKE_LEAF_HEIGHT = 24;

// Row-gap emulation, opt-in per suite (enableRowGapEmulation): the chat
// container's `space-y-3` rule gives every non-first, non-hidden child a
// 12px margin-top, and the turn spacers are the one exception
// (styles.css: #messages > .turn-spacer { margin-top: 0 }). Off by default;
// with it off every margin reads 0 and the layout is exactly the old
// margin-free one.
const ROW_GAP_PX = 12;
let rowGapEmulation = false;

function enableRowGapEmulation() {
  rowGapEmulation = true;
}

// The emulated margin-top of one element: 12px on each non-first, non-hidden
// child of #messages while the emulation is on, 0 everywhere else.
function fakeRowMarginTop(el) {
  if (!rowGapEmulation) return 0;
  const parent = el.parentElement;
  if (!parent || parent.id !== 'messages') return 0;
  if (el.classList.contains('turn-spacer') || el.classList.contains('hidden')) return 0;
  for (const sib of parent.children) {
    if (sib === el) return 0;
    if (!sib.classList.contains('hidden')) return ROW_GAP_PX;
  }
  return 0;
}

// Selector support the turn engine's reading-anchor lookups need: '.class',
// '[data-attr]' and '[data-attr="value"]'. Anything else stays unsupported.
function fakeElementMatches(el, selector) {
  if (selector === 'button') return el.tagName === 'BUTTON';
  const classes = [...selector.matchAll(/\.([a-zA-Z0-9_-]+)/g)].map((match) => match[1]);
  const attrs = [...selector.matchAll(/\[([a-zA-Z-]+)(?:="([^"]*)")?\]/g)];
  const remainder = selector.replace(/\.[a-zA-Z0-9_-]+|\[[a-zA-Z-]+(?:="[^"]*")?\]/g, '');
  if (!remainder && (classes.length || attrs.length)) {
    if (!classes.every((className) => el.classList.contains(className))) return false;
    return attrs.every((attr) => {
      const key = attr[1].replace(/^data-/, '').replace(/-([a-z])/g, (_, c) => c.toUpperCase());
      const value = el.dataset ? el.dataset[key] : undefined;
      if (attr[2] == null) return value != null && value !== '';
      return String(value) === attr[2];
    });
  }
  throw new Error(`Unsupported selector: ${selector}`);
}

// Absolute top within the layout tree: the sum of every preceding sibling's
// height, walked up to the root. With row-gap emulation on, each #messages
// child's own margin-top adds to the walk — a sibling's margin-top collapses
// with the previous row's zero margin-bottom, so the gap above a row is its
// own margin.
function fakeElementTop(el) {
  let top = 0;
  let node = el;
  while (node.parentElement) {
    const parent = node.parentElement;
    const siblings = parent._nodes;
    const index = siblings.indexOf(node);
    for (let i = 0; i < index; i++) {
      if (siblings[i].nodeType === ELEMENT_NODE) {
        top += fakeElementHeight(siblings[i]) + fakeRowMarginTop(siblings[i]);
      }
    }
    top += fakeRowMarginTop(node);
    node = parent;
  }
  return top;
}

// Scroll positions of every ancestor: a real getBoundingClientRect is
// viewport-relative, so content inside a scrolled container reports its box
// minus the scroll offset. An element's own scrollTop never affects its own
// rect.
function fakeAncestorScrollOffset(el) {
  let offset = 0;
  let node = el.parentElement;
  while (node) {
    offset += node.scrollTop || 0;
    node = node.parentElement;
  }
  return offset;
}

function fakeElementHeight(el) {
  const styled = typeof el.style?.height === 'string' && el.style.height.endsWith('px');
  if (styled) return parseFloat(el.style.height);
  if (el.classList.contains('hidden')) return 0;
  let total = el.__baseHeight || 0;
  for (const child of el.children) total += fakeElementHeight(child) + fakeRowMarginTop(child);
  if (total === 0 && el.children.length === 0) return FAKE_LEAF_HEIGHT;
  return total;
}

class FakeText {
  constructor(text) {
    this.nodeType = TEXT_NODE;
    this.textContent = String(text);
    this.parentElement = null;
    this.parentNode = null;
  }
}

// Child bookkeeping runs over `_nodes`, which holds elements and text nodes
// alike: the fold row reads a bubble's text back off the rendered nodes, and
// that read only means anything if element boundaries are real here too.
class FakeElement {
  constructor(tagName = 'DIV', {id = '', className = ''} = {}) {
    this.nodeType = ELEMENT_NODE;
    this.tagName = String(tagName).toUpperCase();
    this.id = id;
    this.dataset = {};
    this.attributes = new Map();
    this.parentElement = null;
    this.parentNode = null;
    this._nodes = [];
    this.classList = createClassList(className);
    this._className = className;
    this.innerHTML = '';
    this.scrollTop = 0;
    this.clientHeight = 0;
    this.style = {};
    this._listeners = {};
  }

  // Like a real scroller, the scrollable range derives from laid-out content.
  get scrollHeight() {
    return this._scrollHeight != null ? this._scrollHeight : fakeElementHeight(this);
  }

  set scrollHeight(value) {
    this._scrollHeight = value;
  }

  get className() {
    return this.classList.toString();
  }

  set className(value) {
    this._className = String(value || '');
    this.classList = createClassList(this._className);
  }

  get childNodes() {
    return this._nodes;
  }

  get children() {
    return this._nodes.filter((node) => node.nodeType === ELEMENT_NODE);
  }

  get textContent() {
    return this._nodes.map((node) => node.textContent).join('');
  }

  set textContent(value) {
    for (const node of this._nodes) {
      node.parentElement = null;
      node.parentNode = null;
    }
    this._nodes = [];
    this.appendChild(new FakeText(value));
    // escapeHtml() round-trips text through textContent -> innerHTML.
    this.innerHTML = escapeHtmlText(value);
  }

  appendChild(child) {
    if (child.parentNode) child.parentNode.removeChild(child);
    child.parentElement = this;
    child.parentNode = this;
    this._nodes.push(child);
    return child;
  }

  removeChild(child) {
    const index = this._nodes.indexOf(child);
    if (index === -1) {
      throw new Error('child not found');
    }
    this._nodes.splice(index, 1);
    child.parentElement = null;
    child.parentNode = null;
    return child;
  }

  insertBefore(child, referenceChild) {
    if (child.parentNode) child.parentNode.removeChild(child);
    child.parentElement = this;
    child.parentNode = this;
    if (!referenceChild) {
      this._nodes.push(child);
      return child;
    }
    const index = this._nodes.indexOf(referenceChild);
    if (index === -1) {
      throw new Error('reference child not found');
    }
    this._nodes.splice(index, 0, child);
    return child;
  }

  prepend(child) {
    return this.insertBefore(child, this._nodes[0] || null);
  }

  remove() {
    if (this.parentNode) this.parentNode.removeChild(this);
  }

  get firstElementChild() {
    return this.children[0] || null;
  }

  get lastElementChild() {
    const elements = this.children;
    return elements[elements.length - 1] || null;
  }

  get firstChild() {
    return this._nodes[0] || null;
  }

  // Deterministic layout stand-in for the turn engine: inline pixel heights
  // win (spacers, placeholders), `.hidden` collapses, otherwise the height is
  // the element's own base plus the sum of its children, with a default leaf
  // height for text-bearing leaves. A hidden element (or one inside one) has
  // no box: like the browser over display:none, its rect reads (0, 0) — a
  // rect scan must not mistake that for a position at the viewport's top
  // edge.
  getBoundingClientRect() {
    for (let node = this; node; node = node.parentElement) {
      if ((node.classList && node.classList.contains('hidden'))
          || (node.hasAttribute && node.hasAttribute('hidden'))) {
        return {height: 0, top: 0};
      }
    }
    return {
      height: fakeElementHeight(this),
      top: fakeElementTop(this) - fakeAncestorScrollOffset(this),
    };
  }

  replaceWith(newNode) {
    const parent = this.parentNode;
    if (!parent) return;
    const index = parent._nodes.indexOf(this);
    if (index === -1) throw new Error('replaceWith: node not in parent');
    parent.removeChild(this);
    parent.insertBefore(newNode, parent._nodes[index] || null);
  }

  addEventListener(type, fn) {
    if (!this._listeners[type]) this._listeners[type] = [];
    this._listeners[type].push(fn);
  }

  removeEventListener(type, fn) {
    const list = this._listeners[type];
    if (!list) return;
    const index = list.indexOf(fn);
    if (index >= 0) list.splice(index, 1);
  }

  fire(type, event) {
    (this._listeners[type] || []).slice().forEach((fn) => fn(event || {}));
  }

  get nextSibling() {
    if (!this.parentNode) return null;
    const siblings = this.parentNode._nodes;
    const index = siblings.indexOf(this);
    return index === -1 ? null : (siblings[index + 1] || null);
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
    for (const child of this.children) {
      if (fakeElementMatches(child, selector)) return child;
      const nested = child.querySelector(selector);
      if (nested) return nested;
    }
    return null;
  }

  querySelectorAll(selector) {
    const matches = [];
    for (const child of this.children) {
      if (fakeElementMatches(child, selector)) matches.push(child);
      matches.push(...child.querySelectorAll(selector));
    }
    return matches;
  }

  closest(selector) {
    if (!selector.startsWith('.') && !selector.startsWith('[')) {
      throw new Error(`Unsupported selector: ${selector}`);
    }
    let current = this;
    while (current) {
      if (selector.startsWith('.')
          ? current.classList.contains(selector.slice(1))
          : fakeElementMatches(current, selector)) {
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
    // Like the DOM: an attribute set to the empty string reads back as '', and
    // only a missing one reads as null.
    return this.attributes.has(name) ? this.attributes.get(name) : null;
  }

  removeAttribute(name) {
    this.attributes.delete(name);
  }
}

function loadChatContext(document) {
  const nowIso = '2026-04-02T03:04:05.000Z';
  const context = {
    CSS: {escape: (value) => String(value)},
    SESSION_ID: 'session-a',
    document: {
      addEventListener() {},
      createElement(tag) {
        return new FakeElement(tag);
      },
      ...document,
    },
    console: {error: () => {}},
    relativeTime: (iso) => `relative:${iso}`,
    window: {addEventListener() {}},
    platform: {isMobile: false, onChange() {}},
    // Real parsing (the fold row formats a message timestamp), fixed "now"
    // (the sidebar bump stamps the current time).
    Date: class FakeDate extends Date {
      toISOString() {
        return nowIso;
      }
    },
  };

  vm.createContext(context);
  runStaticModules(context, chatModules());
  return {context, nowIso};
}

const PROSE_ROLES = ['assistant', 'worker_summary', 'plan'];
const BUBBLE_TIME_TEXT = 'Apr 2, 2026, 3:04:05 AM PDT';

// --- input spec -> DOM -----------------------------------------------------
function msg(role, id, extra = {}) {
  return Object.assign({kind: 'msg', role, id, text: `${role} ${id}`, ts: null}, extra);
}

function separator(id, extra = {}) {
  return Object.assign({kind: 'msg', role: 'separator', id, secs: 42}, extra);
}

function plain(id) {
  return {kind: 'plain', id};
}

// #streaming-msg / #load-more-sentinel: container fixtures inside no span.
function fixture(id) {
  return {kind: 'fixture', id};
}

function appendTimeDiv(el, ts) {
  if (!ts) return;
  const time = new FakeElement('DIV', {className: 'text-[10px] mt-1'});
  time.textContent = BUBBLE_TIME_TEXT;
  el.appendChild(time);
}

// Mirrors where each renderMessage() branch puts its text: prose bubbles keep
// the unrendered markdown on `.prose-msg[data-raw]`, plain bubbles hold it as
// text next to their time div.
function buildElement(item) {
  const el = new FakeElement('DIV');
  el.dataset.nodeId = item.id;
  if (item.kind !== 'msg') {
    el.id = item.kind === 'fixture' ? item.id : '';
    el.textContent = `node ${item.id}`;
    return el;
  }
  el.dataset.messageId = item.id;
  el.dataset.messageRole = item.role;
  if (item.ts) el.dataset.messageTs = item.ts;

  if (item.role === 'separator') {
    el.className = 'separator-line group/sep';
    const line = new FakeElement('DIV', {className: 'flex-1 border-t'});
    el.appendChild(line);
    return el;
  }

  // The trigger bubble is the one that holds its text and its time div under
  // the same `.whitespace-pre-wrap` node, as renderMessage() writes it.
  const bubbleClass = {
    user: 'max-w-[75%]',
    scheduled_trigger: 'w-full whitespace-pre-wrap break-words',
  }[item.role] || 'max-w-[90%]';
  const bubble = new FakeElement('DIV', {className: bubbleClass});
  if (PROSE_ROLES.includes(item.role)) {
    const prose = new FakeElement('DIV', {className: 'prose-msg'});
    prose.dataset.raw = item.text;
    bubble.appendChild(prose);
  } else if (item.role === 'user') {
    if (item.voice) {
      const badge = new FakeElement('SPAN', {className: 'text-xs text-blue-200 block mb-1'});
      badge.textContent = '\u{1F3A4} Voice';
      bubble.appendChild(badge);
    }
    const text = new FakeElement('DIV', {className: 'whitespace-pre-wrap'});
    text.textContent = item.text;
    bubble.appendChild(text);
  } else {
    bubble.appendChild(new FakeText(item.text));
  }
  appendTimeDiv(bubble, item.ts);
  el.appendChild(bubble);
  return el;
}

function mountCase(items) {
  const root = new FakeElement('DIV', {id: 'messages', className: 'space-y-3'});
  const nodes = new Map();
  for (const item of items) {
    const el = buildElement(item);
    nodes.set(item, el);
    root.appendChild(el);
  }
  const {context} = loadChatContext({
    getElementById(id) {
      if (id === 'messages') return root;
      return null;
    },
  });
  return {context, root, nodes};
}

function wrappers(root) {
  return root.children.filter((el) => el.classList.contains('turn-wrap'));
}

// DOM = top spacer + one contiguous turn window near the viewport + bottom
// spacer (+#streaming-msg fixture). Fetched messages live in the engine's
// store; HTML build/postprocess runs in scroll-gated idle slices; folded
// turn bodies and out-of-window turns are never in the DOM. These tests drive
// the engine through its public surface (mount, scroll events, pagination
// ingest, override/toggle globals) on the same fake DOM as the legacy suites,
// extended with deterministic layout (fakeElementHeight) and queueable timers
// so every scheduling step is explicit.
// ---------------------------------------------------------------------------

// --- engine fixtures --------------------------------------------------------
function eMsg(role, id, content, extra = {}) {
  return Object.assign(
      {role, id, content: content == null ? `${role} ${id}` : content, timestamp: '2026-04-02T10:07:00.000Z'},
      extra);
}

// A finished turn: user head, `steps` intermediate messages, assistant
// conclusion, separator with an event index, as the server sends it.
function eTurn(prefix, i, steps = 2) {
  const msgs = [eMsg('user', `${prefix}h${i}`, `question ${prefix} number ${i}`)];
  for (let s = 0; s < steps; s++) {
    msgs.push(eMsg(s % 2 ? 'system' : 'assistant', `${prefix}s${i}n${s}`, `step notice ${s}`));
  }
  msgs.push(eMsg('assistant', `${prefix}c${i}`, `the answer for ${i}`));
  msgs.push(eMsg('separator', `${prefix}p${i}`, '', {thinking_seconds: 30 + i, event_index: 1000 + i}));
  return msgs;
}

function ePage(prefix, turnCount, steps = 2) {
  const msgs = [];
  for (let i = 0; i < turnCount; i++) msgs.push(...eTurn(`${prefix}t${i}_`, 0, steps));
  return msgs;
}

function eTurnKey(prefix, i) {
  return `${prefix}h${i}|${prefix}c${i}|${prefix}p${i}`;
}

// The engine's message-node factory hook: mirrors where renderMessage puts
// identity, text and the separator's collapse anchor.
function fakeEngineNode(msg) {
  const el = new FakeElement('DIV');
  if (msg.id != null) el.dataset.messageId = String(msg.id);
  el.dataset.messageRole = msg.role;
  if (msg.timestamp) el.dataset.messageTs = msg.timestamp;
  if (msg.role === 'separator') {
    el.className = 'separator-line group/sep';
    el.__baseHeight = 30;
    el.appendChild(new FakeElement('DIV', {className: 'flex-1 border-t'}));
    return el;
  }
  const bubble = new FakeElement('DIV', {
    className: PROSE_ROLES.includes(msg.role) ? 'prose-msg' : 'whitespace-pre-wrap',
  });
  if (PROSE_ROLES.includes(msg.role)) bubble.dataset.raw = msg.content;
  else bubble.textContent = msg.content;
  if (msg.role === 'web_reply') {
    // renderMessage always gives a web reply the flex time row the For-you
    // round buttons hang on (rendering.js webReplyTimeRow).
    const timeRow = new FakeElement('DIV', {
      className: 'text-[10px] mt-1 flex items-center justify-between bubble-time-row',
    });
    if (msg.timestamp) timeRow.textContent = BUBBLE_TIME_TEXT;
    bubble.appendChild(timeRow);
  }
  const inner = new FakeElement('DIV', {className: 'max-w-[90%]'});
  inner.appendChild(bubble);
  el.appendChild(inner);
  el.__baseHeight = 46;
  return el;
}

function makeEngineTimers() {
  return {now: 0, idle: [], raf: [], timeout: []};
}

// The timing seams the turn engine drives; the recorders stay observable so a
// test can flush each queue deterministically.
function installEngineTimers(context, timers) {
  context.performance = {now: () => timers.now};
  context.requestIdleCallback = (fn) => (timers.idle.push(fn), timers.idle.length);
  context.requestAnimationFrame = (fn) => (timers.raf.push(fn), timers.raf.length);
  context.setTimeout = (fn) => (timers.timeout.push(fn), timers.timeout.length);
  context.clearTimeout = () => {};
}

// Like a real scroller, the position never leaves the laid-out range: writes
// clamp, and a read clamps again so content shrinking under the viewport
// (fold collapses, pagination sentinel removal) behaves like the browser's.
function installScrollTopClamp(root) {
  let scrollTop = root.scrollTop;
  Object.defineProperty(root, 'scrollTop', {
    configurable: true,
    get() {
      const max = Math.max(0, root.scrollHeight - root.clientHeight);
      return Math.max(0, Math.min(scrollTop, max));
    },
    set(value) {
      scrollTop = Math.max(0, Math.min(value, root.scrollHeight - root.clientHeight));
    },
  });
}

function mountEngine(messages, {clientHeight = 900, clampScrollTop = false, view = null} = {}) {
  const root = new FakeElement('DIV', {id: 'messages', className: 'space-y-3'});
  root.clientHeight = clientHeight;
  if (clampScrollTop) installScrollTopClamp(root);
  const stream = new FakeElement('DIV', {id: 'streaming-msg'});
  root.appendChild(stream);
  const timers = makeEngineTimers();
  const {context} = loadChatContext({
    createTreeWalker() {
      return {};
    },
    getElementById(id) {
      if (id === 'messages') return root;
      if (id === 'streaming-msg') return stream;
      return null;
    },
  });
  installEngineTimers(context, timers);
  context.fetch = async () => ({ok: false, status: 500, json: async () => ({})});
  context.Chat.buildTurnEngineMessageNode = fakeEngineNode;
  const engine = context.Chat.TurnEngine.mountIfAvailable(root, messages, 'sess-eng', view ? {view} : {});
  assert.ok(engine, 'engine mounts on a document with createTreeWalker');
  return {context, root, stream, timers, engine};
}

// The reader path to a fully opened history: every turn opened one click at a
// time, the way a reader opens turns from their rows.
function openAllTurns(engine) {
  engine.segments.forEach((seg) => {
    if (seg.kind === 'turn') engine.setOverride(seg.key, true);
  });
}

// --- deterministic scheduling ------------------------------------------------
function flushRaf(timers) {
  timers.raf.splice(0).forEach((fn) => fn());
}

// Let every queued slice run until the queues settle; the clock keeps jumping
// past the scroll-quiet window so gated slices become eligible.
function settle(timers) {
  for (let guard = 0; guard < 1000; guard++) {
    timers.now += 500;
    const idle = timers.idle.splice(0);
    const timeouts = timers.timeout.splice(0);
    const raf = timers.raf.splice(0);
    if (!idle.length && !timeouts.length && !raf.length) return;
    idle.forEach((fn) => fn());
    timeouts.forEach((fn) => fn());
    raf.forEach((fn) => fn());
  }
  throw new Error('engine scheduling did not settle');
}

function scrollTo(timers, root, position) {
  root.scrollTop = position;
  root.fire('scroll');
  timers.now += 16;
  flushRaf(timers);
}

function distanceFromBottom(root) {
  return root.scrollHeight - root.scrollTop - root.clientHeight;
}

// --- engine invariant readers --------------------------------------------------
function containerDescendants(root) {
  let count = 0;
  const walk = (node) => {
    for (const child of node.childNodes) {
      count++;
      if (child.nodeType === ELEMENT_NODE) walk(child);
    }
  };
  walk(root);
  return count;
}

function styleHeightPx(el) {
  const height = el.style.height;
  assert.ok(typeof height === 'string' && height.endsWith('px'), `spacer lacks a px height: ${height}`);
  return parseFloat(height);
}

function engineDebug(context, root) {
  const debug = context.Chat.TurnEngine.debug(root);
  assert.ok(debug, 'engine debug is available');
  return debug;
}

// 4.1 outline: DOM skeleton order + spacer arithmetic, exact within the
// engine's own height model.
function assertEngineInvariants(context, root, stream, label) {
  const kids = root.children;
  const topI = kids.findIndex((el) => el.classList.contains('turn-spacer-top'));
  const bottomI = kids.findIndex((el) => el.classList.contains('turn-spacer-bottom'));
  assert.ok(topI !== -1 && bottomI !== -1 && topI < bottomI, `${label}: spacer skeleton order`);
  assert.equal(kids[kids.length - 1], stream, `${label}: streaming fixture stays last`);

  const debug = engineDebug(context, root);
  const {start, end} = debug.window;
  const topExpected = start < debug.offsets.length ? debug.offsets[start] : 0;
  const windowEnd = end >= start ? debug.offsets[end] + debug.heights[end] : topExpected;
  const bottomExpected = debug.totalHeight - windowEnd;
  assert.equal(styleHeightPx(kids[topI]), Math.round(topExpected), `${label}: top spacer`);
  assert.equal(styleHeightPx(kids[bottomI]), Math.round(Math.max(0, bottomExpected)), `${label}: bottom spacer`);
  assert.ok(
      Math.abs((topExpected + (windowEnd - topExpected) + bottomExpected) - debug.totalHeight) < 1e-9,
      `${label}: spacer sum + window content = full height`);

  // The window in the DOM is exactly one contiguous run of segments.
  const windowNodes = kids.slice(topI + 1, bottomI);
  assert.equal(windowNodes.length, Math.max(0, end - start + 1), `${label}: window segment count`);
  return debug;
}

function assertFoldedWrapsBodyFree(root, label) {
  root.querySelectorAll('.turn-wrap').forEach((wrap) => {
    if (wrap.dataset.turnOpen !== 'false') return;
    const bodies = [];
    const walk = (node) => {
      for (const child of node.children) {
        if (child.dataset.messageId) bodies.push(child.dataset.messageId);
        walk(child);
      }
    };
    walk(wrap);
    assert.deepEqual(bodies, [], `${label}: folded wrap ${wrap.dataset.turnKey} holds body nodes`);
  });
}

function wrapsByKey(root) {
  const found = new Map();
  root.querySelectorAll('.turn-wrap').forEach((wrap) => found.set(wrap.dataset.turnKey, wrap));
  return found;
}

function messageIdsUnder(el) {
  const ids = [];
  const walk = (node) => {
    for (const child of node.children) {
      if (child.dataset.messageId) ids.push(child.dataset.messageId);
      walk(child);
    }
  };
  walk(el);
  return ids;
}

module.exports = {
  ELEMENT_NODE,
  TEXT_NODE,
  ROW_GAP_PX,
  enableRowGapEmulation,
  FakeText,
  FakeElement,
  loadChatContext,
  PROSE_ROLES,
  BUBBLE_TIME_TEXT,
  msg,
  separator,
  plain,
  fixture,
  buildElement,
  mountCase,
  wrappers,
  eMsg,
  eTurn,
  ePage,
  eTurnKey,
  fakeEngineNode,
  makeEngineTimers,
  installEngineTimers,
  installScrollTopClamp,
  mountEngine,
  openAllTurns,
  flushRaf,
  settle,
  scrollTo,
  distanceFromBottom,
  containerDescendants,
  styleHeightPx,
  engineDebug,
  assertEngineInvariants,
  assertFoldedWrapsBodyFree,
  wrapsByKey,
  messageIdsUnder,
};
