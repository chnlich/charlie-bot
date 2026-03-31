// ---------------------------------------------------------------------------
// File upload
// ---------------------------------------------------------------------------
let uploadedFiles = []; // Array of {filename, path, size}
let uploadsInFlight = 0;

async function handleFiles(input) {
  const files = Array.from(input.files);
  input.value = ''; // Reset so the same file can be re-selected
  for (const file of files) {
    await uploadFile(file);
  }
}

async function uploadFile(file) {
  if (!SESSION_ID) return;
  const form = new FormData();
  form.append('file', file);
  uploadsInFlight++;
  setUploadingState(true);
  try {
    const res = await fetch(`/api/chat/${SESSION_ID}/upload`, { method: 'POST', body: form });
    if (!res.ok) {
      const err = await res.json().catch(() => ({}));
      showToast('Upload failed: ' + (err.detail || res.status), true);
      return;
    }
    const data = await res.json();
    uploadedFiles.push(data);
    renderFileChips();
  } catch (err) {
    console.error('Upload failed:', err);
    showToast('Upload failed: ' + err.message, true);
  } finally {
    uploadsInFlight--;
    if (uploadsInFlight === 0) setUploadingState(false);
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
  container.innerHTML = uploadedFiles.map((f, i) =>
    `<span class="file-chip"><span title="${escapeHtml(f.filename)}">${escapeHtml(f.filename)}</span>` +
    `<button onclick="removeFile(${i})" title="Remove">&#x2715;</button></span>`
  ).join('');
}

function removeFile(i) {
  uploadedFiles.splice(i, 1);
  renderFileChips();
}

function setUploadingState(busy) {
  const btn = document.getElementById('send-btn');
  const container = document.getElementById('file-chips');
  if (busy) {
    if (btn) {
      btn.setAttribute('disabled', '');
      btn.classList.add('opacity-50', 'cursor-not-allowed');
      btn.classList.remove('hover:bg-blue-500');
    }
    if (container) {
      container.classList.remove('hidden');
      if (!container.querySelector('.uploading-indicator')) {
        const chip = document.createElement('span');
        chip.className = 'file-chip uploading-indicator';
        chip.textContent = 'Uploading\u2026';
        container.prepend(chip);
      }
    }
  } else {
    if (btn) {
      btn.removeAttribute('disabled');
      btn.classList.remove('opacity-50', 'cursor-not-allowed');
      btn.classList.add('hover:bg-blue-500');
    }
    if (container) {
      const indicator = container.querySelector('.uploading-indicator');
      if (indicator) indicator.remove();
      if (!uploadedFiles.length) {
        container.classList.add('hidden');
        container.innerHTML = '';
      }
    }
  }
}
