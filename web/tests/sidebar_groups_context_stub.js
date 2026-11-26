// ---------------------------------------------------------------------------
// vm-context loader for web/static/js/sidebar/groups.js, shared by the node
// --test harnesses that drive groups.js standalone. It holds one base context
// carrying the globals renderSessionItem reaches for, then one load sequence:
// createContext, utils.js, sidebar/namespace.js, sidebar/status.js,
// sidebar/groups.js — the page's order, which the row title rule needs:
// groups.js reads sessionRowTitle, which status.js defines and the page loads
// first, and the column-form reader, which utils.js defines. The load
// order every suite depends on lives in one place. The indicator fakes return
// after status.js's load, so the suites keep the empty indicators they had.
// The sandbox `escapeHtmlAttr`
// global wraps `escapeHtml`, not escapeHtmlText: that is the wrapper
// escape_html_stub.js documents as reproducing production escapeHtmlAttr, and
// groups.js interpolates its output into title/data attributes.
// ---------------------------------------------------------------------------
const vm = require('node:vm');

const {readStatic, readPackageStatic, runIconModule} = require('./read_static');
const {escapeHtml, escapeHtmlText} = require('./escape_html_stub');

const UTILS_JS = readStatic('utils.js');
const NAMESPACE_JS = readStatic('sidebar/namespace.js');
const CHAT_NAMESPACE_JS = readStatic('chat/namespace.js');
const CHAT_SHARED_JS = readStatic('chat/shared.js');
const STATUS_JS = readStatic('sidebar/status.js');
const GROUPS_JS = readStatic('sidebar/groups.js');
const CRON_JS = readPackageStatic('features', 'cron', 'cron.js');

const BACKEND_OPTIONS = {
  'claude-opus-5': 'CC · Opus 5',
  'opencode-glm52': 'OC · GLM-5.2',
  'codex-gpt-5.3-codex-spark': 'Codex · GPT-5.3 Codex Spark xHigh (personal)',
  'codex-gpt-luna': 'Codex GPT Luna',
};

const BACKEND_POOLS = [{id: 'auto', label: 'Auto'}, {id: 'cheap', label: 'Cheap'}];

// The page's server-rendered label table: BACKEND_LABELS[backend][member || ""] is the [full, compact] pair.
const BACKEND_LABELS = {
  'claude-opus-5': {'': ['CC · Opus 5', 'Opus 5']},
  'opencode-glm52': {'': ['OC · GLM-5.2', 'GLM-5.2']},
  'codex-gpt-5.3-codex-spark': {'': ['Codex · GPT-5.3 Codex Spark xHigh (personal)', 'GPT-5.3 Codex Spark xHigh (personal)']},
  'codex-gpt-luna': {'': ['Codex GPT Luna', 'Codex GPT Luna']},
  auto: {
    '': ['Auto', 'Auto'],
    'claude-opus-5': ['Auto → CC · Opus 5', 'Auto → Opus 5'],
  },
  cheap: {
    '': ['Cheap', 'Cheap'],
    'codex-gpt-luna': ['Cheap → Codex GPT Luna', 'Cheap → Codex GPT Luna'],
  },
};

// The status.js members the suites stub out: status.js defines them for real,
// so they go back to their fakes after its load.
const STATUS_FAKES = {
  getSessionIndicatorState: () => 'idle',
  renderSessionIndicators: () => '',
  renderPendingTriggerIndicator: () => '',
  renderPendingPlanApprovalIndicator: () => '',
  recordRenderedSessionStatus: () => {},
  recordTreeShape: () => {},
};

function loadGroups() {
  const Sidebar = {state: {}};
  const context = {
    Sidebar,
    globalThis: null,
    BACKEND_OPTIONS,
    BACKEND_POOLS,
    BACKEND_LABELS,
    SESSION_ID: 'other-session',
    console: {error: () => {}},
    // A package script the page fetched after its document reads readyState; no paint is on screen in this harness,
    // so the repaint such a script asks for has no last paint to re-render.
    document: {readyState: 'complete', getElementById: () => null, addEventListener: () => {}},
    localStorage: {getItem: () => null, setItem: () => {}},
    fetch: () => Promise.resolve({ok: true, json: async () => ({})}),
    JSON_HEADERS: {},
    escapeHtml: escapeHtmlText,
    escapeHtmlAttr: (value) => escapeHtml(value == null ? '' : String(value)),
    relativeTime: () => 'Jul 29, 5:12 PM',
    formatBubbleTime: () => '',
  };
  context.globalThis = context;
  vm.createContext(context);
  runIconModule(context);
  // utils.js first, as on the page, which loads it ahead of every chat and sidebar module: groups.js reads
  // expandSessionColumns and sessionUpdatedMs from it. Its real time forms replace the stubs above, so those land
  // back after its load.
  vm.runInContext(UTILS_JS, context, {filename: 'utils.js'});
  context.relativeTime = () => 'Jul 29, 5:12 PM';
  context.formatBubbleTime = () => '';
  // namespace.js next: it supplies the shared sidebar namespace.
  vm.runInContext(NAMESPACE_JS, context, {filename: 'namespace.js'});
  // chat/shared.js carries sessionModelLabel, the one label function the sidebar rows read. The page loads it after
  // sidebar/namespace.js; its exports overwrite the escape and bubble-time stubs, so those land back after its load.
  vm.runInContext(CHAT_NAMESPACE_JS, context, {filename: 'chat/namespace.js'});
  vm.runInContext(CHAT_SHARED_JS, context, {filename: 'chat/shared.js'});
  context.escapeHtml = escapeHtmlText;
  context.escapeHtmlAttr = (value) => escapeHtml(value == null ? '' : String(value));
  context.formatBubbleTime = () => '';
  vm.runInContext(STATUS_JS, context, {filename: 'status.js'});
  Object.assign(context, STATUS_FAKES);
  vm.runInContext(GROUPS_JS, context, {filename: 'groups.js'});
  return context;
}

// loadGroups plus the cron script, run after groups.js as the page runs a
// package's script after core. The script also registers its Workspace badge
// with filters.js and its Task & context section with namespace.js, which this
// harness does not load, so stubs take both.
function loadGroupsWithCron() {
  const context = loadGroups();
  context.registerWorkspaceBadge = () => {};
  context.registerTaskContextSection = () => {};
  // The load sequence runs while the document parses; a real page fetches this module after the document, and the
  // late-load path (repaint of the painted rows, re-read of the Workspace badges) has its own tests.
  context.document.readyState = 'loading';
  vm.runInContext(CRON_JS, context, {filename: 'cron.js'});
  context.document.readyState = 'complete';
  return context;
}

// One rendered sidebar row for *session* spread over the defaults every row
// test shares.
function row(session) {
  const {renderSessionItem} = loadGroups().Sidebar;
  return renderSessionItem(
    {id: 's1', name: 'demo session', updated_at: '2026-07-29T17:12:00Z', ...session},
    'all'
  );
}

module.exports = {loadGroups, loadGroupsWithCron, row};
