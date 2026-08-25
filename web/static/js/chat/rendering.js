
(function() {
  const Chat = globalThis.Chat;
  const formatBubbleTime = Chat.formatBubbleTime;
  const escapeChatAttr = Chat.escapeChatAttr;
  const messageIdentityAttrs = Chat.messageIdentityAttrs;
  const renderRoundRatingButtons = Chat.renderRoundRatingButtons;
  const embedLinkedHtmlArtifacts = Chat.embedLinkedHtmlArtifacts;

// Page depth: 'outline' (one row per finished turn, last turn open),
// 'compact' (every turn open, `N steps` bars closed), 'expanded' (all open).
let pageDepth = 'outline';
Chat.pageDepth = pageDepth;

function toolInputSummary(tool) {
  var input = tool.input || {};
  if (tool.name === 'Bash') return {text: input.command || '', limit: 80};
  if (tool.name === 'Read' || tool.name === 'Edit' || tool.name === 'Write') return {text: input.file_path || '', limit: 0};
  if (tool.name === 'Glob') return {text: input.pattern || '', limit: 0};
  if (tool.name === 'Grep') return {text: (input.pattern || '') + (input.path ? ' in ' + input.path : ''), limit: 0};
  var first = Object.values(input)[0];
  if (first == null || first === '') return {text: '', limit: 0};
  var display = typeof first === 'object' ? JSON.stringify(first) : String(first);
  return {text: display, limit: 60};
}

function renderToolActivity(tools) {
  if (!Array.isArray(tools) || !tools.length) return '';
  var rows = tools.map(function(tool, i) {
    var summary = toolInputSummary(tool);
    var text = summary.text;
    var limit = summary.limit;
    var summaryHtml;
    if (limit > 0 && text.length > limit) {
      var sid = 'ts-' + Math.random().toString(36).slice(2);
      summaryHtml = escapeHtml(text.substring(0, limit))
        + '<span id="' + sid + '-short">… <button onclick="document.getElementById(\'' + sid + '-short\').style.display=\'none\';document.getElementById(\'' + sid + '-full\').style.display=\'inline\'" class="text-blue-400 hover:underline">Show more</button></span>'
        + '<span id="' + sid + '-full" style="display:none">' + escapeHtml(text.substring(limit)) + '</span>';
    } else {
      summaryHtml = escapeHtml(text);
    }
    var outputHtml = '';
    if (tool.output) {
      var outText = String(tool.output);
      var colorCls = tool.is_error ? 'text-red-400' : 'text-slate-400';
      if (outText.length > 500) {
        var oid = 'to-' + Math.random().toString(36).slice(2);
        outputHtml = '<pre class="mt-1 text-xs ' + colorCls + ' whitespace-pre-wrap break-all">'
          + escapeHtml(outText.substring(0, 500))
          + '<span id="' + oid + '-short">… <button onclick="document.getElementById(\'' + oid + '-short\').style.display=\'none\';document.getElementById(\'' + oid + '-full\').style.display=\'inline\'" class="text-blue-400 hover:underline">Show more</button></span>'
          + '<span id="' + oid + '-full" style="display:none">' + escapeHtml(outText.substring(500)) + '</span>'
          + '</pre>';
      } else {
        outputHtml = '<pre class="mt-1 text-xs ' + colorCls + ' whitespace-pre-wrap break-all">' + escapeHtml(outText) + '</pre>';
      }
    }
    var borderCls = i > 0 ? 'border-t border-slate-600/50 ' : '';
    var truncCls = (limit > 0 && text.length > limit) ? '' : 'truncate ';
    return '<div class="' + borderCls + 'py-1.5">'
      + '<div class="flex items-center gap-2">'
      + '<span class="px-2 py-0.5 rounded-full text-xs font-medium bg-blue-900/60 text-blue-300 border border-blue-700/50">' + escapeHtml(tool.name || '') + '</span>'
      + '<span class="text-xs text-slate-400 ' + truncCls + 'flex-1 min-w-0">' + summaryHtml + '</span>'
      + '</div>'
      + outputHtml
      + '</div>';
  }).join('');
  var label = tools.length + ' tool call' + (tools.length !== 1 ? 's' : '');
  return '<div class="mt-2 border border-slate-600/30 rounded-lg overflow-hidden">'
    + '<button onclick="this.nextElementSibling.classList.toggle(\'hidden\')" class="w-full flex items-center justify-between px-3 py-1.5 bg-slate-800/50 hover:bg-slate-800 transition-colors text-xs text-slate-400">'
    + '<span>' + label + '</span>'
    + '<svg class="w-3 h-3" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M19 9l-7 7-7-7"/></svg>'
    + '</button>'
    + '<div class="hidden bg-slate-800/30 px-3 py-2">' + rows + '</div>'
    + '</div>';
}

function renderRawBackendOutput(text) {
  // Keep literal protocol paths inert during linked-artifact post-processing.
  return '<details class="prose-msg border border-slate-600/30 rounded-lg overflow-hidden">'
    + '<summary class="cursor-pointer flex items-center justify-between gap-3 px-3 py-2 bg-slate-800/50 text-xs text-slate-300">'
    + '<span class="font-medium">Raw backend output</span>'
    + '<span class="text-slate-500 whitespace-nowrap">' + text.length + ' characters</span>'
    + '</summary>'
    + '<div class="code-block">'
    + '<div class="code-header"><span class="code-lang">literal text</span><button class="copy-btn" onclick="copyCode(this)">Copy</button></div>'
    + '<pre style="max-height:24rem;overflow:auto"><code data-embedded="1">' + escapeHtml(text) + '</code></pre>'
    + '</div>'
    + '</details>';
}

function renderedMessageId(el) {
  return el && el.dataset ? (el.dataset.messageId || '') : '';
}

function renderedMessageRole(el) {
  return el && el.dataset ? (el.dataset.messageRole || '') : '';
}

function isStableRenderedMessage(el) {
  return Boolean(renderedMessageId(el) && renderedMessageRole(el));
}

// ---------------------------------------------------------------------------
// Turn outline fold
//
// Every finished turn (one stimulus-to-answer round, ending in a separator)
// lives in one `.turn-wrap`: that span's own nodes in their original order,
// plus one `.turn-row`. Folded shows the row and hides the span; open does the
// reverse. Nothing is ever unwrapped and no message is ever moved again, so a
// reader-expanded `N steps` bar, an open recap panel and embedded artifact
// iframes all survive every later derive.
// ---------------------------------------------------------------------------
const STIMULUS_ROLES = ['user', 'scheduled_trigger', 'agent_message', 'worker_summary'];
const TURN_TYPE_LABELS = {user: 'You', scheduled_trigger: 'Trigger', agent_message: 'Agent', worker_summary: 'Worker'};
const TEXT_NODE = 3;

function isStimulusMessage(el) {
  return STIMULUS_ROLES.includes(renderedMessageRole(el));
}

// #streaming-msg and #load-more-sentinel are container fixtures owned by no
// turn — the sentinel sits at the top of a paginated page, exactly where a
// span would otherwise start.
function isTurnSpanNode(el) {
  return el.id !== 'streaming-msg' && el.id !== 'load-more-sentinel';
}

// One pass over the container's flat runs. An existing wrapper is a settled
// turn: its span can no longer change, so it only ends the run before it.
function collectTurns(root) {
  const turns = [];
  let nodes = [];

  Array.from(root.children).forEach(el => {
    if (el.classList.contains('turn-wrap')) {
      nodes = [];
      return;
    }
    if (!isTurnSpanNode(el)) return;
    nodes.push(el);
    if (isStableRenderedMessage(el) && renderedMessageRole(el) === 'separator') {
      const turn = describeTurn(nodes);
      if (turn) turns.push(turn);
      nodes = [];
    }
  });

  return turns;
}

// The parts of one finished turn. Null for a span that stays flat: a bare
// separator with an empty body.
function describeTurn(nodes) {
  const messages = nodes.filter(isStableRenderedMessage);
  const separator = messages[messages.length - 1];
  const body = messages.slice(0, -1);
  const conclusion = lastMessageWithRole(body, 'assistant');
  const stimulus = lastStimulusBefore(body, conclusion);
  const head = stimulus || body[0];
  if (!head) return null;
  const foldRange = conclusion
    ? body.slice(body.indexOf(head) + 1, body.indexOf(conclusion))
    : [];
  return {nodes, head, conclusion, separator, foldRange};
}

function lastMessageWithRole(messages, role) {
  for (let i = messages.length - 1; i >= 0; i--) {
    if (renderedMessageRole(messages[i]) === role) return messages[i];
  }
  return null;
}

function lastStimulusBefore(messages, conclusion) {
  const limit = conclusion ? messages.indexOf(conclusion) : messages.length;
  let stimulus = null;
  for (let i = limit - 1; i >= 0; i--) {
    if (renderedMessageRole(messages[i]) === 'user') return messages[i];
    if (!stimulus && isStimulusMessage(messages[i])) stimulus = messages[i];
  }
  return stimulus;
}

function turnFoldKey(turn) {
  return [
    renderedMessageId(turn.head),
    turn.conclusion ? renderedMessageId(turn.conclusion) : '',
    renderedMessageId(turn.separator),
  ].join('|');
}

function turnFoldLabel(count) {
  return count + ' step' + (count === 1 ? '' : 's');
}

function setTurnFoldBarExpanded(btn, expanded) {
  btn.setAttribute('aria-expanded', String(expanded));
  btn.setAttribute('title', expanded ? 'Collapse steps' : 'Expand steps');
}

function buildTurnFoldBar(turnKey, count) {
  const btn = document.createElement('button');
  btn.type = 'button';
  btn.className = 'turn-fold-bar';
  btn.dataset.turnFoldKey = turnKey;
  btn.dataset.turnFoldCount = String(count);
  btn.onclick = function() { toggleTurnFold(this); };
  btn.innerHTML = '<span class="turn-fold-label">' + turnFoldLabel(count) + '</span>'
    + '<svg class="turn-fold-chevron" fill="none" stroke="currentColor" viewBox="0 0 24 24">'
    + '<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M19 9l-7 7-7-7"/></svg>';
  setTurnFoldBarExpanded(btn, false);
  return btn;
}

// The `N steps` band: the stable messages between head and conclusion. New
// bands start collapsed; Expand all opens them from the derive. The engine
// drives this with already-rendered elements; the legacy derive drives it
// with its DOM turn.
function installTurnFold(wrap, foldRange, turnKey) {
  if (!foldRange.length) return;

  const content = document.createElement('div');
  content.className = 'turn-fold-content space-y-3 hidden';
  content.dataset.turnFoldKey = turnKey;

  const bar = buildTurnFoldBar(content.dataset.turnFoldKey, foldRange.length);
  const ref = foldRange[0];
  wrap.insertBefore(bar, ref);
  wrap.insertBefore(content, ref);
  foldRange.forEach(el => content.appendChild(el));
}

function installTurnFoldFromTurn(wrap, turn) {
  installTurnFold(wrap, turn.foldRange, turnFoldKey(turn));
}

function turnFoldContent(btn) {
  const content = btn.nextElementSibling;
  if (!content || !content.classList.contains('turn-fold-content')) {
    throw new Error('Turn fold content missing');
  }
  return content;
}

function setTurnFoldExpanded(btn, expanded) {
  turnFoldContent(btn).classList.toggle('hidden', !expanded);
  setTurnFoldBarExpanded(btn, expanded);
}

function setAllTurnFolds(root, expanded) {
  Array.from(root.querySelectorAll('.turn-fold-bar')).forEach(bar => {
    setTurnFoldExpanded(bar, expanded);
  });
}

function toggleTurnFold(btn) {
  const wrap = btn.closest && btn.closest('.turn-wrap');
  const engine = turnEngineForWrap(wrap);
  if (engine) {
    engine.toggleNSteps(btn);
    return;
  }
  setTurnFoldExpanded(btn, turnFoldContent(btn).classList.contains('hidden'));
}

// ---------------------------------------------------------------------------
// Fold row: six derived fields, all read off the turn's own messages
// ---------------------------------------------------------------------------

// Rendered nodes read back with element boundaries as line breaks, so a
// trailing time div can never merge into the first line.
function renderedNodeText(node) {
  if (node.nodeType === TEXT_NODE) return node.textContent;
  return Array.from(node.childNodes).map(renderedNodeText).join('\n');
}

// `.prose-msg[data-raw]` carries the message's own unrendered markdown; a plain
// bubble keeps its text in `.whitespace-pre-wrap`, which is also what leaves the
// voice badge and the time div out of the first line.
function messageSourceText(el) {
  const prose = el.querySelector('.prose-msg');
  if (prose) return prose.dataset.raw != null ? prose.dataset.raw : renderedNodeText(prose);
  const plain = el.querySelector('.whitespace-pre-wrap');
  if (plain) return renderedNodeText(plain);
  return renderedNodeText(el);
}

function turnFirstLine(text) {
  const lines = String(text).split('\n');
  for (const raw of lines) {
    const line = raw.replace(/^\s*(?:#{1,6}|[-*+>]|\d+[.)])\s+/, '')
      .replace(/\[([^\]]*)\]\([^)]*\)/g, '$1')
      .replace(/`/g, '')
      .trim();
    if (line) return line;
  }
  return '';
}

// A worker summary titles itself by the worker it reports on; every other head
// titles itself by its first line.
function turnRowTitleFromText(role, text) {
  if (role === 'worker_summary') {
    const workerId = /`([0-9a-f]{6,})`/.exec(text);
    if (workerId) return workerId[1];
  }
  return turnFirstLine(text);
}

function formatTurnDuration(seconds) {
  if (!seconds) return '';
  const total = Number(seconds);
  if (total < 60) return total + 's';
  return Math.floor(total / 60) + 'm' + String(total % 60).padStart(2, '0') + 's';
}

function formatTurnTime(ts) {
  if (!ts) return '';
  const d = new Date(ts);
  return String(d.getHours()).padStart(2, '0') + ':' + String(d.getMinutes()).padStart(2, '0');
}

function turnRowField(className, text) {
  const span = document.createElement('span');
  span.className = className;
  span.textContent = text;
  return span;
}

// The row built from raw fields rather than rendered nodes — the engine's
// descriptor and the legacy DOM turn produce the same row through this
// single builder.
function buildTurnRowFromSpec(spec) {
  const row = document.createElement('button');
  row.type = 'button';
  row.className = 'turn-row';
  row.setAttribute('title', 'Open this turn');
  row.onclick = function() { setTurnOverride(this.parentNode, true); };

  const label = TURN_TYPE_LABELS[spec.headRole] || 'Turn';
  row.appendChild(turnRowField('turn-row-tag turn-row-tag-' + label.toLowerCase(), label));
  const title = turnRowTitleFromText(spec.headRole, spec.headText);
  row.appendChild(turnRowField('turn-row-title', title));
  row.appendChild(turnRowField('turn-row-conclusion',
      spec.conclusionText != null ? turnFirstLine(spec.conclusionText) : ''));

  const meta = document.createElement('span');
  meta.className = 'turn-row-meta';
  meta.appendChild(turnRowField('turn-row-steps',
      spec.foldCount ? turnFoldLabel(spec.foldCount) : ''));
  meta.appendChild(turnRowField('turn-row-duration',
      formatTurnDuration(spec.thinkingSeconds)));
  const ts = spec.headTs;
  const time = turnRowField('turn-row-time', formatTurnTime(ts));
  if (ts) time.setAttribute('title', formatBubbleTime(ts));
  meta.appendChild(time);
  row.appendChild(meta);

  return row;
}

function buildTurnRow(turn) {
  return buildTurnRowFromSpec({
    headRole: renderedMessageRole(turn.head),
    headText: messageSourceText(turn.head),
    headTs: turn.head.dataset.messageTs || null,
    conclusionText: turn.conclusion ? messageSourceText(turn.conclusion) : null,
    thinkingSeconds: turn.separator.dataset.thinkingSeconds,
    foldCount: turn.foldRange.length,
  });
}

// The fold-back control shares the separator line with clone-to-here, Elon-e,
// Recap and the round rating.
function installTurnCollapseControl(separator) {
  if (separator.querySelector('.turn-collapse')) return;
  const btn = document.createElement('button');
  btn.type = 'button';
  btn.className = 'turn-collapse';
  btn.setAttribute('title', 'Collapse this turn into one row');
  btn.textContent = 'Collapse';
  btn.onclick = function() { setTurnOverride(this.closest('.turn-wrap'), false); };
  separator.insertBefore(btn, separator.lastElementChild);
}

// ---------------------------------------------------------------------------
// Wrapping, open/folded state, derive
// ---------------------------------------------------------------------------
function wrapTurn(root, turn) {
  const wrap = document.createElement('div');
  wrap.className = 'turn-wrap';
  root.insertBefore(wrap, turn.nodes[0]);
  wrap.appendChild(buildTurnRow(turn));
  turn.nodes.forEach(node => wrap.appendChild(node));
  installTurnFoldFromTurn(wrap, turn);
  installTurnCollapseControl(turn.separator);
  return wrap;
}

function turnWrappers(root) {
  return Array.from(root.children).filter(el => el.classList.contains('turn-wrap'));
}

function setTurnOpen(wrap, open) {
  wrap.dataset.turnOpen = String(open);
}

// open(turn) = override ?? (pageDepth === 'outline' ? isLastWrapper : true)
function turnIsOpen(wrap, isLast) {
  const override = wrap.dataset.turnOverride;
  if (override) return override === 'open';
  return pageDepth === 'outline' ? isLast : true;
}

// Engine-hosted wraps route open/fold through the engine's registry; legacy
// wraps keep the dataset implementation.
function turnEngineForWrap(wrap) {
  if (!wrap || !wrap.dataset || !wrap.dataset.turnKey || !Chat.TurnEngine) return null;
  return Chat.TurnEngine.activeFor(document.getElementById('messages'));
}

function setTurnOverride(wrap, open) {
  const engine = turnEngineForWrap(wrap);
  if (engine) {
    engine.setOverride(wrap.dataset.turnKey, open);
    return;
  }
  wrap.dataset.turnOverride = open ? 'open' : 'folded';
  setTurnOpen(wrap, open);
}

// The derive: wrap every newly finished turn, then set every wrapper's state to
// open(turn). Runs at session load, at pagination, and when a separator lands.
function applyTurnOutline(root) {
  if (!root) {
    updatePageDepthControl();
    return;
  }

  // Hold the reader's place: folding a turn above the viewport changes the
  // height above it, so keep the distance to the bottom (or stay pinned).
  const wasAtBottom = shouldAutoScroll(root);
  const bottomOffset = root.scrollHeight - root.scrollTop;

  collectTurns(root).forEach(turn => wrapTurn(root, turn));

  const wrappers = turnWrappers(root);
  wrappers.forEach((wrap, i) => setTurnOpen(wrap, turnIsOpen(wrap, i === wrappers.length - 1)));
  if (pageDepth === 'expanded') setAllTurnFolds(root, true);

  root.scrollTop = wasAtBottom ? root.scrollHeight : root.scrollHeight - bottomOffset;
  updatePageDepthControl();
}

function updatePageDepthControl() {
  const control = document.getElementById('page-depth-control');
  if (!control) return;
  Array.from(control.children).forEach(btn => {
    btn.setAttribute('aria-pressed', String(btn.dataset.pageDepth === pageDepth));
  });
}

function setPageDepth(depth) {
  pageDepth = depth;
  Chat.pageDepth = depth;
  const root = document.getElementById('messages');
  const engine = Chat.TurnEngine && Chat.TurnEngine.activeFor(root);
  if (engine) {
    engine.setDepth(depth);
    updatePageDepthControl();
    return;
  }
  // A depth click is the one place that closes a reader-expanded `N steps`
  // bar; Expand all reopens every bar from the derive.
  if (root && depth !== 'expanded') setAllTurnFolds(root, false);
  applyTurnOutline(root);
}

function renderMessagesIntoContainer(container, messages, sessionId) {
  const streamEl = document.getElementById('streaming-msg');
  const streamHtml = streamEl ? streamEl.outerHTML : '';
  const list = messages || [];
  const parts = list.map(msg => globalThis.renderMessage(msg, sessionId));
  container.innerHTML = parts.join('') + streamHtml;
  globalThis.postProcessRenderedMessages(container);
  globalThis.applyTurnOutline(container);
}

function postProcessRenderedMessages(root) {
  root.querySelectorAll('.prose-msg').forEach(renderChatMath);
  embedLinkedHtmlArtifacts(root);
  root.querySelectorAll('.bubble-time[data-ts]').forEach(el => {
    el.textContent = formatBubbleTime(el.dataset.ts);
  });
  root.querySelectorAll('.rounded-full[title]').forEach(el => {
    const t = el.getAttribute('title');
    if (t && t.includes('T')) el.title = formatBubbleTime(t);
  });
}

function renderMessagesToDetachedContainer(messages, sessionId) {
  const tempDiv = document.createElement('div');
  tempDiv.innerHTML = (messages || []).map(msg => globalThis.renderMessage(msg, sessionId)).join('');
  globalThis.postProcessRenderedMessages(tempDiv);
  return tempDiv;
}

function displayMetadataValue(value, emptyText) {
  if (value === null || value === undefined || value === '') return emptyText;
  return String(value);
}

function renderMetadataRow(label, value) {
  return '<div class="grid grid-cols-[9rem_minmax(0,1fr)] gap-3">'
    + '<span class="text-slate-500">' + escapeHtml(label) + '</span>'
    + '<span class="min-w-0 break-words">' + escapeHtml(value) + '</span>'
    + '</div>';
}

function delegateBackendModel(invocation, msg) {
  var backend = displayMetadataValue((invocation && invocation.backend) || msg.backend || msg.resolved_backend, '(none)');
  var model = displayMetadataValue(msg.model || msg.resolved_model, '');
  return model ? backend + ' / ' + model : backend;
}

function renderDelegateMetadata(msg) {
  var invocation = msg.delegate_invocation;
  var rows = [];
  if (invocation) {
    var taskType = displayMetadataValue(invocation.task_type || msg.task_type, '(unknown)');
    var repoEmpty = taskType === 'verify' ? '(none)' : '(unknown)';
    var baseEmpty = taskType === 'verify' ? '(none)' : '(unknown)';
    rows = [
      ['task type', taskType],
      ['thread id', displayMetadataValue(msg.thread_id, '(unknown)')],
      ['repo', displayMetadataValue(invocation.repo_path, repoEmpty)],
      ['base', displayMetadataValue(invocation.base_branch, baseEmpty)],
      ['task spec file', displayMetadataValue(invocation.task_spec_file, '(unknown)')],
      ['reviewer context file', displayMetadataValue(invocation.reviewer_context_file, '(none)')],
      ['backend/model', delegateBackendModel(invocation, msg)],
      ['keep worktree', invocation.keep_worktree ? 'true' : 'false'],
      ['full details', 'Workers panel'],
    ];
  } else {
    rows = [
      ['thread id', displayMetadataValue(msg.thread_id, '(unknown)')],
    ];
    var backendModel = delegateBackendModel(null, msg);
    if (backendModel !== '(none)') rows.push(['backend/model', backendModel]);
    rows.push(['full details', 'Workers panel']);
  }
  return '<div class="font-mono text-xs leading-5 bg-slate-950/35 border border-amber-700/20 rounded-lg px-3 py-2 space-y-1">'
    + rows.map(row => renderMetadataRow(row[0], row[1])).join('')
    + '</div>';
}

function renderMessage(msg, sessionId) {
  function timeDiv(colorClass) {
    if (!msg.timestamp) return "";
    var cls = colorClass || "text-slate-400/60";
    return "<div class=\"text-[10px] " + cls + " mt-1\">" + formatBubbleTime(msg.timestamp) + "</div>";
  }
  function mdDiv(text) {
    var raw = escapeHtml(text || "").replace(/"/g, "&quot;");
    return "<div class=\"prose-msg\" data-raw=\"" + raw + "\">" + marked.parse(fixNestedFences(text || "")) + "</div>";
  }

  if (msg.role === "user") {
    return "<div class=\"flex justify-end\"" + messageIdentityAttrs(msg) + ">"
      + renderUserMessageBubble(msg.content, msg.is_voice, msg.timestamp, msg.uploaded_files) + "</div>";
  }
  if (msg.role === "assistant") {
    var content = msg.content || "";
    var contentHtml = content.trimStart().startsWith("<tool_call>")
      ? renderRawBackendOutput(content)
      : mdDiv(content);
    var toolsHtml = renderToolActivity(msg.tools);
    var thinkingHtml = "";
    if (msg.thinking) {
      var thinkId = 'think-' + (msg.id || Math.random().toString(36).slice(2));
      thinkingHtml = "<button onclick=\"const el=document.getElementById(\x27" + thinkId + "\x27);el.style.display=el.style.display===\x27none\x27?\x27block\x27:\x27none\x27\" class=\"text-xs text-slate-500 hover:text-slate-400 italic mb-1\">Thinking…</button>"
        + "<div id=\"" + thinkId + "\" style=\"display:none\" class=\"text-xs text-slate-500 whitespace-pre-wrap mb-2\">" + escapeHtml(String(msg.thinking)) + "</div>";
    }
    return "<div class=\"flex justify-start\"" + messageIdentityAttrs(msg) + "><div class=\"max-w-[90%] overflow-hidden bg-slate-700 rounded-2xl rounded-bl-md px-4 py-2.5 text-sm\">"
      + thinkingHtml + contentHtml + toolsHtml + timeDiv() + "</div></div>";
  }
  if (msg.role === "system") {
    var titleAttr = msg.timestamp ? " title=\"" + formatBubbleTime(msg.timestamp) + "\"" : "";
    return "<div class=\"flex justify-center\"" + messageIdentityAttrs(msg) + "><div class=\"bg-slate-700/50 text-slate-400 text-xs px-3 py-1.5 rounded-full max-w-[85%] overflow-hidden truncate\"" + titleAttr + ">"
      + escapeHtml(msg.content) + "</div></div>";
  }
  if (msg.role === "task_delegated") {
    return "<div class=\"flex justify-start\"" + messageIdentityAttrs(msg) + "><div class=\"max-w-[90%] overflow-hidden bg-amber-900/30 border border-amber-700/30 rounded-2xl rounded-bl-md px-4 py-2.5 text-sm text-slate-300\">"
      + "<div class=\"flex items-center gap-2 text-amber-400 text-xs font-semibold mb-2\">"
      + "<svg class=\"w-3.5 h-3.5\" fill=\"none\" stroke=\"currentColor\" viewBox=\"0 0 24 24\"><path stroke-linecap=\"round\" stroke-linejoin=\"round\" stroke-width=\"2\" d=\"M13 5l7 7-7 7M5 5l7 7-7 7\"/></svg>"
      + "Delegated</div>"
      + renderDelegateMetadata(msg) + timeDiv() + "</div></div>";
  }
  if (msg.role === "worker_summary") {
    var originFooter = "";
    if (msg.origin_session_id && msg.thread_id && msg.origin_session_id !== sessionId) {
      originFooter = "<div class=\"mt-2 pt-2 border-t border-emerald-700/30 text-xs text-emerald-400/50\">"
        + "Ran in session <a href=\"/?session=" + encodeURIComponent(msg.origin_session_id)
        + "\" class=\"text-emerald-400/50 underline\">"
        + escapeHtml(msg.origin_session_id) + "</a> &middot; thread " + escapeHtml(msg.thread_id)
        + "</div>";
    }
    return "<div class=\"flex justify-start\"" + messageIdentityAttrs(msg) + "><div class=\"max-w-[90%] overflow-hidden bg-emerald-900/40 border border-emerald-700/30 rounded-2xl rounded-bl-md px-4 py-2.5 text-sm text-slate-300\">"
      + mdDiv(msg.content) + timeDiv("text-emerald-400/50") + originFooter + "</div></div>";
  }
  if (msg.role === "plan") {
    return "<div class=\"flex justify-start\"" + messageIdentityAttrs(msg) + "><div class=\"max-w-[90%] overflow-hidden bg-slate-800 border border-blue-500/30 rounded-2xl px-4 py-3 text-sm\">"
      + "<div class=\"flex items-center gap-2 text-blue-400 text-xs font-semibold mb-2\">"
      + "<svg class=\"w-3.5 h-3.5\" fill=\"none\" stroke=\"currentColor\" viewBox=\"0 0 24 24\"><path stroke-linecap=\"round\" stroke-linejoin=\"round\" stroke-width=\"2\" d=\"M9 5H7a2 2 0 00-2 2v12a2 2 0 002 2h10a2 2 0 002-2V7a2 2 0 00-2-2h-2M9 5a2 2 0 002 2h2a2 2 0 002-2M9 5a2 2 0 012-2h2a2 2 0 012 2m-6 9l2 2 4-4\"/></svg>"
      + "Plan</div>"
      + mdDiv(msg.content) + timeDiv() + "</div></div>";
  }
  if (msg.role === "clone_start") {
    return "<div class=\"flex items-center gap-3 py-3 px-4\"" + messageIdentityAttrs(msg) + ">"
      + "<div class=\"flex-1 border-t border-purple-500/40\"></div>"
      + "<div class=\"flex items-center gap-2 text-purple-400 text-xs\">"
      + "<svg class=\"w-3.5 h-3.5\" fill=\"none\" stroke=\"currentColor\" viewBox=\"0 0 24 24\"><path stroke-linecap=\"round\" stroke-linejoin=\"round\" stroke-width=\"2\" d=\"M6 3v12M6 9h6m0 0V3m0 6v6m0 0h6\"/></svg>"
      + "<span>Cloned from <a href=\"/?session=" + encodeURIComponent(msg.parent_session_id || "")
      + "\" class=\"text-purple-300 hover:text-purple-200 underline\">"
      + escapeHtml(msg.content || "") + "</a></span></div>"
      + "<div class=\"flex-1 border-t border-purple-500/40\"></div></div>";
  }
  if (msg.role === "scheduled_trigger") {
    return "<div class=\"flex justify-start\"" + messageIdentityAttrs(msg) + "><div class=\"w-full bg-slate-700/40 border border-slate-600/30 rounded-lg px-4 py-2 text-xs text-slate-400 whitespace-pre-wrap break-words\">"
      + escapeHtml(msg.content) + timeDiv() + "</div></div>";
  }
  if (msg.role === "agent_message") {
    return "<div class=\"flex justify-start\"" + messageIdentityAttrs(msg) + "><div class=\"w-full bg-slate-700/40 border border-slate-600/30 rounded-lg px-4 py-2 text-xs text-slate-300\">"
      + "<div class=\"flex items-center gap-2 mb-1 text-indigo-300 font-semibold\">"
      + "<span class=\"px-1.5 py-0.5 rounded bg-indigo-900 text-[10px] tracking-wide\">AGENT</span>"
      + "<span class=\"truncate\">" + escapeHtml(msg.from_session_name || "") + "</span>"
      + "</div>"
      + "<div class=\"whitespace-pre-wrap break-words\">" + escapeHtml(msg.content) + "</div>"
      + timeDiv() + "</div></div>";
  }
  if (msg.role === "separator") {
    var timeStr = msg.thinking_seconds != null ? " &middot; " + msg.thinking_seconds + "s" : "";
    var secondsAttr = msg.thinking_seconds != null
      ? " data-thinking-seconds=\"" + escapeChatAttr(msg.thinking_seconds) + "\"" : "";
    var buttons = "";
    if (sessionId) {
      if (msg.event_index != null) {
        buttons = "<button onclick=\"forkSession(\x27" + sessionId + "\x27, " + msg.event_index + ")\""
          + " class=\"p-0.5 text-slate-500 hover:text-green-400\" title=\"Clone to here\">"
          + "<svg class=\"w-3.5 h-3.5\" fill=\"none\" stroke=\"currentColor\" viewBox=\"0 0 24 24\"><path stroke-linecap=\"round\" stroke-linejoin=\"round\" stroke-width=\"2\" d=\"M6 3v12M6 9h6m0 0V3m0 6v6m0 0h6\"/></svg>"
          + "</button>"
          + "<button onclick=\"eloneSession(\x27" + sessionId + "\x27, " + msg.event_index + ")\""
          + " class=\"p-0.5 text-slate-500 hover:text-yellow-400\" title=\"Elon-e: retry with a fresh perspective\">"
          + "<svg class=\"w-3.5 h-3.5\" fill=\"none\" stroke=\"currentColor\" viewBox=\"0 0 24 24\"><path stroke-linecap=\"round\" stroke-linejoin=\"round\" stroke-width=\"2\" d=\"M13 10V3L4 14h7v7l9-11h-7z\"/></svg>"
          + "</button>"
          + "<button onclick=\"toggleRecapPanel(this, \x27" + sessionId + "\x27, " + msg.event_index + ")\""
          + " class=\"recap-toggle p-0.5 text-slate-500 hover:text-sky-400\" title=\"Recap: what this section covered\">"
          + "<svg class=\"w-3.5 h-3.5\" fill=\"none\" stroke=\"currentColor\" viewBox=\"0 0 24 24\"><path stroke-linecap=\"round\" stroke-linejoin=\"round\" stroke-width=\"2\" d=\"M3.75 6h16.5M3.75 12h16.5M3.75 18h10.5\"/></svg>"
          + "</button>";
      }
      if (msg.id != null) {
        buttons += renderRoundRatingButtons(sessionId, msg.id);
      }
    }
    return "<div class=\"flex items-center gap-3 py-2 px-4 separator-line group/sep\""
      + messageIdentityAttrs(msg) + secondsAttr + ">"
      + "<div class=\"flex-1 border-t border-slate-600/40\"></div>"
      + "<span class=\"text-xs text-slate-500 whitespace-nowrap\">response complete" + timeStr + "</span>"
      + buttons
      + "<div class=\"flex-1 border-t border-slate-600/40\"></div></div>";
  }
  return "";
}

// A live message appends to the engine's store when the container is hosted;
// the engine renders it synchronously (like the legacy path), materializes
// the projection and pins the bottom when the reader is there.
function ingestLiveMessage(msg, sessionId, forceScroll) {
  var container = document.getElementById("messages");
  var engine = Chat.TurnEngine && Chat.TurnEngine.activeFor(container);
  if (engine) {
    engine.appendMessage(msg, forceScroll);
    return true;
  }
  return false;
}

function _appendRenderedMessage(html, forceScroll) {
  var container = document.getElementById("messages");
  var wasAtBottom = shouldAutoScroll(container);
  var wrapper = document.createElement("div");
  wrapper.innerHTML = html;
  globalThis.postProcessRenderedMessages(wrapper);
  var el = wrapper.firstElementChild || wrapper;
  var streamEl = document.getElementById("streaming-msg");
  container.insertBefore(el, streamEl);
  // A landed separator finishes a turn: derive so it becomes a wrapper and the
  // turn before it folds.
  if (renderedMessageRole(el) === 'separator') globalThis.applyTurnOutline(container);
  if (forceScroll || wasAtBottom) {
    container.scrollTop = container.scrollHeight;
  } else {
    showScrollToBottom();
  }
}

function appendMessageObject(msg, sessionId) {
  if (ingestLiveMessage(msg, sessionId, msg.role === "user")) return;
  _appendRenderedMessage(globalThis.renderMessage(msg, sessionId || SESSION_ID), msg.role === "user");
}

function appendMessage(role, content, isVoice, timestamp, uploadedFiles) {
  var msg = {
    role: role,
    content: content || "",
    is_voice: !!isVoice,
    timestamp: timestamp || null,
    uploaded_files: uploadedFiles || null,
  };
  if (!ingestLiveMessage(msg, SESSION_ID, role === "user")) {
    _appendRenderedMessage(globalThis.renderMessage(msg, SESSION_ID), role === "user");
  }
  return msg;
}

function appendSeparator(seconds, eventIndex) {
  var msg = {
    role: "separator",
    thinking_seconds: seconds,
    event_index: eventIndex,
  };
  if (!ingestLiveMessage(msg, SESSION_ID, false)) {
    _appendRenderedMessage(globalThis.renderMessage(msg, SESSION_ID));
  }
}

function appendCloneBanner(parentName, parentSessionId) {
  var msg = {
    role: "clone_start",
    content: parentName,
    parent_session_id: parentSessionId,
  };
  if (!ingestLiveMessage(msg, SESSION_ID, false)) {
    _appendRenderedMessage(globalThis.renderMessage(msg, SESSION_ID));
  }
}

Chat.renderMessage = renderMessage;
Chat.renderMessagesIntoContainer = renderMessagesIntoContainer;
Chat.postProcessRenderedMessages = postProcessRenderedMessages;
Chat.renderMessagesToDetachedContainer = renderMessagesToDetachedContainer;
Chat.appendMessageObject = appendMessageObject;
Chat.appendMessage = appendMessage;
Chat.appendSeparator = appendSeparator;
Chat.appendCloneBanner = appendCloneBanner;
Chat.applyTurnOutline = applyTurnOutline;
Chat.setPageDepth = setPageDepth;
Chat.toggleTurnFold = toggleTurnFold;
// Turn primitives the window engine builds wraps from. Legacy callers go
// through wrapTurn / applyTurnOutline above and never touch these directly.
Chat.buildTurnRowFromSpec = buildTurnRowFromSpec;
Chat.installTurnFold = installTurnFold;
Chat.installTurnCollapseControl = installTurnCollapseControl;
Chat.setTurnFoldExpanded = setTurnFoldExpanded;
Chat.expose([
  'renderMessage',
  'renderMessagesIntoContainer',
  'postProcessRenderedMessages',
  'renderMessagesToDetachedContainer',
  'appendMessageObject',
  'appendMessage',
  'appendSeparator',
  'appendCloneBanner',
  'applyTurnOutline',
  'setPageDepth',
  'toggleTurnFold',
]);

})();
