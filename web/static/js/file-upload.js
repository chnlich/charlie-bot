// ---------------------------------------------------------------------------
// File upload
// ---------------------------------------------------------------------------
let uploadedFiles = []; // Array of {filename, path, size}

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
