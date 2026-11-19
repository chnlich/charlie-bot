// Loads the icon table and icons.js into a vm context the way a page does: the table first, then the script that
// reads it. The table is built from web/static/icons/icons.json and the Lucide files, and each SVG's children are
// read as Icons::load reads them, so cbIcon in a test emits the markup the page would.
const vm = require('node:vm');

const {iconTableScript, readStatic} = require('./read_static');

// Runs in a context that already holds `window`. cbIcon is also exposed on the context itself, because the page's
// scripts call it as a global.
function loadIconScripts(context) {
  vm.runInContext(iconTableScript(), context, {filename: 'icon-table.js'});
  vm.runInContext(readStatic('icons.js'), context, { filename: 'icons.js' });
  context.cbIcon = context.window.cbIcon;
}

module.exports = { loadIconScripts };
