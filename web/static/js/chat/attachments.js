
(function() {
  const Chat = globalThis.Chat;
  const formatBubbleTime = Chat.formatBubbleTime;
  const escapeHtmlAttr = Chat.escapeHtmlAttr;

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

// The user bubble's input-mode badge: a package that sets an input mode
// registers the badge HTML for it when its script loads.
const inputModeBadges = new Map();

function registerInputMode(mode, badgeHtml) {
  if (!mode || !badgeHtml) throw new Error('Invalid input mode registration');
  if (inputModeBadges.has(mode)) throw new Error('Duplicate input mode: ' + mode);
  inputModeBadges.set(mode, badgeHtml);
}

// A user text folds by its rendered height: the clamp (styles.css, .user-msg-clamp) shows its first ten lines, and the
// 展开全文 control shows only while the clamp hides some of the text. Every text starts clamped and undecided, and the
// observer below decides it once it is laid out.
function renderUserTextHtml(text) {
  return '<div class="user-msg-fold" data-folded="true" data-overflows="false">'
    + `<div class="user-msg-clamp whitespace-pre-wrap">${escapeHtml(text)}</div>`
    + `<button type="button" class="user-msg-more"`
      + ` onclick="this.closest('.user-msg-fold').dataset.folded='false'">展开全文</button>`
    + '</div>';
}

// A clamp hides text exactly when its content is taller than its box. A message the reader opened stays open, so only a
// folded clamp is decided; a clamp that left the page is dropped from the observer.
function fitUserMessageClamp(clamp, sizes) {
  if (!clamp.isConnected) {
    sizes.unobserve(clamp);
    return;
  }
  const fold = clamp.parentElement;
  if (fold.dataset.folded !== 'true') return;
  fold.dataset.overflows = String(clamp.scrollHeight > clamp.clientHeight);
}

// Rows are built detached and mount into the message list later (turn-engine.js, buildNode), so the list is watched.
// Each clamp that enters it is observed; the observer decides it at its first layout and again at every size change,
// and a width change is the one that rewraps the text.
document.addEventListener('DOMContentLoaded', () => {
  const sizes = new ResizeObserver((entries) => {
    for (const entry of entries) fitUserMessageClamp(entry.target, sizes);
  });
  const observeClamps = (node) => {
    if (node.matches('.user-msg-clamp')) sizes.observe(node);
    for (const clamp of node.querySelectorAll('.user-msg-clamp')) sizes.observe(clamp);
  };
  const list = document.getElementById('messages');
  observeClamps(list);
  new MutationObserver((records) => {
    for (const record of records) {
      for (const node of record.addedNodes) {
        if (node.nodeType === Node.ELEMENT_NODE) observeClamps(node);
      }
    }
  }).observe(list, {childList: true, subtree: true});
});

function renderUserMessageBubble(content, inputMode, timestamp, uploadedFiles, requestId, deliveryStatus) {
  const normalized = normalizeUserMessage(content, uploadedFiles);
  // On blue-600, slate-100 holds about 4.7:1 for the time and delivery lines, quieter than
  // the message text. The darker assistant bubble's translucent slate would read at about
  // 1.5:1 here.
  const timeHtml = timestamp ? '<div class="text-[10px] text-slate-100 mt-1">' + formatBubbleTime(timestamp) + '</div>' : '';
  const deliveryHtml = requestId && deliveryStatus
    ? '<div class="text-[10px] mt-1"><span id="message-delivery-' + escapeHtmlAttr(requestId)
      + '" class="text-slate-100">' + escapeHtml(deliveryStatus) + '</span></div>'
    : '';
  const textHtml = normalized.content ? renderUserTextHtml(normalized.content) : '';
  const attachmentsHtml = renderUserAttachments(normalized.uploadedFiles, Boolean(textHtml));
  return `<div class="max-w-[75%] overflow-hidden bg-blue-600 rounded-2xl rounded-br-md px-4 py-2.5 text-sm">`
    + (inputModeBadges.has(inputMode)
      ? '<span class="text-xs text-blue-200 block mb-1">' + inputModeBadges.get(inputMode) + '</span>'
      : '')
    + textHtml
    + attachmentsHtml
    + timeHtml
    + deliveryHtml
    + '</div>';
}

Chat.wire({
  renderUserMessageBubble,
}, {
  normalizeUserMessage,
  registerInputMode,
});

})();
