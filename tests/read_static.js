// ---------------------------------------------------------------------------
// Loader for the committed web/static/js sources, shared by the node --test
// vm-harness tests. Callers pass the path relative to web/static/js
// (e.g. 'chat/namespace.js'); the base directory has exactly one definition
// here.
// ---------------------------------------------------------------------------
const fs = require('node:fs');
const path = require('node:path');

function readStatic(...parts) {
  return fs.readFileSync(path.join(__dirname, '..', 'web', 'static', 'js', ...parts), 'utf8');
}

module.exports = { readStatic };
