// /diff page: form-driven diff2html renderer.
(() => {
  const repoInput = document.getElementById('repo-input');
  const repoList = document.getElementById('repo-list');
  const baseInput = document.getElementById('base-input');
  const headInput = document.getElementById('head-input');
  const branchesList = document.getElementById('branches-list');
  const modeSelect = document.getElementById('mode-select');
  const formatSelect = document.getElementById('format-select');
  const form = document.getElementById('diff-form');
  const statusEl = document.getElementById('status');
  const loadingEl = document.getElementById('loading');
  const errorEl = document.getElementById('error');
  const emptyEl = document.getElementById('empty');
  const outputEl = document.getElementById('diff-output');

  function show(el) { el.classList.remove('hidden'); el.classList.add('flex'); }
  function hide(el) { el.classList.add('hidden'); el.classList.remove('flex'); }
  function setError(msg) {
    errorEl.textContent = msg;
    errorEl.classList.remove('hidden');
  }
  function clearError() { errorEl.classList.add('hidden'); errorEl.textContent = ''; }
  function clearOutput() { outputEl.innerHTML = ''; emptyEl.classList.add('hidden'); }

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

  async function compare(params) {
    clearError();
    clearOutput();
    show(loadingEl);
    statusEl.textContent = '';
    const qs = new URLSearchParams({
      repo: params.repo,
      base: params.base,
      head: params.head,
      mode: params.mode,
    });
    let resp;
    try {
      resp = await fetch(`/api/git/diff?${qs.toString()}`);
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

    if (resp.status === 413) {
      const d = data.detail || {};
      const sz = d.size_bytes ?? '?';
      const lim = d.limit_bytes ?? '?';
      setError(`Diff too large: ${sz} bytes (limit ${lim} bytes). Narrow the range and retry.`);
      return;
    }
    if (!resp.ok) {
      const detail = (data && (data.detail?.message || data.detail)) || `HTTP ${resp.status}`;
      setError(typeof detail === 'string' ? detail : JSON.stringify(detail));
      return;
    }

    const diffText = data.diff || '';
    const sep = data.mode === 'three-dot' ? '...' : '..';
    statusEl.textContent = `${data.size_bytes.toLocaleString()} bytes · ${data.mode} · ${data.base}${sep}${data.head}`;
    if (!diffText.trim()) {
      emptyEl.classList.remove('hidden');
      return;
    }

    const ui = new Diff2HtmlUI(outputEl, diffText, {
      drawFileList: true,
      matching: 'lines',
      outputFormat: params.outputFormat,
    });
    ui.draw();
    ui.highlightCode();
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
