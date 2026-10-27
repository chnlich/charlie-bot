// Dependency graph of the pages that tests/test_frontend_structure.py checks. Prints one JSON
// object: {packages, pages: {chat}} with, per page, the templates, the nodes in
// page order, the edges (using node -> defining node -> [names]), the strongly connected
// components of two or more nodes, the global names defined in more than one node, and the load
// errors. A node is a loaded script (repo-relative path), '<template>#bootstrap' for a template's
// inline <script> bodies, or '<template>#markup' for its inline handler attributes.
//
// The graph algorithm ports the restructure's frozen measurement script page_graph.js
// (~/scripts/20261008_web_frontend_restructure/): the parts of a page run in page order inside a
// vm context with permissive stubs; definitions are the global, Chat.* and Sidebar.* keys each
// part adds, plus the top-level let/const/class names from the sucrase tokenizer; uses are the
// unshadowed identifier accesses and the name( calls inside string literals. The restructure's
// fidelity check compares this script's chat page against page_graph.js on one tree, so an
// algorithm change belongs in both files.
//
// Usage: node tests/frontend_structure_graph.js <repo root>   (npm install first: sucrase
// resolves from the repo's node_modules)
'use strict';
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const root = process.argv[2];
const sucraseDist = path.join(root, 'node_modules', 'sucrase', 'dist');
const {parse} = require(path.join(sucraseDist, 'parser'));
const {TokenType: tt} = require(path.join(sucraseDist, 'parser/tokenizer/types'));
const {IdentifierRole} = require(path.join(sucraseDist, 'parser/tokenizer'));
const identifyShadowedGlobals = require(path.join(sucraseDist, 'identifyShadowedGlobals')).default;

const read = (rel) => fs.readFileSync(path.join(root, rel), 'utf8');

// Registered packages in registration order: "src.features.cron" -> "features/cron".
const registrations = read('src/app/registrations.py');
const packagesBlock = registrations.match(/PACKAGES\s*=\s*\(([\s\S]*?)\n\)/)[1];
const packages = [...packagesBlock.matchAll(/"src\.(features|backends)\.([A-Za-z0-9_]+)"/g)].map((m) => `${m[1]}/${m[2]}`);

// The page part templates of one package: page_*.html files under its templates directory, in
// walk order.
function pageTemplatesOf(pkg) {
  const dir = path.join(root, 'src', pkg, 'templates');
  const out = [];
  (function walk(d) {
    if (!fs.existsSync(d)) return;
    for (const e of fs.readdirSync(d, {withFileTypes: true}).sort((a, b) => a.name.localeCompare(b.name))) {
      if (e.isDirectory()) walk(path.join(d, e.name));
      else if (/^page_.*\.html$/.test(e.name)) out.push(path.relative(root, path.join(d, e.name)));
    }
  })(dir);
  return out;
}

// One <script src="/static/..."> path as a repo-relative file: web/static for js/, the package's
// static directory for <kind>/<name>/.
function scriptPathOf(src) {
  if (src.startsWith('js/')) return `web/static/${src}`;
  const parts = src.split('/');  // <kind>/<name>/<file...>
  return `src/${parts[0]}/${parts[1]}/static/${parts.slice(2).join('/')}`;
}

function scriptSrcs(html) {
  return [...html.matchAll(/<script\s+src="\/static\/([^"?]+)/g)].map((m) => scriptPathOf(m[1]));
}

const stripJinja = (s) => s.replace(/\{\{[\s\S]*?\}\}/g, 'null').replace(/\{%[\s\S]*?%\}/g, '').replace(/\{#[\s\S]*?#\}/g, '');
const inlineBodiesOf = (html) => [...html.matchAll(/<script>([\s\S]*?)<\/script>/g)].map((m) => stripJinja(m[1]));
const handlerCodeOf = (html) => [...html.matchAll(/\son[a-z]+="([^"]*)"/g)].map((m) => stripJinja(m[1])).join(';\n');

// A permissive value: any property read, call or construction returns another permissive value.
function permissive() {
  const fn = function() {};
  return new Proxy(fn, {
    get(t, k) {
      if (k === Symbol.toPrimitive) return () => '';
      if (k === Symbol.iterator) return function* () {};
      if (k === 'then') return undefined;
      if (k in t && k !== 'length' && k !== 'name') return t[k];
      return permissive();
    },
    set(t, k, v) { t[k] = v; return true; },
    apply() { return permissive(); },
    construct() { return permissive(); },
    has() { return true; },
  });
}

const HOST_GLOBALS = ['marked', 'hljs', 'katex', 'renderMathInElement', 'Diff2HtmlUI', 'Terminal',
  'FitAddon',
  'document', 'navigator', 'localStorage', 'sessionStorage', 'location', 'history', 'performance',
  'requestAnimationFrame', 'cancelAnimationFrame', 'ResizeObserver', 'MutationObserver', 'IntersectionObserver',
  'WebSocket', 'EventSource', 'AudioContext', 'MediaRecorder', 'Notification', 'matchMedia', 'getComputedStyle',
  'addEventListener', 'removeEventListener', 'Element', 'HTMLElement', 'Node', 'CustomEvent', 'Event',
  'DOMParser', 'Blob', 'FileReader', 'URL', 'URLSearchParams', 'TextEncoder', 'TextDecoder', 'crypto',
  'AbortController', 'Worker', 'Audio', 'Image', 'alert', 'confirm', 'prompt', 'open', 'scrollTo',
  'innerWidth', 'innerHeight', 'devicePixelRatio', 'screen', 'visualViewport', 'clipboardData', 'atob', 'btoa',
  'fetch', 'FormData', 'Headers', 'Response', 'Request', 'queueMicrotask', 'structuredClone',
  'AudioWorkletProcessor', 'registerProcessor', 'sampleRate', 'currentTime',
  'dispatchEvent'];

function newContext() {
  const sandbox = {};
  for (const k of HOST_GLOBALS) sandbox[k] = permissive();
  for (const k of ['setTimeout', 'clearTimeout', 'setInterval', 'clearInterval']) sandbox[k] = () => 0;
  sandbox.console = {log() {}, warn() {}, error() {}, info() {}, debug() {}};
  const ctx = vm.createContext(sandbox);
  vm.runInContext('globalThis.window = globalThis; globalThis.self = globalThis;', ctx);
  return ctx;
}

// The graph of one page: every template's inline scripts first (bootstrap), then every loaded
// script in load order, then every template's inline handler attributes (markup).
function computeGraph(templates) {
  const ctx = newContext();
  const templateHtml = Object.fromEntries(templates.map((t) => [t, read(t)]));
  const scripts = [];
  for (const t of templates) for (const s of scriptSrcs(templateHtml[t])) if (!scripts.includes(s)) scripts.push(s);

  function keysOf(expr) {
    try { return new Set(vm.runInContext(`Object.getOwnPropertyNames(${expr} || {})`, ctx)); } catch (e) { return new Set(); }
  }

  const defs = {};          // name -> [node]   (bare globals)
  const nsDefs = {Chat: {}, Sidebar: {}};
  const loadErrors = {};
  function addDef(map, name, node) { (map[name] = map[name] || []).includes(node) || map[name].push(node); }

  function runPart(node, code) {
    const g0 = keysOf('globalThis');
    const c0 = keysOf('globalThis.Chat');
    const s0 = keysOf('globalThis.Sidebar');
    try { vm.runInContext(code, ctx, {filename: node}); } catch (e) { loadErrors[node] = String(e && e.message || e); }
    for (const k of keysOf('globalThis')) if (!g0.has(k)) addDef(defs, k, node);
    for (const k of keysOf('globalThis.Chat')) if (!c0.has(k)) addDef(nsDefs.Chat, k, node);
    for (const k of keysOf('globalThis.Sidebar')) if (!s0.has(k)) addDef(nsDefs.Sidebar, k, node);
  }

  for (const t of templates) {
    const bodies = inlineBodiesOf(templateHtml[t]);
    if (!bodies.length) continue;
    const node = `${t}#bootstrap`;
    for (const body of bodies) runPart(node, body);
  }
  const codeOf = {};
  for (const file of scripts) {
    codeOf[file] = read(file);
    runPart(file, codeOf[file]);
  }

  const tokensOf = (code) => parse(code, false, false, false);

  // Top-level lexical declarations (let/const/class at script scope) share one global lexical scope.
  for (const file of scripts) {
    const code = codeOf[file];
    const {tokens} = tokensOf(code);
    for (const t of tokens) {
      if (t.type === tt.name && t.scopeDepth === 0 &&
          (t.identifierRole === IdentifierRole.TopLevelDeclaration ||
           t.identifierRole === IdentifierRole.BlockScopedDeclaration)) {
        addDef(defs, code.slice(t.start, t.end), file);
      }
    }
  }
  for (const k of Object.keys(defs)) if (HOST_GLOBALS.includes(k) || k === 'window' || k === 'self') delete defs[k];

  const globalNames = new Set(Object.keys(defs));
  function usesOf(code) {
    const {tokens, scopes} = tokensOf(code);
    const tp = {tokens, identifierNameForToken: (t) => code.slice(t.start, t.end)};
    identifyShadowedGlobals(tp, scopes, globalNames);
    const bare = new Set();
    const ns = {Chat: new Set(), Sidebar: new Set()};
    for (let i = 0; i < tokens.length; i++) {
      const t = tokens[i];
      if (t.type === tt.name) {
        const name = code.slice(t.start, t.end);
        const prev = tokens[i - 1];
        const afterDot = prev && prev.type === tt.dot;
        if (!afterDot && !t.shadowsGlobal && globalNames.has(name) &&
            (t.identifierRole === IdentifierRole.Access || t.identifierRole === IdentifierRole.ObjectShorthand)) {
          bare.add(name);
        }
        const next = tokens[i + 1], nn = tokens[i + 2];
        if (next && next.type === tt.dot && nn && nn.type === tt.name) {
          const member = code.slice(nn.start, nn.end);
          if (name === 'Chat' || name === 'Sidebar') ns[name].add(member);
          if ((name === 'window' || name === 'globalThis' || name === 'global') && globalNames.has(member)) bare.add(member);
        }
      } else if (t.type === tt.string || t.type === tt.template) {
        for (const m of code.slice(t.start, t.end).matchAll(/([A-Za-z_$][\w$]*)\s*\(/g)) {
          if (globalNames.has(m[1])) bare.add(m[1]);
        }
      }
    }
    return {bare, ns};
  }

  const edges = {};  // from -> {to -> [names]}
  function addEdge(from, to, name) {
    if (from === to) return;
    edges[from] = edges[from] || {};
    (edges[from][to] = edges[from][to] || []).push(name);
  }
  function addUses(node, code) {
    let used;
    try { used = usesOf(code); } catch (e) { loadErrors[`${node}#uses`] = String(e.message); return; }
    for (const name of used.bare) for (const def of defs[name] || []) addEdge(node, def, name);
    for (const nsName of ['Chat', 'Sidebar']) {
      for (const name of used.ns[nsName]) for (const def of nsDefs[nsName][name] || []) addEdge(node, def, `${nsName}.${name}`);
    }
  }
  for (const t of templates) {
    const bodies = inlineBodiesOf(templateHtml[t]);
    if (bodies.length) addUses(`${t}#bootstrap`, bodies.join('\n;\n'));
  }
  for (const file of scripts) addUses(file, codeOf[file]);
  for (const t of templates) {
    const handlers = handlerCodeOf(templateHtml[t]);
    if (!handlers) continue;
    addUses(`${t}#markup`, handlers);
  }

  // Tarjan SCC over the nodes in page order: bootstrap nodes, scripts, markup nodes.
  const nodes = [
    ...templates.filter((t) => inlineBodiesOf(templateHtml[t]).length).map((t) => `${t}#bootstrap`),
    ...scripts,
    ...templates.filter((t) => handlerCodeOf(templateHtml[t])).map((t) => `${t}#markup`),
  ];
  let index = 0; const stack = []; const on = new Set(); const idx = {}; const low = {}; const sccs = [];
  function strong(v) {
    idx[v] = low[v] = index++; stack.push(v); on.add(v);
    for (const w of Object.keys(edges[v] || {})) {
      if (!(w in idx)) { strong(w); low[v] = Math.min(low[v], low[w]); }
      else if (on.has(w)) low[v] = Math.min(low[v], idx[w]);
    }
    if (low[v] === idx[v]) {
      const comp = []; let w;
      do { w = stack.pop(); on.delete(w); comp.push(w); } while (w !== v);
      if (comp.length > 1) sccs.push(comp.sort());
    }
  }
  for (const v of nodes) if (!(v in idx)) strong(v);

  const multiDefs = Object.fromEntries(Object.entries(defs).filter(([, f]) => f.length > 1));
  return {templates, nodes, edges, sccs, multiDefs, loadErrors};
}

// The page the structure test checks: index.html plus every package's page part templates, in
// registration order.
const chatTemplates = ['web/templates/index.html'];
for (const pkg of packages) chatTemplates.push(...pageTemplatesOf(pkg));

console.log(JSON.stringify({
  packages,
  pages: {
    chat: computeGraph(chatTemplates),
  },
}, null, 1));
