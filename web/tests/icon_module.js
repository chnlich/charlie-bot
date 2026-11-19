// ---------------------------------------------------------------------------
// vm-context loader for the icon module. A page gets the meaning table from
// icon_script() in chat_scripts.html, then runs web/static/js/icons.js. A
// harness that runs a module calling cbIcon() loads both here, from the same
// web/static/icons files the server reads, so the markup a test sees is the
// markup the page draws.
// ---------------------------------------------------------------------------
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const ICON_DIRECTORY = path.join(__dirname, '..', 'static', 'icons');
const ICONS_JS = fs.readFileSync(path.join(__dirname, '..', 'static', 'js', 'icons.js'), 'utf8');

// The meaning table as the page's icon_script() carries it: each meaning's fill flag and the inner shapes of its
// Lucide file, the file's license comment and root <svg> tags left out.
function iconTable() {
  const meanings = JSON.parse(fs.readFileSync(path.join(ICON_DIRECTORY, 'icons.json'), 'utf8'));
  const table = {};
  for (const [key, entry] of Object.entries(meanings)) {
    const source = fs.readFileSync(path.join(ICON_DIRECTORY, 'lucide', `${entry.lucide}.svg`), 'utf8');
    const match = source.match(/<svg[^>]*>([\s\S]*)<\/svg>/);
    table[key] = { lucide: entry.lucide, shapes: match[1] };
    if (entry.fill === true) table[key].fill = true;
  }
  return table;
}

// Installs cbIcon as a global of the context, as the page's inline script does. A context that carries its own
// window object gets the table and the function on it, and the bare cbIcon name aliases the function.
function loadIconModule(context) {
  if (context.window === undefined) context.window = context;
  context.window.CB_ICON_TABLE = iconTable();
  vm.runInContext(ICONS_JS, context, { filename: 'js/icons.js' });
  context.cbIcon = context.window.cbIcon;
  return context;
}

module.exports = { iconTable, loadIconModule };
