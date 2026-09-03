// ---------------------------------------------------------------------------
// vm-context loader for web/static/js/sidebar/workers.js, shared by the node
// --test harnesses that drive workers.js standalone: one base context
// (SESSION_ID 'session-a', empty BACKEND_OPTIONS, silent console, an ok-empty
// fetch, the escapeHtml/escapeHtmlAttr sandbox wiring, and no-op
// loadedThreads / stopThreadPoll / fetchAndRenderEvents stand-ins), the
// harness's own globals spread over that base (document, fetch, modal
// fixtures), then one load sequence -- createContext, sidebar/namespace.js,
// sidebar/workers.js -- so the load order every suite depends on lives in one
// place.
// ---------------------------------------------------------------------------
const vm = require('node:vm');

const { readStatic } = require('./read_static');
const { escapeHtml } = require('./escape_html_stub');

const NAMESPACE_JS = readStatic('sidebar/namespace.js');
const WORKERS_JS = readStatic('sidebar/workers.js');

function loadSidebarWorkersContext(extraContext) {
  const context = {
    SESSION_ID: 'session-a',
    BACKEND_OPTIONS: {},
    console: { warn: () => {}, error: () => {} },
    fetch: () => Promise.resolve({ ok: true, json: async () => ({}) }),
    escapeHtml,
    escapeHtmlAttr: (value) => escapeHtml(value == null ? '' : String(value)),
    loadedThreads: { delete() {} },
    stopThreadPoll: () => {},
    fetchAndRenderEvents: () => Promise.resolve(),
    ...extraContext,
  };
  vm.createContext(context);
  vm.runInContext(NAMESPACE_JS, context, { filename: 'sidebar/namespace.js' });
  vm.runInContext(WORKERS_JS, context, { filename: 'sidebar/workers.js' });
  return context;
}

module.exports = { loadSidebarWorkersContext };
