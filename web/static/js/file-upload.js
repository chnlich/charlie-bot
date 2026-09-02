// ---------------------------------------------------------------------------
// File upload
// ---------------------------------------------------------------------------
let uploadedFiles = []; // Array of {id, filename, path, size, status, error}
let uploadsInFlight = 0;
let nextUploadId = 1;

// The chat-input and slash-command send paths both refuse submission with this
// toast while an upload is in flight; one constant keeps the two user-visible
// copies from drifting.
const UPLOADS_IN_FLIGHT_MESSAGE = 'Please wait for uploads to finish';

function getUploadedFileById(id) {
  return uploadedFiles.find((file) => file.id === id) || null;
}

function getUploadedFilesForPayload() {
  return uploadedFiles.filter((file) => file.status === 'uploaded');
}

function clearSentUploadedFiles(ids) {
  if (!Array.isArray(ids) || !ids.length) return;
  const sentIds = new Set(ids);
  uploadedFiles = uploadedFiles.filter((file) => !sentIds.has(file.id));
  renderFileChips();
}

function renderFileChipIcon(file) {
  if (file.status === 'uploading') {
    return '<span class="file-chip-icon file-chip-icon--spinner" aria-hidden="true"></span>';
  }
  if (file.status === 'failed') {
    return '<span class="file-chip-icon" aria-hidden="true">&#x26A0;</span>';
  }
  return '<span class="file-chip-icon" aria-hidden="true">&#x2713;</span>';
}

async function handleFiles(input) {
  const files = Array.from(input.files);
  input.value = ''; // Reset so the same file can be re-selected
  for (const file of files) {
    uploadFile(file);
  }
}

async function uploadFile(file) {
  if (!SESSION_ID) return;
  const item = {
    id: nextUploadId++,
    filename: file.name,
    path: '',
    size: file.size,
    status: 'uploading',
    error: '',
  };
  uploadedFiles.push(item);
  renderFileChips();

  const uploadSessionId = SESSION_ID;
  const form = new FormData();
  form.append('file', file);
  uploadsInFlight++;
  setUploadingState(true);
  try {
    const res = await fetch(`/api/chat/${uploadSessionId}/upload`, {method: 'POST', body: form});
    if (!res.ok) {
      const err = await res.json().catch(() => ({}));
      const errorText = String(err.detail || res.status);
      const target = getUploadedFileById(item.id);
      if (target && uploadSessionId === SESSION_ID) {
        target.status = 'failed';
        target.error = errorText;
        renderFileChips();
      }
      showToast('Upload failed: ' + errorText, true);
      return;
    }

    const data = await res.json();
    const target = getUploadedFileById(item.id);
    if (!target || uploadSessionId !== SESSION_ID) return;
    target.filename = data.filename;
    target.path = data.path;
    target.size = data.size;
    target.status = 'uploaded';
    target.error = '';
    renderFileChips();
  } catch (err) {
    console.error('Upload failed:', err);
    const target = getUploadedFileById(item.id);
    if (target && uploadSessionId === SESSION_ID) {
      target.status = 'failed';
      target.error = err.message;
      renderFileChips();
    }
    showToast('Upload failed: ' + err.message, true);
  } finally {
    uploadsInFlight--;
    setUploadingState(uploadsInFlight > 0);
  }
}

function renderFileChips() {
  const container = document.getElementById('file-chips');
  if (!container) return;
  if (!uploadedFiles.length) {
    container.classList.add('hidden');
    container.innerHTML = '';
    return;
  }

  container.classList.remove('hidden');
  container.innerHTML = uploadedFiles.map((file) => {
    const title = escapeHtml(file.error || file.path || file.filename);
    const removeButton = file.status === 'uploading'
      ? ''
      : `<button onclick="removeFile(${file.id})" title="Remove">&#x2715;</button>`;
    const statusLabel = file.status === 'failed' ? '<span class="file-chip-state">Failed</span>' : '';
    return `<span class="file-chip file-chip--${file.status}" title="${title}">`
      + renderFileChipIcon(file)
      + `<span class="file-chip-name">${escapeHtml(file.filename)}</span>`
      + statusLabel
      + removeButton
      + '</span>';
  }).join('');
}

function removeFile(id) {
  uploadedFiles = uploadedFiles.filter((file) => file.id !== id);
  renderFileChips();
}

function setUploadingState(busy) {
  const btn = document.getElementById('send-btn');
  if (!btn) return;
  if (busy) {
    btn.setAttribute('disabled', '');
    btn.classList.add('opacity-50', 'cursor-not-allowed');
    btn.classList.remove('hover:bg-blue-500');
    return;
  }
  btn.removeAttribute('disabled');
  btn.classList.remove('opacity-50', 'cursor-not-allowed');
  btn.classList.add('hover:bg-blue-500');
}

// ---------------------------------------------------------------------------
// Paste-screenshot support
// ---------------------------------------------------------------------------
const MIME_EXTENSION_MAP = {
  'image/png': 'png',
  'image/jpeg': 'jpg',
  'image/jpg': 'jpg',
  'image/gif': 'gif',
  'image/webp': 'webp',
  'image/bmp': 'bmp',
  'image/svg+xml': 'svg',
  'image/tiff': 'tiff',
  'image/heic': 'heic',
  'image/heif': 'heif',
};

function extensionForMime(mime) {
  return MIME_EXTENSION_MAP[(mime || '').toLowerCase()] || 'png';
}

function screenshotTimestamp(date) {
  const pad = (n) => String(n).padStart(2, '0');
  return (
    String(date.getFullYear())
    + pad(date.getMonth() + 1)
    + pad(date.getDate())
    + '-'
    + pad(date.getHours())
    + pad(date.getMinutes())
    + pad(date.getSeconds())
  );
}

function handlePaste(event) {
  const data = event.clipboardData;
  if (!data || !data.items) return;
  const imageBlobs = [];
  for (const item of data.items) {
    if (item.kind === 'file' && item.type && item.type.startsWith('image/')) {
      const blob = item.getAsFile();
      if (blob) imageBlobs.push(blob);
    }
  }
  if (!imageBlobs.length) return;
  event.preventDefault();
  const stamp = screenshotTimestamp(new Date());
  imageBlobs.forEach((blob, idx) => {
    const ext = extensionForMime(blob.type);
    const suffix = imageBlobs.length > 1 ? `-${idx + 1}` : '';
    const name = `screenshot-${stamp}${suffix}.${ext}`;
    const renamed = new File([blob], name, {type: blob.type});
    uploadFile(renamed);
  });
}

function initPasteUpload() {
  const input = document.getElementById('msg-input');
  if (!input) return;
  input.addEventListener('paste', handlePaste);
}

if (document.readyState === 'loading') {
  document.addEventListener('DOMContentLoaded', initPasteUpload);
} else {
  initPasteUpload();
}
