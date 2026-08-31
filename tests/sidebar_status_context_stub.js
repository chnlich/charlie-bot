// ---------------------------------------------------------------------------
// vm-context loader for web/static/js/sidebar/status.js, shared by the node
// --test harnesses that drive status.js standalone: one base context
// (SESSION_ID 'session-a', silent console, an ok-empty-json fetch), the
// harness's own globals spread over that base (document, fetch, backend
// fixtures), then one load sequence -- createContext, sidebar/namespace.js,
// sidebar/status.js -- so the load order every suite depends on lives in one
// place.
// ---------------------------------------------------------------------------
const vm = require('node:vm');

const { readStatic } = require('./read_static');

const NAMESPACE_JS = readStatic('sidebar/namespace.js');
const STATUS_JS = readStatic('sidebar/status.js');

function loadSidebarStatusContext(extraContext) {
  const context = {
    SESSION_ID: 'session-a',
    console: { error: () => {} },
    fetch: () => Promise.resolve({ ok: true, json: async () => ({}) }),
    ...extraContext,
  };
  vm.createContext(context);
  vm.runInContext(NAMESPACE_JS, context, { filename: 'sidebar/namespace.js' });
  vm.runInContext(STATUS_JS, context, { filename: 'sidebar/status.js' });
  return context;
}

module.exports = { loadSidebarStatusContext };
