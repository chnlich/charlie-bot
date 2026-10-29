// Frontend structure check: the groups of the checked pages and the names each group may use.
//
// Every frontend file the checked pages load belongs to one group:
//
//     core            every file under web/
//     features.<x>    src/features/<x>/static/** and src/features/<x>/templates/<x>/page_*.html
//     backends.<x>    src/backends/<x>/static/** and src/backends/<x>/templates/<x>/page_*.html
//
// The chat page is web/templates/index.html plus every package's page part templates. The page
// loads its files in layers, and the load order is the layer order: the inline <script> bodies of
// a template are its lowest node (bootstrap), each script its <script src="/static/..."> tags load
// is one node, and its inline handler attributes (onclick="..." and the like) are its highest node
// (markup). Package parts load after core, in the server's package registry order
// (rust/crates/app/packages/src/lib.rs, the PACKAGES table).
//
// Directions: core uses no name a package defines. A package uses names of core and of itself
// only; the features in CHAT_CHANNELS may also use features.chat_threads, and the backends in
// BACKEND_VARIANTS may also use backends.claude_code. A core node uses only names that core nodes
// loaded before it define, so core has no cycle between files. A cycle inside one package is
// allowed. One global name is defined in one file only. web/static has no features/ and no
// backends/ directory: those URL prefixes belong to package static files.
//
// web/templates/index.html loads core in five layers, lowest first:
//
//     base        platform.js to chat/shared.js: the helpers, config.js and the Chat and Sidebar namespaces
//     connection  websocket.js and tabs.js
//     chat view   chat/ratings.js to file-upload.js
//     sidebar     sidebar/row-menu.js to sidebar/session-menus.js
//     control     chat/input.js, websocket-handlers.js and app.js
//
// A package learns core's facts through a registration entry: a function core provides, and the
// package's script registers a handler with it at load time. The core registration entries:
//
//     Chat.registerDeliveryLink   web/static/js/chat/rendering.js (a link on the worker delivery banner)
//     Chat.registerFileLinkFamily web/static/js/chat/file-links.js (a family of file links the package owns)
//     Chat.registerForYouRules    web/static/js/chat/turn-engine.js (the For-you projection's rules, from chat/for-you.js)
//     Chat.registerInputMode      web/static/js/chat/attachments.js (an input mode and its user-bubble badge)
//     Chat.registerMessagePostProcessor
//                                 web/static/js/chat/rendering.js (a pass over each rendered message root)
//     Chat.registerProjectionCheck
//                                 web/static/js/chat/turn-engine.js (the page's check, run after each projection that ran)
//     Chat.registerSeparatorButton
//                                 web/static/js/chat/rendering.js (a button on each separator line)
//     Chat.registerSeparatorPanel web/static/js/chat/turn-engine.js (a panel under a separator line that the engine reopens)
//     Chat.registerStatusViewControl
//                                 web/static/js/chat/for-you.js (the Status view's show and hide, from chat/status-panel.js)
//     registerGroupMenuItem       web/static/js/sidebar/groups.js (an item of a group header's menu)
//     registerSessionMenuItems    web/static/js/sidebar/groups.js (items of a session row's Settings menu)
//     registerSessionRowBadge     web/static/js/sidebar/groups.js (a badge after a session row's indicators)
//     registerSessionRowLine      web/static/js/sidebar/groups.js (a line in place of a session row's time line)
//     registerSidebarControl      web/static/js/sidebar/namespace.js (the handler a rendered sidebar control calls by name)
//     registerSidebarFilter       web/static/js/sidebar/filters.js (a sidebar filter button)
//     registerTab                 web/static/js/tabs.js (a tab: its layout, show and hide callbacks)
//     registerWorkspaceBadge      web/static/js/sidebar/filters.js (a badge at the top of the Workspace list)
//     registerWsConnectHandler    web/static/js/websocket.js (a handler of every socket (re)connect)
//     registerWsMessageHandler    web/static/js/websocket.js (the handler of one WebSocket event type)
//
// A package that adds an item to a core menu needs no registration entry: it appends its own
// element to the menu's container (the voice package adds the composer ⋯ menu's Voice backend
// entry to #overflow-menu), so the core markup names nothing the package defines.
//
// Core announces a moment that needs no answer as a CustomEvent on document, and a package listens
// for it. The core page events:
//
//     charliebot:active-session-changed
//                                 web/static/js/sidebar/session-view.js (SESSION_ID names another session)
//     charliebot:session-data-load
//                                 web/static/js/sidebar/session-view.js (the idle load of the session's per-session data)
//     charliebot:session-switch-phase
//                                 web/static/js/sidebar/session-view.js (one phase of a session switch, with its timing)
//     charliebot:session-switched web/static/js/sidebar/session-view.js (the end of a session switch)
//     charliebot:session-view-teardown
//                                 web/static/js/sidebar/session-view.js (the active session view is torn down)
//
// A landing that adds a registration entry or a page event extends these lists.
//
// An upward use — a file using a name that only a higher file defines — has three fixes. Move the
// shared constant or function to the lower file. The higher file registers a handler through a
// registration entry of the lower file. The higher file attaches a delegated DOM listener in place
// of a generated onclick string that names a higher function.
'use strict';
const assert = require('node:assert/strict');
const { execFileSync } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');

const ROOT = path.join(__dirname, '..');
const GRAPH_SCRIPT = path.join(__dirname, 'frontend_structure_graph.js');
const PAGE_NAMES = ['chat'];

// The sets the workspace structure test reads too (rust/crates/app/structure/tests/structure.rs):
// the features that may use features.chat_threads and the backends that may use backends.claude_code.
const CHAT_CHANNELS = new Set(['slack', 'discord']);
const BACKEND_VARIANTS = new Set(['anthropic_compatible', 'openai_compatible']);

const FIX_THE_UPWARD_USE =
  'Fix it in one of three ways. Move the shared constant or function to the lower file. The higher file ' +
  'registers a handler through a registration entry of the lower file. The higher file attaches a delegated ' +
  'DOM listener in place of a generated onclick string that names a higher function.';

// A package part: a static file of the package, or one of its templates/<name>/page_*.html part
// templates. A package that adds files at these paths joins the check without an edit here.
const PACKAGE_PART =
  new RegExp('^src/(features|backends)/([A-Za-z0-9_]+)/(?:static/.+|templates/([A-Za-z0-9_]+)/page_[^/]*\\.html)$');

// The group of one graph node: core for a file under web/, <kind>.<name> for a package part; null
// when the node's path is in no group.
function groupOf(node) {
  const filePath = node.split('#')[0];  // strip the #bootstrap or #markup of a template node
  if (filePath.startsWith('web/')) return 'core';
  const match = PACKAGE_PART.exec(filePath);
  if (match === null || (match[3] !== undefined && match[3] !== match[2])) return null;
  return `${match[1]}.${match[2]}`;
}

// The groups whose names a package may use: core, the package itself, and its one exception.
function allowedGroups(group) {
  const [kind, name] = group.split('.');
  const allowed = new Set(['core', group]);
  if (kind === 'features' && CHAT_CHANNELS.has(name)) allowed.add('features.chat_threads');
  if (kind === 'backends' && BACKEND_VARIANTS.has(name)) allowed.add('backends.claude_code');
  return allowed;
}

let pagesCache = null;

// The dependency graph of the checked pages, computed once: sucrase resolves from the repo's
// node_modules, so a missing install runs the same idempotent npm step
// tests/tailwind_class_coverage.test.js runs. The guard keeps the install off the concurrent
// node --test runs, whose suites then share the finished tree.
function pages() {
  if (pagesCache !== null) return pagesCache;
  if (!fs.existsSync(path.join(ROOT, 'node_modules', 'sucrase'))) {
    execFileSync('npm', ['install', '--no-audit', '--no-fund'], { cwd: ROOT, stdio: 'pipe' });
  }
  // The bound turns a hung graph build into a test failure instead of a CI hang.
  const stdout = execFileSync(process.execPath, [GRAPH_SCRIPT, ROOT],
      { cwd: ROOT, maxBuffer: 64 * 1024 * 1024, timeout: 300 * 1000 });
  pagesCache = JSON.parse(stdout).pages;
  return pagesCache;
}

// The (user, definer) pairs that break a direction, per rule, with the used names per pair. A node
// in no group is left to the group check.
function directionBreaks(pagess) {
  const breaks = { 'core-uses-package': {}, 'package-uses-other-group': {}, 'core-loads-late': {} };
  for (const pageName of PAGE_NAMES) {
    const page = pagess[pageName];
    const position = new Map(page.nodes.map((node, index) => [node, index]));
    const groups = new Map(page.nodes.map((node) => [node, groupOf(node)]));
    for (const [user, targets] of Object.entries(page.edges)) {
      for (const [definer, names] of Object.entries(targets)) {
        const userGroup = groups.get(user);
        const definerGroup = groups.get(definer);
        if (userGroup === null || definerGroup === null) continue;
        let key = null;
        if (userGroup === 'core' && definerGroup !== 'core') {
          key = 'core-uses-package';
        } else if (userGroup !== 'core' && !allowedGroups(userGroup).has(definerGroup)) {
          key = 'package-uses-other-group';
        } else if (userGroup === 'core' && definerGroup === 'core' && position.get(definer) > position.get(user)) {
          key = 'core-loads-late';
        } else {
          continue;
        }
        const pair = (breaks[key][user] = breaks[key][user] || {});
        pair[definer] = (pair[definer] || new Set());
        for (const name of names) pair[definer].add(name);
      }
    }
  }
  return breaks;
}

// One failure line per violating pair.
function breakFailures(breaks, message) {
  const failures = [];
  const byUser = Object.entries(breaks).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
  for (const [user, definers] of byUser) {
    const byDefiner = Object.entries(definers).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
    for (const [definer, names] of byDefiner) {
      failures.push(message(user, definer, [...names].sort().join(', ')));
    }
  }
  return failures;
}

test('frontend files have groups and unique global names', () => {
  const pagess = pages();
  const failures = [];
  for (const pageName of PAGE_NAMES) {
    for (const node of pagess[pageName].nodes) {
      if (groupOf(node) === null) {
        failures.push(`${node}: outside the groups web/ (core) and src/<kind>/<name> (package); move it into one.`);
      }
    }
    for (const [name, nodes] of Object.entries(pagess[pageName].multiDefs).sort()) {
      failures.push(`${name}: defined by ${nodes.join(', ')}; one global name is defined in one file only.`);
    }
  }
  assert.equal(failures.length, 0, failures.join('\n'));
});

test('core uses no name a package defines', () => {
  const failures = breakFailures(directionBreaks(pages())['core-uses-package'], (user, definer, names) =>
    `${user} uses ${names}, defined by ${definer} (group ${groupOf(definer)}); core uses no name a ` +
    `package defines. ${FIX_THE_UPWARD_USE}`);
  assert.equal(failures.length, 0, failures.join('\n'));
});

test('a package uses only core, itself and its exception', () => {
  const failures = breakFailures(directionBreaks(pages())['package-uses-other-group'], (user, definer, names) => {
    const may = [...allowedGroups(groupOf(user))].sort().join(', ');
    return `${user} (group ${groupOf(user)}) uses ${names}, defined by ${definer} ` +
      `(group ${groupOf(definer)}); ${groupOf(user)} may use names of ${may} only.`;
  });
  assert.equal(failures.length, 0, failures.join('\n'));
});

test('a core file uses only core names defined before it loads', () => {
  const failures = breakFailures(directionBreaks(pages())['core-loads-late'], (user, definer, names) =>
    `${user} uses ${names}, defined by ${definer}, which loads later; a core file uses only names ` +
    `that core files loaded before it define. ${FIX_THE_UPWARD_USE}`);
  assert.equal(failures.length, 0, failures.join('\n'));
});

test('web/static has no package static directory', () => {
  const failures = [];
  for (const kind of ['features', 'backends']) {
    if (fs.existsSync(path.join(ROOT, 'web', 'static', kind))) {
      failures.push(
        `web/static/${kind}/: this URL prefix belongs to package static files, served from ` +
        `src/${kind}/<name>/static/; remove the directory.`);
    }
  }
  assert.equal(failures.length, 0, failures.join('\n'));
});
