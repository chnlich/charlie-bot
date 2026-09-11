// ---------------------------------------------------------------------------
// vm-context loader for web/static/js/sidebar/groups.js, shared by the node
// --test harnesses that drive groups.js standalone: one base context carrying
// the globals renderSessionItem reaches for, then one load sequence --
// createContext, sidebar/namespace.js, sidebar/groups.js -- so the load order
// every suite depends on lives in one place. The sandbox `escapeHtmlAttr`
// global wraps `escapeHtml`, not escapeHtmlText: that is the wrapper
// escape_html_stub.js documents as reproducing production escapeHtmlAttr, and
// groups.js interpolates its output into title/data attributes.
// ---------------------------------------------------------------------------
const vm = require('node:vm');

const {readStatic} = require('./read_static');
const {escapeHtml, escapeHtmlText} = require('./escape_html_stub');

const NAMESPACE_JS = readStatic('sidebar/namespace.js');
const GROUPS_JS = readStatic('sidebar/groups.js');

const BACKEND_OPTIONS = {
  'claude-opus-5': 'CC · Opus 5',
  'opencode-glm52': 'OC · GLM-5.2',
  'codex-gpt-5.3-codex-spark': 'Codex · GPT-5.3 Codex Spark xHigh (personal)',
};

function loadGroups() {
  const Sidebar = {expose() {}, state: {}};
  const context = {
    Sidebar,
    globalThis: null,
    BACKEND_OPTIONS,
    SESSION_ID: 'other-session',
    console: {error: () => {}},
    localStorage: {getItem: () => null, setItem: () => {}},
    escapeHtml: escapeHtmlText,
    escapeHtmlAttr: (value) => escapeHtml(value == null ? '' : String(value)),
    relativeTime: () => 'Jul 29, 5:12 PM',
    formatBubbleTime: () => '',
    getSessionIndicatorState: () => 'idle',
    renderSessionIndicators: () => '',
    renderPendingTriggerIndicator: () => '',
    renderPendingPlanApprovalIndicator: () => '',
    renderTuiStatusDot: () => '',
    recordRenderedSessionStatus: () => {},
  };
  context.globalThis = context;
  vm.createContext(context);
  // namespace.js first, as on the page: it supplies the shared sidebar namespace.
  vm.runInContext(NAMESPACE_JS, context, {filename: 'namespace.js'});
  vm.runInContext(GROUPS_JS, context, {filename: 'groups.js'});
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

module.exports = {loadGroups, row};
