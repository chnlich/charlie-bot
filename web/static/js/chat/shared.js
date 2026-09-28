
(function() {
  const Chat = globalThis.Chat;

// Roles whose messages open a chat turn — the one list, read by the shared
// span rule below and by rendering.js's DOM matcher.
const STIMULUS_ROLES = ['user', 'scheduled_trigger', 'agent_message', 'worker_summary', 'child_report'];

// ---------------------------------------------------------------------------
// The turn-input rule, stated once.
//
// A chat round renders as one turn: every input the session received while
// that round ran (a user message, a relayed agent_message, a scheduled
// trigger, a child report, a worker summary) shows in the turn that handled
// it, outside the folded steps. Both turn-layout consumers — turn-engine.js
// over message entries and rendering.js over rendered elements — call this
// one pure function; neither keeps a second copy of the rule.
//
// `items` is one separator-terminated span's messages in arrival order (the
// separator itself excluded); `roleOf(item)` reads the message's role, which
// is what lets the two consumers share the function. Inputs a round queued
// while an earlier round was still running arrive after that round's first
// assistant message: they are carried out of this span and the caller moves
// them ahead of the next span, in arrival order, where they become that
// span's own inputs (they sit at its front, before its first assistant).
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
  // Head: today's priority over the span's inputs, carried-in ones included —
  // the last user input, else the last input of any stimulus role, else
  // body[0]. Without a stimulus the fold keeps today's shape too: it starts
  // after body[0]. With inputs the fold holds the span's work — from the
  // first assistant message to the conclusion, minus the carried stimuli —
  // so notices between the last input and the first assistant message (a
  // model switch, say) stay outside the fold next to the inputs.
  let head = null;
  for (let i = ownInputs.length - 1; i >= 0; i--) {
    if (roleOf(ownInputs[i]) === 'user') { head = ownInputs[i]; break; }
  }
  if (!head && ownInputs.length) head = ownInputs[ownInputs.length - 1];
  if (!head) head = items[0] || null;
  if (!head) return {ownInputs, carried, head: null, conclusion: null, fold: []};
  let fold = [];
  if (conclusion) {
    if (ownInputs.length) {
      const queued = new Set(carried);
      for (let i = firstAssistantIdx; i < conclusionIdx; i++) {
        if (!queued.has(items[i])) fold.push(items[i]);
      }
    } else {
      fold = items.slice(1, conclusionIdx);
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
// Every caller's host renders its text at text-xs, except the workers.js
// assistant bubble (text-sm); the pinned class holds the button at text-xs
// there too.
function showMoreToggleHtml(id, restHtml) {
  return `<span id="${id}-short">… <button onclick="document.getElementById('${id}-short').style.display='none';document.getElementById('${id}-full').style.display='inline'" class="text-blue-400 hover:underline text-xs">Show more</button></span><span id="${id}-full" style="display:none">${restHtml}</span>`;
}

// Collapsed "Thinking…" button: the onclick flips the target element's display
// in place, so the paired element must carry this id and start hidden. The id
// is interpolated into a DOM query and must be page-unique — chat mints one per
// message; the streaming draft is a singleton, so its fixed id cannot collide;
// workers thread events mint one per event. Each caller passes its palette
// class; the button label and flip mechanism are the shared part.
function thinkingButtonHtml(id, buttonClass) {
  return `<button onclick="const el=document.getElementById('${id}');el.style.display=el.style.display==='none'?'block':'none'" class="${buttonClass}">Thinking…</button>`;
}

// Collapsed "Thinking…" block at the chat palette: the shared button plus the
// hidden thinking text.
function thinkingToggleHtml(id, thinking) {
  return `${thinkingButtonHtml(id, 'text-xs text-slate-500 hover:text-slate-400 italic mb-1')}<div id="${id}" style="display:none" class="text-xs text-slate-500 whitespace-pre-wrap mb-2">${escapeHtml(String(thinking))}</div>`;
}

// Tool-name chip on a turn's tool-call row. Both renderers stamp it:
// rendering.js in the chat transcript and workers.js in the thread event
// detail. The caller passes the resolved display name; the helper escapes it.
function toolNameChipHtml(name) {
  return '<span class="px-2 py-0.5 rounded-full text-xs font-medium bg-blue-900/60 text-blue-300 border border-blue-700/50">'
    + escapeHtml(name) + '</span>';
}

// One-line summary of a tool call's input: the argument that names what the tool
// acts on (command, file path, pattern), else the first input value. `limit` caps
// the visible text (0 = never truncated) and both consumers truncate on it. Tool
// names arrive in both cases — 'Bash' from Claude Code transcripts, 'bash' from
// opencode events — so both spellings map to the command line.
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

function formatBubbleTime(isoStr) {
  if (!isoStr) return '';
  const d = new Date(isoStr);
  return d.toLocaleString('en-US', {
    month: 'short', day: 'numeric',
    hour: 'numeric', minute: '2-digit', second: '2-digit', hour12: true, timeZoneName: 'short'
  });
}

function messageRenderId(msg) {
  if (!msg || msg.id == null || msg.id === '') return '';
  return String(msg.id);
}

function messageIdentityAttrs(msg) {
  const id = messageRenderId(msg);
  let attrs = id ? ' data-message-id="' + escapeHtmlAttr(id) + '"' : '';
  if (msg && msg.role) attrs += ' data-message-role="' + escapeHtmlAttr(msg.role) + '"';
  // The turn fold row reads its time field from here — a message without a
  // timestamp emits no attribute and the row's time field stays empty.
  if (msg && msg.timestamp) attrs += ' data-message-ts="' + escapeHtmlAttr(msg.timestamp) + '"';
  return attrs;
}

function isRenderedMessage(msg) {
  const id = messageRenderId(msg);
  if (!id) return false;
  return document.querySelector('[data-message-id="' + CSS.escape(id) + '"]') !== null;
}

const GLOBALS = {
  splitTurnSpan,
  shouldAutoScroll,
  escapeHtml,
  escapeHtmlAttr,
  isRenderedMessage,
  showMoreToggleHtml,
  thinkingButtonHtml,
  thinkingToggleHtml,
  toolNameChipHtml,
  formatBubbleTime,
  STIMULUS_ROLES,
};
const CHAT_ONLY = {
  escapeJsSingleQuoted,
  messageIdentityAttrs,
  toolInputSummary,
};
Chat.wire(GLOBALS, CHAT_ONLY);

})();
