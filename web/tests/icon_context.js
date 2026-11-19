// Loads the icon table and icons.js into a vm context the way a page does: the table first, then the script that
// reads it. The table is built from web/static/icons/icons.json and the Lucide files, and each SVG's children are
// read as Icons::load reads them, so cbIcon in a test emits the markup the page would.
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const ICON_DIRECTORY = path.join(__dirname, '..', 'static', 'icons');
const ICONS_JS = fs.readFileSync(path.join(__dirname, '..', 'static', 'js', 'icons.js'), 'utf8');

function iconTableScript() {
  const table = JSON.parse(fs.readFileSync(path.join(ICON_DIRECTORY, 'icons.json'), 'utf8'));
  const entries = {};
  for (const [key, { lucide, fill }] of Object.entries(table)) {
    const source = fs.readFileSync(path.join(ICON_DIRECTORY, 'lucide', `${lucide}.svg`), 'utf8');
    const start = source.indexOf('>', source.indexOf('<svg')) + 1;
    const end = source.lastIndexOf('</svg>');
    entries[key] = { lucide, ...(fill === true ? { fill: true } : {}), shapes: source.slice(start, end) };
  }
  return `window.CB_ICON_TABLE = ${JSON.stringify(entries)};`;
}

// Runs in a context that already holds `window`. cbIcon is also exposed on the context itself, because the page's
// scripts call it as a global.
function loadIconScripts(context) {
  vm.runInContext(iconTableScript(), context, { filename: 'icon-table.js' });
  vm.runInContext(ICONS_JS, context, { filename: 'icons.js' });
  context.cbIcon = context.window.cbIcon;
}

module.exports = { loadIconScripts };
