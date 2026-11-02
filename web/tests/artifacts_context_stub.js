// ---------------------------------------------------------------------------
// vm-context loader for the artifacts package's artifact-cards.js, shared by
// the node --test vm harnesses: one stub context (escapeHtml/hljs/localStorage/
// window/console/URL), the window.SESSIONS_ROOT the cards' relative-path
// resolution reads, and chat/namespace.js, core chat/file-links.js and the
// cards loaded into one vm. The cards register their message post-processor
// with chat/rendering.js, which this loader stubs.
// opts.document/opts.planPanel inject the cards' optional globals;
// opts.sessionId overrides the 'test-session' default for suites whose staged
// paths name another session id; opts.window merges extra members into the
// window stub (a recording addEventListener, an innerHeight); opts.withScroll
// also loads chat/scroll.js, for suites exercising the scroll seam
// (restoreBottomPin) that the cards' follow decisions route through.
// ---------------------------------------------------------------------------
const vm = require('node:vm');

const { escapeHtml } = require('./escape_html_stub');
const { readStatic, readPackageStatic } = require('./read_static');
const { SESSIONS_ROOT } = require('./sessions_root_stub');

function loadArtifactsScript(opts) {
  const o = opts || {};
  const context = {
    SESSION_ID: o.sessionId || 'test-session',
    escapeHtml,
    hljs: { highlight: (value) => ({ value: escapeHtml(value) }) },
    localStorage: { getItem: () => null, setItem: () => {} },
    window: Object.assign({ addEventListener: () => {}, SESSIONS_ROOT }, o.window),
    console,
    URL: globalThis.URL,
    CSS: { escape: (value) => String(value) },
    Node: { ELEMENT_NODE: 1, TEXT_NODE: 3 },
  };
  if (o.document) context.document = o.document;
  if (o.planPanel) context.planPanel = o.planPanel;
  context.Chat = { registerMessagePostProcessor: () => {} };
  vm.createContext(context);
  // namespace.js runs inside the vm so expose() assigns onto the vm's own
  // globalThis, not the outer Node global.
  vm.runInContext(readStatic('chat/namespace.js'), context, { filename: 'chat/namespace.js' });
  vm.runInContext(readStatic('chat/file-links.js'), context, { filename: 'chat/file-links.js' });
  vm.runInContext(readPackageStatic('features', 'artifacts', 'artifact-cards.js'), context, { filename: 'artifact-cards.js' });
  if (o.withScroll) vm.runInContext(readStatic('chat/scroll.js'), context, { filename: 'chat/scroll.js' });
  return context;
}

module.exports = { loadArtifactsScript };
