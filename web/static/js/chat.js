// ---------------------------------------------------------------------------
// Auto-scroll helper — returns true only when user is near the bottom
// ---------------------------------------------------------------------------
function shouldAutoScroll(container, threshold = 150) {
  return container.scrollHeight - container.scrollTop - container.clientHeight < threshold;
}

// ---------------------------------------------------------------------------
// Send message
// ---------------------------------------------------------------------------
function bumpCurrentSessionToTop() {
  const nav = document.getElementById('session-list');
  const el = document.getElementById('session-' + SESSION_ID);
  if (!nav || !el) return;

  const groupItems = el.closest('.session-group-items');
  const parent = groupItems || nav;
  if (parent.firstElementChild !== el) {
    parent.insertBefore(el, parent.firstElementChild);
  }

  const timeEl = el.querySelector('.session-time');
  if (timeEl) {
    const now = new Date().toISOString();
    timeEl.dataset.time = now;
    timeEl.textContent = relativeTime(now);
  }
}

function formatBubbleTime(isoStr) {
  if (!isoStr) return '';
  const d = new Date(isoStr);
  return d.toLocaleString(undefined, {
    month: 'short', day: 'numeric',
    hour: 'numeric', minute: '2-digit', second: '2-digit', hour12: true, timeZoneName: 'short'
  });
}

function getUploadedFileDisplayName(file) {
  if (file && file.filename) return file.filename;
  const path = file && file.path ? String(file.path) : '';
  const normalized = path.replaceAll('\\', '/').replace(/\/+$/, '');
  const parts = normalized.split('/');
  return parts[parts.length - 1] || path;
}

function normalizeUploadedFiles(uploadedFiles) {
  if (!Array.isArray(uploadedFiles) || !uploadedFiles.length) return [];
  return uploadedFiles.map((file) => {
    if (typeof file === 'string') {
      return {filename: getUploadedFileDisplayName({path: file}), path: file};
    }
    return {
      filename: getUploadedFileDisplayName(file),
      path: file.path || '',
      size: file.size,
    };
  });
}

function splitLegacyUploadedFiles(content) {
  const text = typeof content === 'string' ? content : '';
  const marker = '\n\n[Attached files]\n';
  const markerIndex = text.lastIndexOf(marker);
  if (markerIndex === -1) return {content: text, uploadedFiles: []};

  const attachmentBlock = text.slice(markerIndex + marker.length);
  const lines = attachmentBlock.split('\n').filter((line) => line.length > 0);
  if (!lines.length || lines.some((line) => !line.startsWith('- ') || !line.slice(2).trim())) {
    return {content: text, uploadedFiles: []};
  }

  return {
    content: text.slice(0, markerIndex),
    uploadedFiles: lines.map((line) => {
      const path = line.slice(2).trim();
      return {filename: getUploadedFileDisplayName({path}), path};
    }),
  };
}

function normalizeUserMessage(content, uploadedFiles) {
  const normalizedFiles = normalizeUploadedFiles(uploadedFiles);
  if (normalizedFiles.length) {
    return {content: typeof content === 'string' ? content : '', uploadedFiles: normalizedFiles};
  }
  return splitLegacyUploadedFiles(content);
}

function renderUserAttachments(uploadedFiles, withTopMargin) {
  const files = normalizeUploadedFiles(uploadedFiles);
  if (!files.length) return '';
  return `<div class="message-attachments${withTopMargin ? ' mt-2' : ''}">`
    + files.map((file) => {
      const title = file.path ? ` title="${escapeHtml(file.path)}"` : '';
      return `<div class="message-attachment"${title}>`
        + '<svg class="w-3.5 h-3.5 flex-shrink-0" fill="none" stroke="currentColor" viewBox="0 0 24 24">'
        + '<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M15.172 7l-6.586 6.586a2 2 0 102.828 2.828l6.414-6.586a4 4 0 00-5.656-5.656l-6.415 6.585a6 6 0 108.486 8.486L20.5 13"/></svg>'
        + `<span>${escapeHtml(file.filename)}</span></div>`;
    }).join('')
    + '</div>';
}

function renderUserMessageBubble(content, isVoice, timestamp, uploadedFiles) {
  const normalized = normalizeUserMessage(content, uploadedFiles);
  const timeHtml = timestamp ? '<div class="text-[10px] text-slate-400/60 mt-1">' + formatBubbleTime(timestamp) + '</div>' : '';
  const textHtml = normalized.content
    ? `<div class="whitespace-pre-wrap">${escapeHtml(normalized.content)}</div>`
    : '';
  const attachmentsHtml = renderUserAttachments(normalized.uploadedFiles, Boolean(textHtml));
  return `<div class="max-w-[75%] overflow-hidden bg-blue-600 rounded-2xl rounded-br-md px-4 py-2.5 text-sm">`
    + (isVoice ? '<span class="text-xs text-blue-200 block mb-1">&#127908; Voice</span>' : '')
    + textHtml
    + attachmentsHtml
    + timeHtml
    + '</div>';
}

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

// ---------------------------------------------------------------------------
// HTML artifact rendering — inline sandboxed iframes for Write tool calls
// targeting artifacts/*.html.
// ---------------------------------------------------------------------------
function isHtmlArtifactTool(tool) {
  if (!tool || tool.name !== 'Write') return false;
  var input = tool.input || {};
  var fp = input.file_path || '';
  if (!fp || !tool.output) return false;
  return /(^|\/)artifacts\/[^/]+\.html$/.test(fp);
}

function basename(path) {
  var parts = String(path || '').split('/');
  return parts[parts.length - 1];
}

function resolveArtifactAbsolutePath(filePath) {
  if (filePath.charAt(0) === '/') return filePath;
  // TODO: read user home from a backend-injected global instead of hardcoding.
  var home = '/data/home/user';
  return home + '/.charliebot/sessions/' + SESSION_ID + '/' + filePath;
}

function escapeForSrcdoc(html) {
  return String(html || '').replace(/&/g, '&amp;').replace(/"/g, '&quot;');
}

function injectResizeScript(html, frameId) {
  var script = '<script>(function(){'
    + 'var frameId=' + JSON.stringify(frameId) + ';'
    + 'function send(){parent.postMessage({type:"html-artifact-height",id:frameId,height:document.documentElement.scrollHeight},"*");}'
    + 'new ResizeObserver(send).observe(document.documentElement);'
    + 'window.addEventListener("load",send);'
    + 'send();'
    + '})();<\/script>';
  var src = String(html || '');
  var idx = src.lastIndexOf('</body>');
  if (idx === -1) return src + script;
  return src.slice(0, idx) + script + src.slice(idx);
}

function renderHtmlArtifact(tool) {
  var filePath = (tool.input && tool.input.file_path) || '';
  var rawHtml = (tool.input && tool.input.content) || '';
  var frameId = 'hf-' + Math.random().toString(36).slice(2);
  var withScript = injectResizeScript(rawHtml, frameId);
  var srcdoc = escapeForSrcdoc(withScript);
  var absPath = resolveArtifactAbsolutePath(filePath);
  var openUrl = '/files' + absPath;
  var sourceHighlighted = hljs.highlight(rawHtml, {language: 'xml'}).value;
  var iframeStyle = 'width:100%;min-height:60px;max-height:80vh;'
    + 'border:1px solid rgba(148,163,184,0.2);border-radius:0 0 0.5rem 0.5rem;'
    + 'background:white;display:block;';
  return '<div class="html-artifact">'
    + '<div class="html-artifact-toolbar">'
    + '<span class="filename">' + escapeHtml(basename(filePath)) + '</span>'
    + '<a href="' + escapeHtml(openUrl) + '" target="_blank" rel="noopener noreferrer">Open in tab</a>'
    + '<button type="button" onclick="toggleHtmlArtifactSource(this)">View source</button>'
    + '</div>'
    + '<iframe class="html-artifact-frame" data-frame-id="' + frameId + '"'
    + ' sandbox="allow-scripts allow-popups allow-popups-to-escape-sandbox"'
    + ' srcdoc="' + srcdoc + '"'
    + ' style="' + iframeStyle + '"></iframe>'
    + '<div class="html-artifact-source"><pre><code class="hljs language-html">'
    + sourceHighlighted + '</code></pre></div>'
    + '</div>';
}

function toggleHtmlArtifactSource(btn) {
  var card = btn.closest('.html-artifact');
  if (!card) return;
  var frame = card.querySelector('.html-artifact-frame');
  var source = card.querySelector('.html-artifact-source');
  if (!frame || !source) return;
  var showingSource = source.style.display === 'block';
  if (showingSource) {
    source.style.display = 'none';
    frame.style.display = 'block';
    btn.textContent = 'View source';
  } else {
    frame.style.display = 'none';
    source.style.display = 'block';
    btn.textContent = 'View rendered';
  }
}

function renderHtmlArtifacts(tools) {
  if (!Array.isArray(tools) || !tools.length) return '';
  var out = '';
  for (var i = 0; i < tools.length; i++) {
    if (isHtmlArtifactTool(tools[i])) {
      out += renderHtmlArtifact(tools[i]);
    }
  }
  return out;
}

function installHtmlArtifactListener() {
  if (window.__htmlArtifactListenerInstalled) return;
  window.__htmlArtifactListenerInstalled = true;
  window.addEventListener('message', function(event) {
    var data = event.data;
    if (!data || data.type !== 'html-artifact-height' || !data.id) return;
    var sel = '.html-artifact-frame[data-frame-id="' + CSS.escape(data.id) + '"]';
    var frame = document.querySelector(sel);
    if (!frame) return;
    var cap = Math.floor(window.innerHeight * 0.8);
    frame.style.height = Math.min(Number(data.height) + 2, cap) + 'px';
  });
}
installHtmlArtifactListener();

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
    var renderedHint = isHtmlArtifactTool(tool)
      ? ' <span class="text-xs text-slate-500 italic">(rendered above)</span>'
      : '';
    return '<div class="' + borderCls + 'py-1.5">'
      + '<div class="flex items-center gap-2">'
      + '<span class="px-2 py-0.5 rounded-full text-xs font-medium bg-blue-900/60 text-blue-300 border border-blue-700/50">' + escapeHtml(tool.name || '') + '</span>'
      + '<span class="text-xs text-slate-400 ' + truncCls + 'flex-1 min-w-0">' + summaryHtml + renderedHint + '</span>'
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
    return "<div class=\"flex justify-end\">" + renderUserMessageBubble(msg.content, msg.is_voice, msg.timestamp, msg.uploaded_files) + "</div>";
  }
  if (msg.role === "assistant") {
    var artifactsHtml = renderHtmlArtifacts(msg.tools);
    var toolsHtml = renderToolActivity(msg.tools);
    return "<div class=\"flex justify-start\"><div class=\"max-w-[90%] overflow-hidden bg-slate-700 rounded-2xl rounded-bl-md px-4 py-2.5 text-sm\">"
      + mdDiv(msg.content) + artifactsHtml + toolsHtml + timeDiv() + "</div></div>";
  }
  if (msg.role === "system") {
    var titleAttr = msg.timestamp ? " title=\"" + formatBubbleTime(msg.timestamp) + "\"" : "";
    return "<div class=\"flex justify-center\"><div class=\"bg-slate-700/50 text-slate-400 text-xs px-3 py-1.5 rounded-full max-w-[85%] overflow-hidden truncate\"" + titleAttr + ">"
      + escapeHtml(msg.content) + "</div></div>";
  }
  if (msg.role === "task_delegated") {
    return "<div class=\"flex justify-start\"><div class=\"max-w-[90%] overflow-hidden bg-amber-900/30 border border-amber-700/30 rounded-2xl rounded-bl-md px-4 py-2.5 text-sm text-slate-300\">"
      + "<div class=\"flex items-center gap-2 text-amber-400 text-xs font-semibold mb-2\">"
      + "<svg class=\"w-3.5 h-3.5\" fill=\"none\" stroke=\"currentColor\" viewBox=\"0 0 24 24\"><path stroke-linecap=\"round\" stroke-linejoin=\"round\" stroke-width=\"2\" d=\"M13 5l7 7-7 7M5 5l7 7-7 7\"/></svg>"
      + "Delegated</div>"
      + "<div class=\"whitespace-pre-wrap\">" + escapeHtml(msg.content) + "</div>" + timeDiv() + "</div></div>";
  }
  if (msg.role === "worker_summary") {
    var escaped = escapeHtml(msg.full_content || "").replace(/"/g, "&quot;");
    return "<div class=\"flex justify-start\"><div class=\"max-w-[90%] overflow-hidden bg-emerald-900/40 border border-emerald-700/30 rounded-2xl rounded-bl-md px-4 py-2.5 text-sm text-slate-300 cursor-pointer\""
      + " data-full=\"" + escaped + "\""
      + " onclick=\"showTextModal(\x27Worker Result\x27, this.dataset.full)\">"
      + mdDiv(msg.content) + timeDiv("text-emerald-400/50") + "</div></div>";
  }
  if (msg.role === "plan") {
    return "<div class=\"flex justify-start\"><div class=\"max-w-[90%] overflow-hidden bg-slate-800 border border-blue-500/30 rounded-2xl px-4 py-3 text-sm\">"
      + "<div class=\"flex items-center gap-2 text-blue-400 text-xs font-semibold mb-2\">"
      + "<svg class=\"w-3.5 h-3.5\" fill=\"none\" stroke=\"currentColor\" viewBox=\"0 0 24 24\"><path stroke-linecap=\"round\" stroke-linejoin=\"round\" stroke-width=\"2\" d=\"M9 5H7a2 2 0 00-2 2v12a2 2 0 002 2h10a2 2 0 002-2V7a2 2 0 00-2-2h-2M9 5a2 2 0 002 2h2a2 2 0 002-2M9 5a2 2 0 012-2h2a2 2 0 012 2m-6 9l2 2 4-4\"/></svg>"
      + "Plan</div>"
      + mdDiv(msg.content) + timeDiv() + "</div></div>";
  }
  if (msg.role === "clone_start") {
    return "<div class=\"flex items-center gap-3 py-3 px-4\">"
      + "<div class=\"flex-1 border-t border-purple-500/40\"></div>"
      + "<div class=\"flex items-center gap-2 text-purple-400 text-xs\">"
      + "<svg class=\"w-3.5 h-3.5\" fill=\"none\" stroke=\"currentColor\" viewBox=\"0 0 24 24\"><path stroke-linecap=\"round\" stroke-linejoin=\"round\" stroke-width=\"2\" d=\"M6 3v12M6 9h6m0 0V3m0 6v6m0 0h6\"/></svg>"
      + "<span>Cloned from <a href=\"/?session=" + encodeURIComponent(msg.parent_session_id || "")
      + "\" class=\"text-purple-300 hover:text-purple-200 underline\">"
      + escapeHtml(msg.content || "") + "</a></span></div>"
      + "<div class=\"flex-1 border-t border-purple-500/40\"></div></div>";
  }
  if (msg.role === "separator") {
    var timeStr = msg.thinking_seconds != null ? " &middot; " + msg.thinking_seconds + "s" : "";
    var buttons = "";
    if (msg.event_index != null && sessionId) {
      buttons = "<button onclick=\"forkSession(\x27" + sessionId + "\x27, " + msg.event_index + ")\""
        + " class=\"p-0.5 text-slate-500 hover:text-green-400\" title=\"Clone to here\">"
        + "<svg class=\"w-3.5 h-3.5\" fill=\"none\" stroke=\"currentColor\" viewBox=\"0 0 24 24\"><path stroke-linecap=\"round\" stroke-linejoin=\"round\" stroke-width=\"2\" d=\"M6 3v12M6 9h6m0 0V3m0 6v6m0 0h6\"/></svg>"
        + "</button>"
        + "<button onclick=\"eloneSession(\x27" + sessionId + "\x27, " + msg.event_index + ")\""
        + " class=\"p-0.5 text-slate-500 hover:text-yellow-400\" title=\"Elon-e: retry with a fresh perspective\">"
        + "<svg class=\"w-3.5 h-3.5\" fill=\"none\" stroke=\"currentColor\" viewBox=\"0 0 24 24\"><path stroke-linecap=\"round\" stroke-linejoin=\"round\" stroke-width=\"2\" d=\"M13 10V3L4 14h7v7l9-11h-7z\"/></svg>"
        + "</button>";
    }
    return "<div class=\"flex items-center gap-3 py-2 px-4 separator-line group/sep\">"
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
  var el = wrapper.firstElementChild || wrapper;
  var streamEl = document.getElementById("streaming-msg");
  container.insertBefore(el, streamEl);
  el.querySelectorAll(".prose-msg").forEach(renderChatMath);
  if (forceScroll || wasAtBottom) {
    container.scrollTop = container.scrollHeight;
  } else {
    showScrollToBottom();
  }
}

function appendMessageObject(msg, sessionId) {
  _appendRenderedMessage(renderMessage(msg, sessionId || SESSION_ID), msg.role === "user");
}

async function sendMessage() {
  if (uploadsInFlight > 0) {
    showToast('Please wait for uploads to finish', true);
    return;
  }
  const input = document.getElementById('msg-input');
  const content = input.value.trim();
  const uploadedFilesForPayload = getUploadedFilesForPayload();
  if ((!content && !uploadedFilesForPayload.length) || !SESSION_ID) return;
  if (content.startsWith('/')) {
    const spaceIdx = content.indexOf(' ');
    const name = spaceIdx === -1 ? content.slice(1) : content.slice(1, spaceIdx);
    const args = spaceIdx === -1 ? '' : content.slice(spaceIdx + 1).trim();
    await executeSlashCommand(name, args, {displayText: content, uploadedFiles: uploadedFilesForPayload});
    return;
  }
  const contentWithCtx = applyWorkingContext(content);
  const payloadFiles = uploadedFilesForPayload.map((file) => ({
    filename: file.filename,
    path: file.path,
    size: file.size,
  }));
  clearSentUploadedFiles(uploadedFilesForPayload.map((file) => file.id));

  // Optimistic UI: append user message and bump session to top
  pendingUserMsg = true;
  appendMessage('user', content, false, new Date().toISOString(), payloadFiles);
  bumpCurrentSessionToTop();
  input.value = '';
  input.style.height = 'auto';
  if (DRAFT_KEY) localStorage.removeItem(DRAFT_KEY);

  // Start thinking indicator
  startThinking();

  try {
    const res = await fetch(`/api/chat/${SESSION_ID}/message`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ content: contentWithCtx, uploaded_files: payloadFiles }),
    });
    if (!res.ok) throw new Error(String(res.status));
  } catch (err) {
    console.error('Send failed:', err);
    pendingUserMsg = false;
    appendMessage('system', 'Failed to send message');
    stopThinking();
  }
}

function appendMessage(role, content, isVoice, timestamp, uploadedFiles) {
  var msg = {
    role: role,
    content: content || "",
    is_voice: !!isVoice,
    timestamp: timestamp || null,
    uploaded_files: uploadedFiles || null,
  };
  _appendRenderedMessage(renderMessage(msg, SESSION_ID), role === "user");
}

function appendSeparator(seconds, eventIndex) {
  var msg = {
    role: "separator",
    thinking_seconds: seconds,
    event_index: eventIndex,
  };
  _appendRenderedMessage(renderMessage(msg, SESSION_ID));
}

function appendCloneBanner(parentName, parentSessionId) {
  var msg = {
    role: "clone_start",
    content: parentName,
    parent_session_id: parentSessionId,
  };
  _appendRenderedMessage(renderMessage(msg, SESSION_ID));
}

function escapeHtml(str) {
  const d = document.createElement('div');
  d.textContent = str;
  return d.innerHTML;
}

// ---------------------------------------------------------------------------
// Scroll-to-bottom floating button
// ---------------------------------------------------------------------------
function showScrollToBottom() {
  const btn = document.getElementById('scroll-to-bottom');
  if (btn) btn.classList.remove('hidden');
}

function hideScrollToBottom() {
  const btn = document.getElementById('scroll-to-bottom');
  if (btn) btn.classList.add('hidden');
}

function scrollToBottom() {
  const container = document.getElementById('messages');
  if (container) container.scrollTop = container.scrollHeight;
  hideScrollToBottom();
}

// Hide the button when user scrolls back to bottom
document.addEventListener('DOMContentLoaded', () => {
  const container = document.getElementById('messages');
  if (container) {
    container.addEventListener('scroll', () => {
      if (shouldAutoScroll(container)) hideScrollToBottom();
    });
  }
});
