// ---------------------------------------------------------------------------
// LaTeX panel
// ---------------------------------------------------------------------------
let latexPanelOpen = false;
let latexView = 'pdf'; // 'pdf' or 'tex'
let latexEditorDirty = false;

function toggleLatexPanel() {
  if (latexPanelOpen) {
    switchTab('chat');
  } else {
    switchTab('chat-tex');
  }
}

function switchLatexView(view) {
  latexView = view;
  const pdfView = document.getElementById('latex-pdf-view');
  const texView = document.getElementById('latex-tex-view');
  const pdfBtn = document.getElementById('btn-latex-pdf');
  const texBtn = document.getElementById('btn-latex-tex');
  if (view === 'pdf') {
    pdfView.classList.remove('hidden');
    texView.classList.add('hidden');
    pdfBtn.classList.add('latex-toggle-active');
    pdfBtn.classList.remove('text-slate-400');
    texBtn.classList.remove('latex-toggle-active');
    texBtn.classList.add('text-slate-400');
    loadLatexPdf();
  } else {
    pdfView.classList.add('hidden');
    texView.classList.remove('hidden');
    texBtn.classList.add('latex-toggle-active');
    texBtn.classList.remove('text-slate-400');
    pdfBtn.classList.remove('latex-toggle-active');
    pdfBtn.classList.add('text-slate-400');
    loadLatexSource();
  }
}

async function loadLatexPdf() {
  const container = document.getElementById('latex-pdf-canvas-container');
  if (!container) return;
  // Skip re-fetch if PDF is already rendered (preserves scroll position)
  if (container.querySelector('canvas')) return;
  container.innerHTML = '<p class="text-slate-500 text-sm">Loading PDF...</p>';
  try {
    const resp = await fetch('/api/latex/pdf?t=' + Date.now());
    if (!resp.ok) { container.innerHTML = '<p class="text-red-400 text-sm">PDF not found. Compile first.</p>'; return; }
    const data = await resp.arrayBuffer();
    const pdf = await pdfjsLib.getDocument({data}).promise;
    container.innerHTML = '';
    const containerWidth = container.clientWidth - 16;
    const dpr = window.devicePixelRatio || 1;
    for (let i = 1; i <= pdf.numPages; i++) {
      const page = await pdf.getPage(i);
      const cssScale = containerWidth / page.getViewport({scale: 1}).width;
      const viewport = page.getViewport({scale: cssScale * dpr});
      const canvas = document.createElement('canvas');
      canvas.width = viewport.width;
      canvas.height = viewport.height;
      canvas.style.width = Math.floor(viewport.width / dpr) + 'px';
      canvas.style.height = Math.floor(viewport.height / dpr) + 'px';
      canvas.className = 'shadow-lg';
      container.appendChild(canvas);
      await page.render({canvasContext: canvas.getContext('2d'), viewport}).promise;
    }
  } catch (err) {
    console.error('PDF render failed:', err);
    container.innerHTML = '<p class="text-red-400 text-sm">Failed to render PDF: ' + err.message + '</p>';
  }
}

async function loadLatexGitInfo() {
  const el = document.getElementById('latex-git-info');
  if (!el) return;
  try {
    const resp = await fetch('/api/latex/git-info');
    if (!resp.ok) { el.textContent = ''; el.title = ''; return; }
    const info = await resp.json();
    el.textContent = `${info.repo_name} (${info.branch})`;
    el.title = info.repo_path;
  } catch (e) {
    el.textContent = '';
    el.title = '';
  }
}

async function loadLatexSource() {
  try {
    const res = await fetch('/api/latex/source');
    if (!res.ok) { showToast('Failed to load source', true); return; }
    const text = await res.text();
    const editor = document.getElementById('latex-editor');
    if (editor) { editor.value = text; latexEditorDirty = false; }
  } catch (err) {
    console.error('Load source failed:', err);
    showToast('Failed to load source: ' + err.message, true);
  }
}

async function saveLatexSource() {
  const editor = document.getElementById('latex-editor');
  try {
    const res = await fetch('/api/latex/source', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ content: editor.value }),
    });
    if (!res.ok) { showToast('Failed to save source', true); return false; }
    latexEditorDirty = false;
    return true;
  } catch (err) {
    console.error('Save source failed:', err);
    showToast('Failed to save: ' + err.message, true);
    return false;
  }
}

async function compileLatex() {
  const btn = document.getElementById('btn-compile');
  const status = document.getElementById('compile-status');
  btn.disabled = true;
  btn.textContent = 'Compiling...';
  status.textContent = '';
  if (latexView === 'tex' && latexEditorDirty) {
    const saved = await saveLatexSource();
    if (!saved) { btn.disabled = false; btn.textContent = 'Compile'; return; }
  }
  try {
    const res = await fetch('/api/latex/compile', { method: 'POST' });
    const data = await res.json();
    if (data.ok) {
      status.textContent = 'Done';
      status.className = 'text-xs text-green-400';
      if (latexView === 'pdf') loadLatexPdf();
    } else {
      status.textContent = 'Failed (see console)';
      status.className = 'text-xs text-red-400';
      console.error('LaTeX compilation log:', data.log);
    }
  } catch (err) {
    status.textContent = 'Error';
    status.className = 'text-xs text-red-400';
    console.error('Compile failed:', err);
  } finally {
    btn.disabled = false;
    btn.textContent = 'Compile';
    setTimeout(() => { status.textContent = ''; status.className = 'text-xs text-slate-500'; }, 5000);
  }
}

// ---------------------------------------------------------------------------
// Diff review modal
// ---------------------------------------------------------------------------
async function showDiffModal() {
  try {
    const res = await fetch('/api/latex/diff');
    if (!res.ok) return;
    const data = await res.json();
    const patch = Diff.createTwoFilesPatch(
      'report.tex', 'report.tex (proposed)',
      data.old, data.new, '', '', { context: 5 }
    );
    const html = Diff2Html.html(patch, {
      drawFileList: false,
      matching: 'lines',
      outputFormat: 'side-by-side',
    });
    document.getElementById('diff-modal-body').innerHTML = html;
    document.getElementById('diff-modal').classList.remove('hidden');
  } catch (err) {
    console.error('showDiffModal failed:', err);
    showToast('Failed to load diff', true);
  }
}

async function acceptTexEdit() {
  try {
    const res = await fetch('/api/latex/accept', { method: 'POST' });
    if (!res.ok) { showToast('Accept failed', true); return; }
    document.getElementById('diff-modal').classList.add('hidden');
    showToast('TeX changes accepted');
    if (latexView === 'tex') loadLatexSource();
  } catch (err) {
    console.error('acceptTexEdit failed:', err);
    showToast('Accept failed: ' + err.message, true);
  }
}

async function rejectTexEdit() {
  try {
    await fetch('/api/latex/reject', { method: 'POST' });
  } catch (err) {
    console.error('rejectTexEdit failed:', err);
  }
  document.getElementById('diff-modal').classList.add('hidden');
  showToast('TeX changes rejected');
}

// ---------------------------------------------------------------------------
// LaTeX panel resize
// ---------------------------------------------------------------------------
function initLatexResize() {
  const handle = document.getElementById('latex-resize-handle');
  const panel = document.getElementById('latex-panel');
  const container = panel.parentElement;
  const saved = localStorage.getItem('latex-panel-pct');
  if (saved) panel.style.width = saved + '%';

  let startX, startW, containerW;
  handle.addEventListener('mousedown', (e) => {
    e.preventDefault();
    startX = e.clientX;
    containerW = container.offsetWidth;
    startW = panel.offsetWidth;
    handle.classList.add('active');
    document.body.classList.add('resizing');
    const pdfContainer = document.getElementById('latex-pdf-canvas-container');
    if (pdfContainer) pdfContainer.style.pointerEvents = 'none';

    function onMove(e) {
      // Panel is on the RIGHT, so dragging left = bigger panel
      const delta = startX - e.clientX;
      const w = Math.min(Math.max(startW + delta, containerW * 0.2), containerW * 0.8);
      panel.style.width = w + 'px';
    }
    function onUp() {
      if (pdfContainer) pdfContainer.style.pointerEvents = '';
      handle.classList.remove('active');
      document.body.classList.remove('resizing');
      const pct = (panel.offsetWidth / container.offsetWidth * 100).toFixed(1);
      localStorage.setItem('latex-panel-pct', pct);
      panel.style.width = pct + '%';
      document.removeEventListener('mousemove', onMove);
      document.removeEventListener('mouseup', onUp);
      if (latexView === 'pdf') loadLatexPdf();
    }
    document.addEventListener('mousemove', onMove);
    document.addEventListener('mouseup', onUp);
  });
}
