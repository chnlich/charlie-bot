// ---------------------------------------------------------------------------
// File upload
// ---------------------------------------------------------------------------
let uploadedFiles = []; // Array of {id, filename, path, size, status, error}
let uploadsInFlight = 0;
let nextUploadId = 1;

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
