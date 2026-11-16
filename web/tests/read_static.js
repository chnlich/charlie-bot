// ---------------------------------------------------------------------------
// Loader for the committed web/static/js sources, shared by the node --test
// vm-harness tests. Callers pass the path relative to web/static/js
// (e.g. 'chat/namespace.js').
// ---------------------------------------------------------------------------
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const INDEX_HTML = path.join(__dirname, '..', 'templates', 'index.html');
// The page's scripts stand in the tail partial index.html names, so the harness reads both files in page order.
const CHAT_SCRIPTS = path.join(__dirname, '..', 'templates', 'partials', 'chat_scripts.html');

function readStatic(...parts) {
  return fs.readFileSync(path.join(__dirname, '..', 'static', 'js', ...parts), 'utf8');
}

// A package's source: web/<kind>/<name>/templates/<name>/<parts> for a script or style a page part includes in the
// document. Else web/<kind>/<name>/static/<parts> for a file the server serves at /static/<kind>/<name>/.
function readPackageStatic(kind, name, ...parts) {
  const packageRoot = path.join(__dirname, '..', kind, name);
  const included = path.join(packageRoot, 'templates', name, ...parts);
  return fs.readFileSync(fs.existsSync(included) ? included : path.join(packageRoot, 'static', ...parts), 'utf8');
}

// Script paths the chat page carries in document order, relative to web/static/js. The page includes each core
// script into the document, so the vm harnesses derive their module lists from the include tags and a script
// added to or reordered on the page cannot drift from what the harnesses load. The page's scripts are the tail
// partial it names after its markup, so the partial's tags follow index.html's.
function indexScriptPaths() {
  const html = fs.readFileSync(INDEX_HTML, 'utf8') + fs.readFileSync(CHAT_SCRIPTS, 'utf8');
  return [...html.matchAll(/<script>\{% include 'js\/([^']+)' %\}<\/script>/g)].map((m) => m[1]);
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

module.exports = {readStatic, readPackageStatic, chatModules: () => scopedModules('chat/'), sidebarModules: () => scopedModules('sidebar/'), runStaticModules};
