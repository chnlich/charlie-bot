// /diff page: file-level lazy-loading diff2html renderer.
//
// Compare fetches a cheap per-file manifest and renders one collapsed row per file.
// Expanding a row fetches and renders that single file's diff on demand, so a huge
// cross-branch diff never lands in one payload and only expanded files are in the DOM.
(() => {
  const repoInput = document.getElementById('repo-input');
  const repoList = document.getElementById('repo-list');
  const baseInput = document.getElementById('base-input');
  const headInput = document.getElementById('head-input');
  const branchesList = document.getElementById('branches-list');
  const modeSelect = document.getElementById('mode-select');
  const formatSelect = document.getElementById('format-select');
  const form = document.getElementById('diff-form');
  const openCodeServerButton = document.getElementById('open-codeserver');
  const statusEl = document.getElementById('status');
  const loadingEl = document.getElementById('loading');
  const errorEl = document.getElementById('error');
  const emptyEl = document.getElementById('empty');
  const outputEl = document.getElementById('diff-output');

  // Bumped on every Compare so stale per-file fetches don't render into a newer view.
  let comparisonToken = 0;

  const STATUS_LABELS = { A: 'added', M: 'modified', D: 'deleted', R: 'renamed', C: 'copied', T: 'type changed' };
  const STATUS_CLASSES = {
    A: 'bg-green-100 text-green-700',
    M: 'bg-amber-100 text-amber-700',
    D: 'bg-red-100 text-red-700',
    R: 'bg-blue-100 text-blue-700',
    C: 'bg-blue-100 text-blue-700',
    T: 'bg-purple-100 text-purple-700',
  };

  function show(el) { el.classList.remove('hidden'); el.classList.add('flex'); }
  function hide(el) { el.classList.add('hidden'); el.classList.remove('flex'); }
  function setError(msg) {
    errorEl.textContent = msg;
    errorEl.classList.remove('hidden');
  }
  function clearError() { errorEl.classList.add('hidden'); errorEl.textContent = ''; }
  function clearOutput() { outputEl.innerHTML = ''; emptyEl.classList.add('hidden'); }
  function responseDetail(data, status) {
    const detail = (data && (data.detail?.message || data.detail)) || `HTTP ${status}`;
    return typeof detail === 'string' ? detail : JSON.stringify(detail);
  }

  function readQuery() {
    const p = new URLSearchParams(location.search);
    return {
      repo: p.get('repo') || '',
      base: p.get('base') || '',
      head: p.get('head') || '',
      mode: p.get('mode') || 'three-dot',
      outputFormat: p.get('outputFormat') || 'side-by-side',
    };
  }

  function writeQuery({ repo, base, head, mode, outputFormat }) {
    const p = new URLSearchParams();
    if (repo) p.set('repo', repo);
    if (base) p.set('base', base);
    if (head) p.set('head', head);
    if (mode) p.set('mode', mode);
    if (outputFormat) p.set('outputFormat', outputFormat);
    const qs = p.toString();
    history.replaceState(null, '', qs ? `?${qs}` : location.pathname);
  }

  async function loadRepos() {
    try {
      const r = await fetch('/api/git/repos');
      if (!r.ok) return;
      const repos = await r.json();
      repoList.innerHTML = '';
      for (const repo of repos) {
        const opt = document.createElement('option');
        opt.value = repo.path;
        opt.label = repo.label;
        repoList.appendChild(opt);
      }
    } catch (e) {
      // Free-text fallback — datalist is just a hint.
    }
  }

  async function loadBranches(repo) {
    branchesList.innerHTML = '';
    if (!repo) return;
    try {
      const r = await fetch(`/api/git/branches?repo=${encodeURIComponent(repo)}`);
      if (!r.ok) return;
      const branches = await r.json();
      for (const name of branches) {
        const opt = document.createElement('option');
        opt.value = name;
        branchesList.appendChild(opt);
      }
    } catch (e) {
      // Free-text fallback.
    }
  }

  function statusBadge(status) {
    const span = document.createElement('span');
    span.textContent = status;
    span.title = STATUS_LABELS[status] || status;
    span.className =
      `inline-flex items-center justify-center w-5 h-5 rounded text-xs font-bold shrink-0 ${
        STATUS_CLASSES[status] || 'bg-slate-100 text-slate-700'}`;
    return span;
  }

  function renderFileDiff(container, diffText) {
    const ui = new Diff2HtmlUI(container, diffText, {
      drawFileList: false,
      matching: 'lines',
      outputFormat: formatSelect.value,
    });
    ui.draw();
    ui.highlightCode();
  }

  async function fetchFileDiff(params, path, force, oldPath) {
    const qs = new URLSearchParams({
      repo: params.repo,
      base: params.base,
      head: params.head,
      mode: params.mode,
      path,
    });
    if (oldPath) qs.set('old_path', oldPath);
    if (force) qs.set('force', 'true');
    const resp = await fetch(`/api/git/diff/file?${qs.toString()}`);
    const data = await resp.json();
    if (!resp.ok) {
      throw new Error(responseDetail(data, resp.status));
    }
    return data;
  }

  // Fetch one file's diff into a row body and render it. force=true bypasses the
  // server-side per-file cap (used by the "load anyway" placeholder).
  async function loadFileInto(body, params, file, token, force) {
    body.innerHTML =
      '<div class="px-3 py-2 text-xs text-slate-500 flex items-center gap-2"><span class="spinner"></span>Loading…</div>';
    let data;
    try {
      data = await fetchFileDiff(params, file.path, force, file.old_path);
    } catch (e) {
      if (token !== comparisonToken) return;
      body.innerHTML = '';
      const err = document.createElement('div');
      err.className = 'px-3 py-2 text-sm text-red-700 bg-red-50';
      err.textContent = `Failed to load ${file.path}: ${e.message}`;
      body.appendChild(err);
      return;
    }
    if (token !== comparisonToken) return;
    body.innerHTML = '';

    if (data.too_large) {
      const placeholder = document.createElement('button');
      placeholder.type = 'button';
      placeholder.className = 'w-full text-left px-3 py-2 text-sm text-amber-700 bg-amber-50 hover:bg-amber-100';
      placeholder.textContent =
        `${data.size_bytes.toLocaleString()} bytes — too large to render, click to load anyway`;
      placeholder.addEventListener('click', () => loadFileInto(body, params, file, token, true));
      body.appendChild(placeholder);
      return;
    }

    const diffText = data.diff || '';
    if (!diffText.trim()) {
      const none = document.createElement('div');
      none.className = 'px-3 py-2 text-xs text-slate-500 italic';
      none.textContent = 'No changes to display.';
      body.appendChild(none);
      return;
    }
    const container = document.createElement('div');
    body.appendChild(container);
    renderFileDiff(container, diffText);
  }

  function buildFileRow(params, file, token) {
    const row = document.createElement('div');
    row.className = 'border border-slate-200 rounded mb-2 overflow-hidden';

    const header = document.createElement('button');
    header.type = 'button';
    header.className = 'w-full flex items-center gap-3 px-3 py-2 bg-slate-50 hover:bg-slate-100 text-left';

    const chevron = document.createElement('span');
    chevron.textContent = '▶';
    chevron.className = 'text-slate-400 text-xs shrink-0 transition-transform';

    const pathEl = document.createElement('span');
    pathEl.textContent = file.path;
    pathEl.className = 'font-mono text-sm text-slate-800 flex-1 truncate';

    const counts = document.createElement('span');
    counts.className = 'font-mono text-xs whitespace-nowrap shrink-0';
    const add = document.createElement('span');
    add.className = 'text-green-600';
    add.textContent = `+${file.additions}`;
    const del = document.createElement('span');
    del.className = 'text-red-600 ml-1';
    del.textContent = `-${file.deletions}`;
    counts.append(add, del);

    header.append(chevron, statusBadge(file.status), pathEl, counts);

    const body = document.createElement('div');
    body.className = 'hidden';

    let expanded = false;
    header.addEventListener('click', () => {
      expanded = !expanded;
      if (!expanded) {
        // Collapse: drop the rendered diff so only expanded files stay in the DOM.
        body.innerHTML = '';
        body.classList.add('hidden');
        chevron.style.transform = '';
        return;
      }
      chevron.style.transform = 'rotate(90deg)';
      body.classList.remove('hidden');
      loadFileInto(body, params, file, token, false);
    });

    row.append(header, body);
    return row;
  }

  async function compare(params) {
    clearError();
    clearOutput();
    show(loadingEl);
    statusEl.textContent = '';
    const token = ++comparisonToken;
    const qs = new URLSearchParams({
      repo: params.repo,
      base: params.base,
      head: params.head,
      mode: params.mode,
    });

    let resp;
    try {
      resp = await fetch(`/api/git/diff/files?${qs.toString()}`);
    } catch (e) {
      hide(loadingEl);
      setError(`Network error: ${e.message}`);
      return;
    }

    let data = null;
    try {
      data = await resp.json();
    } catch (e) {
      hide(loadingEl);
      setError(`Server returned non-JSON response (HTTP ${resp.status})`);
      return;
    }

    hide(loadingEl);
    if (token !== comparisonToken) return;

    if (!resp.ok) {
      setError(responseDetail(data, resp.status));
      return;
    }

    const files = data.files || [];
    const sep = data.mode === 'three-dot' ? '...' : '..';
    statusEl.textContent =
      `${data.total_files} files · +${data.total_additions.toLocaleString()} ` +
      `-${data.total_deletions.toLocaleString()} · ${data.mode} · ${data.base}${sep}${data.head}`;

    if (files.length === 0) {
      emptyEl.classList.remove('hidden');
      return;
    }
    for (const file of files) {
      outputEl.appendChild(buildFileRow(params, file, token));
    }
  }

  function currentParams() {
    return {
      repo: repoInput.value.trim(),
      base: baseInput.value.trim(),
      head: headInput.value.trim(),
      mode: modeSelect.value,
      outputFormat: formatSelect.value,
    };
  }

  form.addEventListener('submit', (e) => {
    e.preventDefault();
    const params = currentParams();
    if (!params.repo || !params.base || !params.head) {
      setError('repo, base, and head are required');
      return;
    }
    writeQuery(params);
    compare(params);
  });

  async function openCodeServer() {
    clearError();
    const repo = repoInput.value.trim();
    if (!repo) {
      setError('repo is required to open code-server');
      return;
    }
    statusEl.textContent = 'preparing code-server…';

    let resp;
    try {
      resp = await fetch(`/api/code-server/open?folder=${encodeURIComponent(repo)}`);
    } catch (e) {
      statusEl.textContent = '';
      setError(`Network error: ${e.message}`);
      return;
    }

    let data = null;
    try {
      data = await resp.json();
    } catch (e) {
      statusEl.textContent = '';
      setError(`Server returned non-JSON response (HTTP ${resp.status})`);
      return;
    }

    if (!resp.ok) {
      setError(responseDetail(data, resp.status));
      statusEl.textContent = '';
      return;
    }

    const url = `${location.protocol}//${location.hostname}:${data.port}/?folder=${encodeURIComponent(data.folder)}`;
    window.open(url, '_blank');
    statusEl.textContent = '';
  }

  if (openCodeServerButton) {
    openCodeServerButton.addEventListener('click', openCodeServer);
  }

  repoInput.addEventListener('change', () => {
    loadBranches(repoInput.value.trim());
  });

  // Init from URL.
  (async () => {
    const q = readQuery();
    repoInput.value = q.repo;
    baseInput.value = q.base;
    headInput.value = q.head;
    modeSelect.value = q.mode;
    formatSelect.value = q.outputFormat;

    await loadRepos();
    if (q.repo) await loadBranches(q.repo);

    if (q.repo && q.base && q.head) {
      compare(q);
    }
  })();
})();
