// ---------------------------------------------------------------------------
// Loader for the committed web/static/js sources, shared by the node --test
// vm-harness tests. Callers pass the path relative to web/static/js
// (e.g. 'chat/namespace.js').
// ---------------------------------------------------------------------------
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const INDEX_HTML = path.join(__dirname, '..', 'web', 'templates', 'index.html');

function readStatic(...parts) {
  return fs.readFileSync(path.join(__dirname, '..', 'web', 'static', 'js', ...parts), 'utf8');
}

// Script paths index.html loads, in document order, with the /static/js/ prefix
// and the ?v= cache query stripped. The vm harnesses derive their module lists
// from here so a script added to or reordered on the page cannot drift from
// what the harnesses load.
function indexScriptPaths() {
  const html = fs.readFileSync(INDEX_HTML, 'utf8');
  return [...html.matchAll(/<script src="\/static\/js\/([^"?]+)/g)].map((m) => m[1]);
}

function scopedModules(prefix) {
  const files = indexScriptPaths().filter((p) => p.startsWith(prefix));
  if (files.length === 0) {
    throw new Error(`index.html loads no /static/js/${prefix} scripts — the harness module list is empty`);
  }
  return files;
}

// Run each file into the vm context in list order, labelling stack traces with
// the real file name.
function runStaticModules(context, files) {
  for (const file of files) {
    vm.runInContext(readStatic(file), context, {filename: file});
  }
}

module.exports = {readStatic, chatModules: () => scopedModules('chat/'), sidebarModules: () => scopedModules('sidebar/'), runStaticModules};
