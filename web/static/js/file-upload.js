// ---------------------------------------------------------------------------
// File upload
// ---------------------------------------------------------------------------
let uploadedFiles = []; // Array of {id, filename, path, size, status, error}
let uploadsInFlight = 0;
let nextUploadId = 1;

// The send button's lock is a count, not a flag: attachment uploads count in
// uploadsInFlight, and a package that fills the input asynchronously opens an
// input-work window through inputWorkStarted and closes it through
// inputWorkSettled, both with the message the gate shows while the window is
// open. refreshSendLock below is the button's only disabled writer.
const inputWorkInFlight = new Map(); // waiting message -> open windows

const UPLOAD_IN_FLIGHT_MESSAGE = 'Please wait for the attachment upload to finish';

function inputWorkStarted(message) {
  inputWorkInFlight.set(message, (inputWorkInFlight.get(message) || 0) + 1);
  refreshSendLock();
}

function inputWorkSettled(message) {
  const open = inputWorkInFlight.get(message) - 1;
  if (open > 0) inputWorkInFlight.set(message, open);
  else inputWorkInFlight.delete(message);
  refreshSendLock();
}

// The chat-input and /compact send paths both refuse submission through this
// gate while an attachment upload or an input-work window is open; one
// definition keeps the refusal behavior from drifting.
function blockIfUploadsInFlight() {
  if (uploadsInFlight <= 0 && !inputWorkInFlight.size) return false;
  showToast(uploadsInFlight > 0 ? UPLOAD_IN_FLIGHT_MESSAGE : inputWorkInFlight.keys().next().value, true);
  return true;
}

function getUploadedFileById(id) {
  return uploadedFiles.find((file) => file.id === id) || null;
}

function getUploadedFilesForPayload() {
  return uploadedFiles.filter((file) => file.status === 'uploaded');
}

// The chat-message send path posts entries validated against UploadedFileRef
// in src/infra/models.py; this projection is the wire payload's one definition.
function toPayloadFiles(files) {
  return files.map((file) => ({
    filename: file.filename,
    path: file.path,
    size: file.size,
  }));
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
  const target = composerTargetSession();
  if (!target.id) return;
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
  setUploadingState(true);
  // The attachment belongs to the session the page displays at press: a
  // switch that has not landed holds the upload, and a click that moves on
  // drops it with its chip and a note.
  return bindComposerAction(target.id, 'interrupt',
      () => uploadFileToSession(target.id, file, item),
      () => {
        uploadedFiles = uploadedFiles.filter((chip) => chip.id !== item.id);
        renderFileChips();
        setUploadingState(false);
        showToast('Attachment dropped: the view moved on before the session loaded', true);
      });
}

async function uploadFileToSession(sessionId, file, item) {
  const form = new FormData();
  form.append('file', file);
  try {
    const res = await fetch(`/api/chat/${sessionId}/upload`, {method: 'POST', body: form});
    if (!res.ok) {
      const err = await res.json().catch(() => ({}));
      const errorText = String(err.detail || res.status);
      const chip = getUploadedFileById(item.id);
      if (chip && sessionId === SESSION_ID) {
        chip.status = 'failed';
        chip.error = errorText;
        renderFileChips();
      }
      showToast('Upload failed: ' + errorText, true);
      return;
    }

    const data = await res.json();
    const chip = getUploadedFileById(item.id);
    if (!chip || sessionId !== SESSION_ID) return;
    chip.filename = data.filename;
    chip.path = data.path;
    chip.size = data.size;
    chip.status = 'uploaded';
    chip.error = '';
    renderFileChips();
  } catch (err) {
    console.error('Upload failed:', err);
    const chip = getUploadedFileById(item.id);
    if (chip && sessionId === SESSION_ID) {
      chip.status = 'failed';
      chip.error = err.message;
      renderFileChips();
    }
    showToast('Upload failed: ' + err.message, true);
  } finally {
    setUploadingState(false);
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

// The send button's single disabled writer: locked exactly while an attachment
// upload or an input-work window is open, painted with the same greyed
// style the upload path always used. Thinking state never writes here, so a
// master run can neither lock nor unlock the button.
function refreshSendLock() {
  const btn = document.getElementById('send-btn');
  if (!btn) return;
  if (uploadsInFlight > 0 || inputWorkInFlight.size) {
    btn.setAttribute('disabled', '');
    btn.classList.add('opacity-50', 'cursor-not-allowed');
    btn.classList.remove('hover:bg-blue-500');
    return;
  }
  btn.removeAttribute('disabled');
  btn.classList.remove('opacity-50', 'cursor-not-allowed');
  btn.classList.add('hover:bg-blue-500');
}

// The attachment count's one adjuster: `busy` mirrors the upload window the
// caller just opened or closed, and the count drives the lock write.
function setUploadingState(busy) {
  uploadsInFlight += busy ? 1 : -1;
  refreshSendLock();
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
