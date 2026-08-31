// ---------------------------------------------------------------------------
// vm-context loader for web/static/js/chat/artifacts.js, shared by the node
// --test vm harnesses: one stub context (escapeHtml/hljs/localStorage/window/
// console/URL), the window.SESSIONS_ROOT artifacts.js's relative-path
// resolution reads, and the namespace+artifacts sources loaded into one vm.
// opts.document/opts.planPanel inject artifacts.js's optional globals;
// opts.sessionId overrides the 'test-session' default for suites whose staged
// paths name another session id.
// ---------------------------------------------------------------------------
const vm = require('node:vm');

const { escapeHtml } = require('./escape_html_stub');
const { readStatic } = require('./read_static');
const { SESSIONS_ROOT } = require('./sessions_root_stub');

function loadArtifactsScript(opts) {
  const o = opts || {};
  const context = {
    SESSION_ID: o.sessionId || 'test-session',
    escapeHtml,
    hljs: { highlight: (value) => ({ value: escapeHtml(value) }) },
    localStorage: { getItem: () => null, setItem: () => {} },
    window: { addEventListener: () => {}, SESSIONS_ROOT },
    console,
    URL: globalThis.URL,
  };
  if (o.document) context.document = o.document;
  if (o.planPanel) context.planPanel = o.planPanel;
  vm.createContext(context);
  // namespace.js runs inside the vm so expose() assigns onto the vm's own
  // globalThis, not the outer Node global.
  vm.runInContext(readStatic('chat/namespace.js'), context, { filename: 'chat/namespace.js' });
  vm.runInContext(readStatic('chat/artifacts.js'), context, { filename: 'artifacts.js' });
  return context;
}

module.exports = { loadArtifactsScript };
