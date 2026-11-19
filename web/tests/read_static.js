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

const UTILS_JS = readStatic('utils.js');

// Run each file into the vm context in list order, labelling stack traces with
// the real file name. The page loads utils.js ahead of every chat and sidebar
// module, and a module that reads its column-form or time helpers fails to load
// without them, so a harness that has not run utils.js yet runs it first. The
// harness's own globals keep their values: its time forms and feature loaders
// stand in for the ones utils.js defines.
function runStaticModules(context, files) {
  if (typeof context.expandSessionColumns !== 'function') {
    const stubs = Object.entries(context);
    vm.runInContext(UTILS_JS, context, {filename: 'utils.js'});
    for (const [name, value] of stubs) context[name] = value;
  }
  runIconModule(context);
  for (const file of files) {
    vm.runInContext(readStatic(file), context, {filename: file});
  }
}

// The meaning table the page carries: each meaning's Lucide file, its fill flag when set, and the children of the
// file's <svg> root. The children are read as the server reads them (svg_children in
// rust/crates/runtime/registry/src/icons.rs), so a test's markup matches the server's. The table is built from the
// committed icons.json and the Lucide files.
function iconTable() {
  const iconsDir = path.join(__dirname, '..', 'static', 'icons');
  const meanings = JSON.parse(fs.readFileSync(path.join(iconsDir, 'icons.json'), 'utf8'));
  const table = {};
  for (const [key, meaning] of Object.entries(meanings)) {
    const source = fs.readFileSync(path.join(iconsDir, 'lucide', `${meaning.lucide}.svg`), 'utf8');
    const start = source.indexOf('>', source.indexOf('<svg')) + 1;
    const end = source.lastIndexOf('</svg>');
    const shapes = source.slice(start, end);
    table[key] = meaning.fill === true
      ? {lucide: meaning.lucide, fill: true, shapes}
      : {lucide: meaning.lucide, shapes};
  }
  return table;
}

// The icon module the page inlines ahead of every script: the table icon_script() emits, then icons.js.
// The page's include list skips it, because its tag sits behind icon_script().
function iconTableScript() {
  return `window.CB_ICON_TABLE = ${JSON.stringify(iconTable())};`;
}

// Runs the icon module into a vm context once: cbIcon and the table it reads. A context without a window
// object gets its global as window, the page's own arrangement. A harness's window stub keeps its own copy,
// so the table and cbIcon are also set on the global, where the chat modules read them as bare names.
function runIconModule(context) {
  if (typeof context.cbIcon === 'function') return;
  vm.runInContext("if (typeof window === 'undefined') { globalThis.window = globalThis; }", context);
  vm.runInContext(iconTableScript(), context, {filename: 'icons.table.js'});
  vm.runInContext(readStatic('icons.js'), context, {filename: 'icons.js'});
  vm.runInContext('globalThis.CB_ICON_TABLE = window.CB_ICON_TABLE; globalThis.cbIcon = window.cbIcon;', context);
}

module.exports = {
  readStatic,
  readPackageStatic,
  indexScriptPaths,
  chatModules: () => scopedModules('chat/'),
  sidebarModules: () => scopedModules('sidebar/'),
  runStaticModules,
  runIconModule,
  iconTable,
  iconTableScript,
};
