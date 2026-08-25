// Shared scaffold for the chat.js / sidebar.js compatibility loaders; those files
// exist only for tests that still execute them directly, while runtime pages load
// the responsibility-scoped scripts under /static/js/chat/ and /static/js/sidebar/.
// Must run in the same context before either loader. The vm.runInContext sandboxes
// those tests build provide no `require`, so fs/path come from the host realm's
// `process.getBuiltinModule` via the constructor escape.
(function() {
  function loadCompatModules(subdir, names) {
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
    const base = path.join(baseDir, subdir);
    names.forEach((name) => {
      const code = fs.readFileSync(path.join(base, name), 'utf8');
      eval(code);
    });
  }
  globalThis.charliebotLoadCompatModules = loadCompatModules;
})();
