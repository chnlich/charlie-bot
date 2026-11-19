// ---------------------------------------------------------------------------
// One home for the vm scaffold both artifact-comments suites load the real
// scripts into. Every option exists only where the two suites genuinely
// diverge. A member the scripts read but the harness lacks throws inside the
// vm and fails the loading test (same contract as artifact_comments_dom_stub).
// ---------------------------------------------------------------------------

const vm = require('node:vm');

const { readStatic, readPackageStatic } = require('./read_static');
const { loadIconScripts } = require('./icon_context');
const { makeElement } = require('./artifact_comments_dom_stub');

const ARTIFACT_COMMENTS_JS = readPackageStatic('features', 'artifacts', 'artifact-comments.js');
const COMMENT_POST_JS = readStatic('comment_post.js');

function loadArtifactCommentsContext(opts = {}) {
  const listeners = [];
  let requestSequence = 0;
  const window = {
    location: {pathname: opts.pathname, hash: opts.hash || ''},
    innerWidth: opts.innerWidth !== undefined ? opts.innerWidth : 1024,
    innerHeight: 768,
    addEventListener(type, handler, options) {
      listeners.push({target: 'window', type, handler, options});
    },
    removeEventListener(type, handler) {
      const index = listeners.findIndex((listener) =>
        listener.target === 'window' && listener.type === type && listener.handler === handler);
      if (index !== -1) listeners.splice(index, 1);
    },
    setTimeout() {},
    clearTimeout() {},
    getComputedStyle(el) {
      return {display: el.display || 'block'};
    },
  };
  if (opts.syncRaf) {
    // The tray's scroll path paints through rAF; a synchronous implementation
    // keeps those callbacks on the same tick as the triggering call.
    window.requestAnimationFrame = (fn) => { fn(); return 0; };
  }
  window.self = window;
  window.parent = opts.framed ? (opts.parent || {}) : window;
  // The server's inline tag, reproduced by the tests that own a session identity.
  if (opts.serverSessionId !== undefined) window.__cbcServerSessionId = opts.serverSessionId;

  const head = makeElement();
  const body = makeElement();
  for (const child of opts.bodyChildren || []) {
    body.appendChild(child);
  }
  // documentElement rides along so tests can assert the layer never writes it.
  const documentElement = makeElement();
  documentElement.tagName = 'HTML';
  documentElement.clientWidth = window.innerWidth;
  const document = {
    documentElement,
    head,
    body,
    createElement() {
      return makeElement();
    },
    addEventListener(type, handler, options) {
      listeners.push({target: 'document', type, handler, options});
    },
    removeEventListener(type, handler) {
      const index = listeners.findIndex((listener) =>
        listener.target === 'document' && listener.type === type && listener.handler === handler);
      if (index !== -1) listeners.splice(index, 1);
    },
    querySelectorAll(selector) {
      return body.querySelectorAll(selector);
    },
  };

  const context = {
    window,
    document,
    console: opts.console || console,
    // The comment-order sort reads both direction bits. The comments suite's
    // old context carried only FOLLOWING, so a PRECEDING mask bit read as
    // undefined and fell through. nodePreceding stays off there to preserve
    // that exact fall-through.
    Node: opts.nodePreceding
        ? {DOCUMENT_POSITION_FOLLOWING: 4, DOCUMENT_POSITION_PRECEDING: 2}
        : {DOCUMENT_POSITION_FOLLOWING: 4},
    fetch: opts.fetch || function () {
      throw new Error('fetch should not run while loading artifact-comments.js');
    },
    crypto: {randomUUID: () => `artifact-comment-request-${++requestSequence}`},
    // Timers run at once unless a suite supplies its own clock, as the pick undo tests do.
    setTimeout: opts.setTimeout || ((callback) => { callback(); return 1; }),
    clearTimeout: opts.clearTimeout || (() => {}),
  };
  if (opts.sessionStorage !== undefined) {
    context.sessionStorage = opts.sessionStorage;
  }
  vm.createContext(context);
  loadIconScripts(context);
  vm.runInContext(COMMENT_POST_JS, context, {filename: 'comment_post.js'});
  vm.runInContext(ARTIFACT_COMMENTS_JS, context, {filename: 'artifact-comments.js'});
  return {context, window, head, body, documentElement, listeners};
}

module.exports = {loadArtifactCommentsContext};
