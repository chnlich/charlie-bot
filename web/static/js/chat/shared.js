
(function() {
  const Chat = globalThis.Chat;

// Roles whose messages open a chat turn — the one list, read by the shared
// span rule below and by rendering.js's DOM matcher.
const STIMULUS_ROLES = ['user', 'scheduled_trigger', 'agent_message', 'worker_summary', 'child_report'];
// The page-request retry backoff: a failed request (the switch's bootstrap, an older page, a send) waits 1 s, doubling
// to a 30 s cap, and retries the same request. Every page request path reads these two values.
const PAGE_REQUEST_RETRY_BASE_MS = 1000;
const PAGE_REQUEST_RETRY_MAX_MS = 30000;

// The pending-trigger bell's body, read from the icon table (icons.js): the sidebar row's bell
// (sidebar/status.js) still wraps this body in its own svg element. The tray draws the bell through
// cbIcon directly.
const PENDING_TRIGGER_BELL_SVG_PATH = globalThis.CB_ICON_TABLE.notification.shapes;

// The disclosure mark for a control's state: chevron-right while its body is hidden, chevron-down
// while it is open. The state picks the key, so no CSS turns the glyph.
function disclosureIcon(open, size, cls = '') {
  return cbIcon(open ? 'disclosure-open' : 'disclosure', size, cls);
}

// ---------------------------------------------------------------------------
// The turn-input rule, stated once.
//
// A chat round renders as one turn: every input the session received while
// that round ran shows in the turn that handled it, outside the folded steps.
// The inputs are a user message, a relayed agent_message, a scheduled trigger,
// a child report and a worker summary. Both turn-layout consumers —
// turn-engine.js over message entries and rendering.js over rendered
// elements — call this one pure function. Neither keeps a second copy of the
// rule.
//
// `items` is one separator-terminated span's messages in arrival order (the
// separator itself excluded). `roleOf(item)` reads the message's role, which
// is what lets the two consumers share the function. Inputs a round queued
// while an earlier round was still running arrive after that round's first
// assistant message. They are carried out of this span, and the caller moves
// them ahead of the next span in arrival order. There they become that span's
// own inputs, at its front before its first assistant.
function splitTurnSpan(items, roleOf) {
  let conclusion = null;
  for (let i = items.length - 1; i >= 0; i--) {
    if (roleOf(items[i]) === 'assistant') { conclusion = items[i]; break; }
  }
  const conclusionIdx = conclusion ? items.indexOf(conclusion) : items.length;
  let firstAssistantIdx = items.length;
  for (let i = 0; i < items.length; i++) {
    if (roleOf(items[i]) === 'assistant') { firstAssistantIdx = i; break; }
  }
  const ownInputs = [];
  const carried = [];
  items.forEach((item, i) => {
    if (!STIMULUS_ROLES.includes(roleOf(item))) return;
    (i < firstAssistantIdx ? ownInputs : carried).push(item);
  });
  // Head: the priority over the span's inputs, carried-in ones included —
  // the last user input, else the last input of any stimulus role, else
  // body[0]. The fold holds the span's work and never a carried stimulus: a
  // carried stimulus has left this span for the next one, in every branch.
  // With own inputs it runs from the first assistant message to the
  // conclusion. Notices between the last input and the first assistant
  // message (a model switch, say) stay outside the fold, next to the inputs.
  // Without own inputs it starts after body[0], again minus the carried
  // stimuli.
  let head = null;
  for (let i = ownInputs.length - 1; i >= 0; i--) {
    if (roleOf(ownInputs[i]) === 'user') { head = ownInputs[i]; break; }
  }
  if (!head && ownInputs.length) head = ownInputs[ownInputs.length - 1];
  if (!head) head = items[0] || null;
  if (!head) return {ownInputs, carried, head: null, conclusion: null, fold: []};
  let fold = [];
  if (conclusion) {
    const queued = new Set(carried);
    const foldStart = ownInputs.length ? firstAssistantIdx : 1;
    for (let i = foldStart; i < conclusionIdx; i++) {
      if (!queued.has(items[i])) fold.push(items[i]);
    }
  }
  return {ownInputs, carried, head, conclusion, fold};
}

// ---------------------------------------------------------------------------
// Auto-scroll helper — returns true only when user is near the bottom
// ---------------------------------------------------------------------------
function shouldAutoScroll(container, threshold = 150) {
  return container.scrollHeight - container.scrollTop - container.clientHeight < threshold;
}

// The reading position for the legacy (non-engine) views, in the same shape
// the turn engine uses: one message's identity plus its offset from the
// container's visible top. Captured before a full re-render, restored after.
// Live-rect deltas on both ends keep margins and fixture rows cancelled out.
function captureReadingPosition(container) {
  if (!container || typeof container.getBoundingClientRect !== 'function') return null;
  const cTop = container.getBoundingClientRect().top;
  let straddling = null;
  const nodes = container.querySelectorAll('[data-message-id]');
  for (const node of nodes) {
    if (node.getBoundingClientRect().top <= cTop + 1) straddling = node;
    else break;
  }
  if (!straddling) return null;
  return {
    kind: 'message',
    id: straddling.dataset.messageId,
    offset: straddling.getBoundingClientRect().top - cTop,
  };
}

function restoreReadingPosition(container, captured) {
  if (!container || !captured || captured.kind !== 'message') return false;
  const el = container.querySelector(
    '[data-message-id="' + CSS.escape(String(captured.id)) + '"]');
  if (!el) return false;
  const delta = el.getBoundingClientRect().top
    - container.getBoundingClientRect().top - captured.offset;
  if (delta) container.scrollTop += delta;
  return true;
}

function escapeHtml(str) {
  const d = document.createElement('div');
  d.textContent = str;
  return d.innerHTML;
}

// Missing values must render empty: the DOM textContent coercion would turn undefined
// into the literal string "undefined", and worker descriptions may be missing.
function escapeHtmlAttr(str) {
  return escapeHtml(str == null ? '' : String(str)).replace(/"/g, '&quot;').replace(/'/g, '&#39;');
}

function escapeJsSingleQuoted(str) {
  return String(str)
    .replace(/\\/g, '\\\\')
    .replace(/'/g, "\\'")
    .replace(/\n/g, '\\n')
    .replace(/\r/g, '\\r');
}

// Show-more toggle for over-limit text: the click swaps the short span for
// the full one, so the id base must be page-unique (callers randomize it).
// Every caller's host renders its text at text-xs, and the button's pinned
// text-xs keeps it at the host's size.
function showMoreToggleHtml(id, restHtml) {
  return `<span id="${id}-short">… <button onclick="document.getElementById('${id}-short').style.display='none';document.getElementById('${id}-full').style.display='inline'" class="text-blue-400 hover:underline text-xs">Show more</button></span><span id="${id}-full" style="display:none">${restHtml}</span>`;
}

// Collapsed "Thinking…" button: the onclick flips the target element's display
// in place, so the paired element must carry this id and start hidden. The id
// is interpolated into a DOM query and must be page-unique. Chat mints one per
// message. The streaming draft is a singleton, so its fixed id cannot collide.
// Each caller passes its palette class. The button label and flip mechanism are
// the shared part.
function thinkingButtonHtml(id, buttonClass) {
  return `<button onclick="const el=document.getElementById('${id}');el.style.display=el.style.display==='none'?'block':'none'" class="${buttonClass}">Thinking…</button>`;
}

// Collapsed "Thinking…" block at the chat palette: the shared button plus the
// hidden thinking text.
function thinkingToggleHtml(id, thinking) {
  return `${thinkingButtonHtml(id, 'text-xs text-slate-500 hover:text-slate-400 italic mb-1')}<div id="${id}" style="display:none" class="text-xs text-slate-500 whitespace-pre-wrap mb-2">${escapeHtml(String(thinking))}</div>`;
}

// Tool-name chip on a turn's tool-call row: rendering.js stamps it in the
// chat transcript. The caller passes the resolved display name. The helper
// escapes it.
function toolNameChipHtml(name) {
  return '<span class="px-2 py-0.5 rounded-full text-xs font-medium bg-blue-900/60 text-blue-300 border border-blue-700/50">'
    + escapeHtml(name) + '</span>';
}

// One-line summary of a tool call's input: the argument that names what the tool
// acts on (command, file path, pattern), else the first input value. `limit` caps
// the visible text (0 = never truncated) and both consumers truncate on it. Tool
// names arrive in either case, depending on the backend, so both 'Bash' and 'bash'
// map to the command line.
function toolInputSummary(toolName, input) {
  input = input || {};
  const name = String(toolName || '');
  if (name === 'Bash' || name === 'bash') return {text: input.command || '', limit: 80};
  if (name === 'Read' || name === 'Edit' || name === 'Write') return {text: input.file_path || '', limit: 0};
  if (name === 'Glob') return {text: input.pattern || '', limit: 0};
  if (name === 'Grep') return {text: (input.pattern || '') + (input.path ? ' in ' + input.path : ''), limit: 0};
  const first = Object.values(input)[0];
  if (first == null || first === '') return {text: '', limit: 0};
  return {text: typeof first === 'object' ? JSON.stringify(first) : String(first), limit: 60};
}

// A transcript time names the day and the clock time the reader acts on, in the
// reader's own zone. It is the label the sidebar's session rows print (utils.js
// formatTimeLabel), with the year on an instant outside the current year. A
// reader wondering when a message landed reads the minute. The seconds field and
// the zone name are needless precision on every bubble, so the events viewer
// carries the exact instant instead.
function formatBubbleTime(isoStr) {
  return formatTimeLabel(isoStr);
}

function messageRenderId(msg) {
  if (!msg || msg.id == null || msg.id === '') return '';
  return String(msg.id);
}

function messageIdentityAttrs(msg) {
  const id = messageRenderId(msg);
  let attrs = id ? ' data-message-id="' + escapeHtmlAttr(id) + '"' : '';
  if (msg && msg.role) attrs += ' data-message-role="' + escapeHtmlAttr(msg.role) + '"';
  // The worker-only run_header tag lets the turn fold label that Run's row.
  // Ordinary manager messages keep their existing rendered attributes.
  if (msg && msg.kind === 'run_header') attrs += ' data-message-kind="run_header"';
  // The turn fold row reads its time field from here — a message without a
  // timestamp emits no attribute and the row's time field stays empty.
  if (msg && msg.timestamp) attrs += ' data-message-ts="' + escapeHtmlAttr(msg.timestamp) + '"';
  return attrs;
}

// An error row: a system message whose text starts with the error prefix. The
// prefix and its role come from FOR_YOU_RULE, which the store's for_you module
// defines beside the aggregator's error arms (`render_builtin`,
// `rust/crates/runtime/store/src/aggregator.rs`). The For you filter and
// rendering.js both read this one test.
function isErrorRow(msg) {
  return msg.role === FOR_YOU_RULE.error.role && String(msg.content || '').startsWith(FOR_YOU_RULE.error.prefix);
}

// ---------------------------------------------------------------------------
// Chat threads: the platforms a thread-bound session answers on, and the lines a thread posts.
//
// The server keys a platform by its name: `slack`, `discord` or `feishu`. A summon carries the block its listener
// stored under that key, and a reply renders as a `thread_reply` line that names the platform. Every reader of those
// lines goes through the helpers below, so the badge and the label have one owner.
// ---------------------------------------------------------------------------
const PLATFORM_LABELS = {slack: 'Slack', discord: 'Discord', feishu: 'Feishu'};

function platformLabel(platform) {
  if (!Object.hasOwn(PLATFORM_LABELS, platform)) throw new Error('Unknown thread platform: ' + platform);
  return PLATFORM_LABELS[platform];
}

// The platform badge. The tone classes are literal strings, so the Tailwind scanner keeps each one.
const PLATFORM_BADGE_TONE = {
  slack: 'bg-purple-900/40 text-purple-100 border-purple-400/55',
  discord: 'bg-indigo-900/40 text-indigo-100 border-indigo-400/55',
  feishu: 'bg-teal-900/40 text-teal-100 border-teal-400/55',
};

function platformBadgeHtml(platform, extraClass = '') {
  const label = platformLabel(platform);
  return `<span class="inline-flex items-center h-[18px] px-1.5 rounded-[5px] border text-[10.5px] font-bold `
    + `tracking-wide whitespace-nowrap flex-shrink-0 ${PLATFORM_BADGE_TONE[platform]} ${extraClass}">${label}</span>`;
}

// The legacy line a thread reply wrote before the `thread_reply` role. A cached history still holds it, so the reader
// parses it at read time: the history is never rewritten.
const LEGACY_THREAD_REPLY = /^Posted to (Slack|Discord|Feishu): ([\s\S]*)$/;

// A thread reply as the chat shows it, or null for any other message: {platform, content}.
function threadReplyOf(msg) {
  if (msg.role === 'thread_reply') return {platform: msg.platform, content: String(msg.content ?? '')};
  if (msg.role !== 'system') return null;
  const match = LEGACY_THREAD_REPLY.exec(String(msg.content || ''));
  if (!match) return null;
  return {platform: match[1].toLowerCase(), content: match[2]};
}

// The platform of an inbound summon: an agent_message that carries a platform block without a nudge marker. A nudge
// wakes a session on a thread it already joined, so it is not a summon. The server's is_platform_summon reads this rule.
function summonPlatformOf(msg) {
  if (msg.role !== 'agent_message') return null;
  for (const platform of Object.keys(PLATFORM_LABELS)) {
    const block = msg[platform];
    if (block && typeof block === 'object' && !Object.hasOwn(block, 'nudge_of')) return platform;
  }
  return null;
}

function isRenderedMessage(msg) {
  const id = messageRenderId(msg);
  if (!id) return false;
  return document.querySelector('[data-message-id="' + CSS.escape(id) + '"]') !== null;
}

// The mounted turn engine of each message container. turn-engine.js mounts
// and disposes the engines in this map, and every reader asks
// activeTurnEngineFor, so the chat files below turn-engine.js reach the
// engine too.
const turnEngines = new WeakMap();

function activeTurnEngineFor(container) {
  const engine = turnEngines.get(container);
  return engine && engine.alive ? engine : null;
}

// The one model label every surface writes. A pool session reads "<pool> → <member> · <model>": `poolMember` is
// the member the server already filtered to the pool, so a pool with no member reads the pool label alone. A
// single-option session reads its option's label. The compact form drops the backend family prefix ("CC · ") and
// the model, so a row next to a timestamp or a status verdict keeps its width.
function isPoolBackend(backendId) {
  const pools = typeof BACKEND_POOLS === 'undefined' ? [] : BACKEND_POOLS;
  return pools.some((candidate) => candidate.id === backendId);
}

function backendOptionLabel(backendId) {
  const options = typeof BACKEND_OPTIONS === 'undefined' ? {} : BACKEND_OPTIONS;
  return options[backendId] || backendId;
}

// An option label without its backend family prefix: "CC · Opus" reads "Opus".
function withoutFamilyPrefix(label) {
  const sep = label.indexOf(' · ');
  return sep === -1 ? label : label.slice(sep + 3);
}

function sessionModelLabel(sessionBackend, poolMember, model, style) {
  const compact = style === 'compact';
  if (!isPoolBackend(sessionBackend)) {
    const full = backendOptionLabel(sessionBackend);
    return compact ? withoutFamilyPrefix(full) : full;
  }
  const pool = BACKEND_POOLS.find((candidate) => candidate.id === sessionBackend);
  if (!poolMember) return pool.label;
  if (compact) return pool.label + ' → ' + withoutFamilyPrefix(backendOptionLabel(poolMember));
  return pool.label + ' → ' + backendOptionLabel(poolMember) + (model ? ' · ' + model : '');
}

// The backend id the open session holds: a pool id for a pool session, whatever its turn runs on.
function setActiveSessionBackend(backendId) {
  globalThis.ACTIVE_SESSION_BACKEND = backendId || '';
}

function getActiveSessionBackend() {
  return globalThis.ACTIVE_SESSION_BACKEND || '';
}

const GLOBALS = {
  isPoolBackend,
  sessionModelLabel,
  setActiveSessionBackend,
  getActiveSessionBackend,
  splitTurnSpan,
  shouldAutoScroll,
  captureReadingPosition,
  restoreReadingPosition,
  escapeHtml,
  escapeHtmlAttr,
  isRenderedMessage,
  showMoreToggleHtml,
  thinkingButtonHtml,
  thinkingToggleHtml,
  toolNameChipHtml,
  formatBubbleTime,
  STIMULUS_ROLES,
  PAGE_REQUEST_RETRY_BASE_MS,
  PAGE_REQUEST_RETRY_MAX_MS,
};
const CHAT_ONLY = {
  PENDING_TRIGGER_BELL_SVG_PATH,
  platformLabel,
  platformBadgeHtml,
  threadReplyOf,
  summonPlatformOf,
  disclosureIcon,
  escapeJsSingleQuoted,
  messageIdentityAttrs,
  toolInputSummary,
  isErrorRow,
  turnEngines,
  activeTurnEngineFor,
};
Chat.wire(GLOBALS, CHAT_ONLY);

})();
