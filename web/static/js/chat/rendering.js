
(function() {
  const Chat = globalThis.Chat;
  const formatBubbleTime = Chat.formatBubbleTime;
  const escapeChatAttr = Chat.escapeChatAttr;
  const messageIdentityAttrs = Chat.messageIdentityAttrs;
  const renderRoundRatingButtons = Chat.renderRoundRatingButtons;
  const embedLinkedHtmlArtifacts = Chat.embedLinkedHtmlArtifacts;

let compactMode = 'compact';

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

function resetTurnFolds(root) {
  Array.from(root.querySelectorAll('.turn-fold-content')).forEach(content => {
    const parent = content.parentNode;
    while (content.firstElementChild) {
      parent.insertBefore(content.firstElementChild, content);
    }
    content.remove();
  });
  Array.from(root.querySelectorAll('.turn-fold-bar')).forEach(bar => bar.remove());
}

function collectCompletedTurns(root) {
  const turns = [];
  let turnStart = null;

  Array.from(root.children).forEach(el => {
    if (!isStableRenderedMessage(el)) return;
    const role = renderedMessageRole(el);
    if (role === 'user') {
      turnStart = el;
      return;
    }
    if (role === 'separator') {
      if (turnStart) turns.push({start: turnStart, separator: el});
      turnStart = null;
    }
  });

  return turns;
}

function findTurnConclusion(turn) {
  let el = turn.separator.previousElementSibling;
  while (el && el !== turn.start) {
    if (isStableRenderedMessage(el) && renderedMessageRole(el) === 'assistant') return el;
    el = el.previousElementSibling;
  }
  return null;
}

function collectIntermediateTurnMessages(turn, conclusion) {
  const messages = [];
  let el = turn.start.nextElementSibling;
  while (el && el !== conclusion) {
    if (isStableRenderedMessage(el)) messages.push(el);
    el = el.nextElementSibling;
  }
  return messages;
}

function turnFoldKey(turn, conclusion) {
  return [
    renderedMessageId(turn.start),
    renderedMessageId(conclusion),
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

function buildTurnFoldBar(turnKey, count, expanded) {
  const btn = document.createElement('button');
  btn.type = 'button';
  btn.className = 'turn-fold-bar';
  btn.dataset.turnFoldKey = turnKey;
  btn.dataset.turnFoldCount = String(count);
  btn.onclick = function() { toggleTurnFold(this); };
  btn.innerHTML = '<span class="turn-fold-label">' + turnFoldLabel(count) + '</span>'
    + '<svg class="turn-fold-chevron" fill="none" stroke="currentColor" viewBox="0 0 24 24">'
    + '<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M19 9l-7 7-7-7"/></svg>';
  setTurnFoldBarExpanded(btn, expanded);
  return btn;
}

function installTurnFold(root, turn, collapsed) {
  const conclusion = findTurnConclusion(turn);
  if (!conclusion) return;

  const messages = collectIntermediateTurnMessages(turn, conclusion);
  if (!messages.length) return;

  const content = document.createElement('div');
  content.className = 'turn-fold-content space-y-3';
  content.dataset.turnFoldKey = turnFoldKey(turn, conclusion);
  content.classList.toggle('hidden', collapsed);

  const bar = buildTurnFoldBar(content.dataset.turnFoldKey, messages.length, !collapsed);
  const ref = messages[0];
  root.insertBefore(bar, ref);
  root.insertBefore(content, ref);
  messages.forEach(el => content.appendChild(el));
}

function applyCompactMode(root) {
  if (!root) {
    updateCompactModeButton();
    return;
  }

  resetTurnFolds(root);
  const turns = collectCompletedTurns(root);
  turns.forEach(turn => {
    installTurnFold(root, turn, shouldCollapseTurn());
  });
  updateCompactModeButton();
}

function shouldCollapseTurn() {
  return compactMode !== 'expanded';
}

function toggleTurnFold(btn) {
  const content = btn.nextElementSibling;
  if (!content || !content.classList.contains('turn-fold-content')) {
    throw new Error('Turn fold content missing');
  }
  const collapsed = content.classList.toggle('hidden');
  setTurnFoldBarExpanded(btn, !collapsed);
}

function updateCompactModeButton() {
  const btn = document.getElementById('compact-mode-toggle');
  if (!btn) return;
  const expanded = compactMode === 'expanded';
  btn.textContent = expanded ? 'Compact' : 'Expand all';
  btn.setAttribute('title', expanded ? 'Collapse completed turns' : 'Expand collapsed turns');
  btn.setAttribute('aria-pressed', String(!expanded));
}

function toggleCompactMode() {
  compactMode = compactMode === 'expanded' ? 'compact' : 'expanded';
  applyCompactMode(document.getElementById('messages'));
}

function renderMessagesIntoContainer(container, messages, sessionId) {
  const streamEl = document.getElementById('streaming-msg');
  const streamHtml = streamEl ? streamEl.outerHTML : '';
  const list = messages || [];
  const parts = list.map(msg => globalThis.renderMessage(msg, sessionId));
  container.innerHTML = parts.join('') + streamHtml;
  globalThis.postProcessRenderedMessages(container);
  globalThis.applyCompactMode(container);
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
    return "<div class=\"flex justify-start\"" + messageIdentityAttrs(msg) + "><div class=\"max-w-[90%] overflow-hidden bg-emerald-900/40 border border-emerald-700/30 rounded-2xl rounded-bl-md px-4 py-2.5 text-sm text-slate-300\">"
      + mdDiv(msg.content) + timeDiv("text-emerald-400/50") + "</div></div>";
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
          + " class=\"p-0.5 text-slate-500 hover:text-sky-400\" title=\"Recap: what this section covered\">"
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

function _appendRenderedMessage(html, forceScroll) {
  var container = document.getElementById("messages");
  var wasAtBottom = shouldAutoScroll(container);
  var wrapper = document.createElement("div");
  wrapper.innerHTML = html;
  globalThis.postProcessRenderedMessages(wrapper);
  var el = wrapper.firstElementChild || wrapper;
  var streamEl = document.getElementById("streaming-msg");
  container.insertBefore(el, streamEl);
  if (forceScroll || wasAtBottom) {
    container.scrollTop = container.scrollHeight;
  } else {
    showScrollToBottom();
  }
}

function appendMessageObject(msg, sessionId) {
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
  _appendRenderedMessage(globalThis.renderMessage(msg, SESSION_ID), role === "user");
  return msg;
}

function appendSeparator(seconds, eventIndex) {
  var msg = {
    role: "separator",
    thinking_seconds: seconds,
    event_index: eventIndex,
  };
  _appendRenderedMessage(globalThis.renderMessage(msg, SESSION_ID));
}

function appendCloneBanner(parentName, parentSessionId) {
  var msg = {
    role: "clone_start",
    content: parentName,
    parent_session_id: parentSessionId,
  };
  _appendRenderedMessage(globalThis.renderMessage(msg, SESSION_ID));
}

Chat.renderMessage = renderMessage;
Chat.renderMessagesIntoContainer = renderMessagesIntoContainer;
Chat.postProcessRenderedMessages = postProcessRenderedMessages;
Chat.renderMessagesToDetachedContainer = renderMessagesToDetachedContainer;
Chat.appendMessageObject = appendMessageObject;
Chat.appendMessage = appendMessage;
Chat.appendSeparator = appendSeparator;
Chat.appendCloneBanner = appendCloneBanner;
Chat.applyCompactMode = applyCompactMode;
Chat.toggleCompactMode = toggleCompactMode;
Chat.toggleTurnFold = toggleTurnFold;
Chat.expose([
  'renderMessage',
  'renderMessagesIntoContainer',
  'postProcessRenderedMessages',
  'renderMessagesToDetachedContainer',
  'appendMessageObject',
  'appendMessage',
  'appendSeparator',
  'appendCloneBanner',
  'applyCompactMode',
  'toggleCompactMode',
  'toggleTurnFold',
]);

})();
