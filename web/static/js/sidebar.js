// Compatibility loader for tests or tools that still execute sidebar.js directly.
// Runtime pages load the responsibility-scoped scripts under /static/js/sidebar/.
(function() {
  let nodeRequire = null;
  let nodeProcess = null;
  if (typeof require === 'function') {
    nodeRequire = require;
  } else {
    try {
      nodeRequire = globalThis.constructor.constructor('return require')();
    } catch (_err) {
      try {
        nodeProcess = globalThis.constructor.constructor('return process')();
        nodeRequire = (id) => nodeProcess.getBuiltinModule(id.replace(/^node:/, ''));
      } catch (_processErr) {
        return;
      }
    }
  }
  const fs = nodeRequire('node:fs');
  const path = nodeRequire('node:path');
  const process = nodeProcess || nodeRequire('node:process');
  const baseDir = typeof __dirname === 'undefined'
    ? path.join(process.cwd(), 'web', 'static', 'js')
    : __dirname;
  const base = path.join(baseDir, 'sidebar');
  [
    'namespace.js',
    'status.js',
    'workers.js',
    'filters.js',
    'groups.js',
    'session-view.js',
    'modals.js',
  ].forEach((name) => {
    const code = fs.readFileSync(path.join(base, name), 'utf8');
    eval(code);
  });
})();
