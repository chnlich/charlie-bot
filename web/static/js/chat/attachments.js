
(function() {
  const Chat = globalThis.Chat;
  const formatBubbleTime = Chat.formatBubbleTime;

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

Chat.normalizeUserMessage = normalizeUserMessage;
Chat.renderUserMessageBubble = renderUserMessageBubble;
Chat.expose([
  'normalizeUserMessage',
  'renderUserMessageBubble',
]);

})();
