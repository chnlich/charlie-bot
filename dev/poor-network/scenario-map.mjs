// Which rows a change measures (`run.sh --scenarios auto`). The default four are always measured, so every change keeps
// the baseline rows; a path that a row can move adds that row, and an open condition for an open cell. The map is the
// one definition of the selection; run.sh asks it through this file's command line.
//
//   node scenario-map.mjs <changed file>...   prints { "scenarios": [...], "openConditions": [...] } as JSON

import { fileURLToPath } from 'node:url';

export const DEFAULT_SCENARIOS = Object.freeze(['background', 'held', 'reset', 'stream']);

// Each rule names files or directories by prefix or exact path, and the rows and open conditions those paths add.
const RULES = Object.freeze([
  {
    // Sidebar search: the search row (search.mjs), at the sidebar's own files.
    files: [
      'web/static/js/sidebar/filters.js',
      'web/static/js/websocket-handlers.js',
      'web/templates/index.html',
      'rust/crates/runtime/session-api/src/search.rs',
      'rust/crates/runtime/session-api/src/routes.rs',
      'rust/crates/app/server/src/etag.rs',
      'rust/crates/app/mock-home/src/generate.rs',
      'dev/poor-network/search.mjs',
    ],
    scenarios: ['search'],
  },
  {
    // A page load: the open cell over a normal link.
    files: [
      'web/templates/index.html',
      'web/templates/partials/chat_scripts.html',
      'web/static/js/app.js',
      'rust/crates/app/server/src/routes/pages.rs',
    ],
    prefixes: ['web/static/css/'],
    openConditions: ['link'],
  },
  {
    // The offline shell: the cells that open from the offline copy.
    files: [
      'web/static/js/offline-shell.js',
      'web/static/js/offline-shell-worker.js',
      'web/templates/offline-shell-worker.js',
      'web/static/js/opened-sessions.js',
    ],
    openConditions: ['offline', 'release'],
  },
  {
    // The socket and the reconnect: the cells that drop the link.
    files: [
      'web/static/js/websocket.js',
      'web/static/js/websocket-handlers.js',
      'web/static/js/view-requests.js',
      'rust/crates/runtime/session-api/src/websocket.rs',
    ],
    openConditions: ['held', 'reset', 'flaps'],
  },
  {
    // The task tree: the background load, and the sidebar's starred and threads views that show it.
    prefixes: ['rust/crates/runtime/tasks/', 'rust/crates/runtime/agents/'],
    scenarios: ['starred', 'threads'],
  },
  {
    // The sidebar's own views.
    prefixes: ['web/static/js/sidebar/'],
    scenarios: ['starred', 'threads'],
  },
]);

function matches(rule, file) {
  return (rule.files ?? []).includes(file) || (rule.prefixes ?? []).some((prefix) => file.startsWith(prefix));
}

// The rows a set of changed files selects, with the default four always included.
export function autoScenarios(changedFiles) {
  const scenarios = new Set(DEFAULT_SCENARIOS);
  const openConditions = new Set();
  for (const file of changedFiles) {
    for (const rule of RULES) {
      if (!matches(rule, file)) {
        continue;
      }
      for (const name of rule.scenarios ?? []) {
        scenarios.add(name);
      }
      for (const name of rule.openConditions ?? []) {
        openConditions.add(name);
      }
    }
  }
  // An open condition is a cell of the open scenario, so a selected condition selects the scenario.
  if (openConditions.size > 0) {
    scenarios.add('open');
  }
  return {
    scenarios: [...scenarios],
    openConditions: [...openConditions],
  };
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  process.stdout.write(`${JSON.stringify(autoScenarios(process.argv.slice(2)))}\n`);
}
