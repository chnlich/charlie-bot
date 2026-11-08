// /diff page: file-level lazy-loading diff2html renderer.
//
// Compare fetches a cheap per-file manifest and renders one collapsed row per file.
// Expanding a row fetches and renders that single file's diff on demand. A huge
// cross-branch diff never lands in one payload, and only expanded files are in the DOM.
(() => {
  const repoInput = document.getElementById('repo-input');
  const repoList = document.getElementById('repo-list');
  const baseInput = document.getElementById('base-input');
  const headInput = document.getElementById('head-input');
  const branchesList = document.getElementById('branches-list');
  const modeSelect = document.getElementById('mode-select');
  const formatSelect = document.getElementById('format-select');
  const form = document.getElementById('diff-form');
  const startEl = document.getElementById('start');
  const startReposEl = document.getElementById('start-repos');
  const startRepoListEl = document.getElementById('start-repo-list');
  const summaryEl = document.getElementById('summary');
  const loadingEl = document.getElementById('loading');
  const errorEl = document.getElementById('error');
  const emptyEl = document.getElementById('empty');
  const outputEl = document.getElementById('diff-output');

  // Bumped on every Compare so stale per-file fetches don't render into a newer view.
  let comparisonToken = 0;
  let comparison = null;

  const STATUS_LABELS = { A: 'added', M: 'modified', D: 'deleted', R: 'renamed', C: 'copied', T: 'type changed' };
  const STATUS_CLASSES = {
    A: 'bg-green-100 text-green-700',
    M: 'bg-amber-100 text-amber-700',
    D: 'bg-red-100 text-red-700',
    R: 'bg-blue-100 text-blue-700',
    C: 'bg-blue-100 text-blue-700',
    T: 'bg-purple-100 text-purple-700',
  };

  // The added-line figures are body text on the row's slate-50 and the summary's white. green-700 holds 4.9:1 and
  // 5.0:1 there; green-600 held 3.2:1 and 3.3:1, under the 4.5:1 floor for text this size.
  const ADDED_COUNT_CLASS = 'text-green-700';

  function show(el) { el.classList.remove('hidden'); el.classList.add('flex'); }
  function hide(el) { el.classList.add('hidden'); el.classList.remove('flex'); }
  function setError(msg) {
    errorEl.textContent = msg;
    errorEl.classList.remove('hidden');
  }
  function clearError() { errorEl.classList.add('hidden'); errorEl.textContent = ''; }
  function clearOutput() {
    outputEl.innerHTML = '';
    emptyEl.classList.add('hidden');
    summaryEl.innerHTML = '';
    summaryEl.classList.add('hidden');
  }
  function responseDetail(data, status) {
    const detail = (data && (data.detail?.message || data.detail)) || `HTTP ${status}`;
    return typeof detail === 'string' ? detail : JSON.stringify(detail);
  }

  // The one error shape for the page-level fetches: a network failure or a
  // non-JSON body reports through setError after reset() restores the page's
  // idle chrome. The ok-check stays at the call site.
  async function fetchJson(url, reset) {
    let resp;
    try {
      resp = await fetch(url);
    } catch (e) {
      reset();
      setError(`Network error: ${e.message}`);
      return null;
    }

    let data = null;
    try {
      data = await resp.json();
    } catch (e) {
      reset();
      setError(`Server returned non-JSON response (HTTP ${resp.status})`);
      return null;
    }
    return { resp, data };
  }

  // A phone shows one code column: side-by-side halves the width and cuts the changed
  // line, while line-by-line keeps the whole line on the width the phone has. A wider
  // window starts side-by-side, the Format select still offers either, and a URL's
  // outputFormat wins over this default.
  function defaultOutputFormat() {
    return window.matchMedia('(max-width: 768px)').matches ? 'line-by-line' : 'side-by-side';
  }

  // The refs the form starts with. They are the fields' values rather than
  // their hints, so Compare acts on the refs the reader sees.
  const DEFAULT_BASE = 'main';
  const DEFAULT_HEAD = 'HEAD';

  function readQuery() {
    const p = new URLSearchParams(location.search);
    return {
      repo: p.get('repo') || '',
      base: p.get('base') || DEFAULT_BASE,
      head: p.get('head') || DEFAULT_HEAD,
      // Only a URL that names all three refs asks for its own comparison; the
      // defaults fill the form without starting one.
      named: Boolean(p.get('repo') && p.get('base') && p.get('head')),
      mode: p.get('mode') || 'three-dot',
      outputFormat: p.get('outputFormat') || defaultOutputFormat(),
      session: p.get('session') || '',
    };
  }

  function writeQuery({ repo, base, head, mode, outputFormat }) {
    const p = new URLSearchParams(location.search);
    for (const key of ['repo', 'base', 'head', 'mode', 'outputFormat']) p.delete(key);
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
      startRepoListEl.innerHTML = '';
      for (const repo of repos) {
        const opt = document.createElement('option');
        opt.value = repo.path;
        opt.label = repo.label;
        repoList.appendChild(opt);
        startRepoListEl.appendChild(startRepoButton(repo));
      }
      if (repos.length > 0) startReposEl.classList.remove('hidden');
    } catch (e) {
      // Free-text fallback — datalist is just a hint.
    }
  }

  // One start-guide button per repository: a click fills Repo and moves the cursor to Base.
  function startRepoButton(repo) {
    const button = document.createElement('button');
    button.type = 'button';
    button.className =
      'font-mono text-xs px-2 py-1 max-w-full truncate rounded border border-slate-300 bg-white text-slate-800 ' +
      'hover:border-blue-500 hover:text-blue-700';
    button.textContent = repo.label;
    button.title = repo.path;
    button.addEventListener('click', () => {
      repoInput.value = repo.path;
      loadBranches(repo.path);
      baseInput.focus();
    });
    return button;
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

  // Both diff endpoints take the same four comparison selectors; single-file
  // requests add path/old_path/force on top via qs.set.
  function diffQuery(params) {
    return new URLSearchParams({
      repo: params.repo,
      base: params.base,
      head: params.head,
      mode: params.mode,
    });
  }

  async function fetchFileDiff(params, path, force, oldPath) {
    const qs = diffQuery(params);
    qs.set('path', path);
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
        `${data.size_bytes.toLocaleString('en-US')} bytes — too large to render, click to load anyway`;
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
    body.dispatchEvent(new CustomEvent('cbdiff:file-rendered', {
      bubbles: true,
      detail: { body, container, file },
    }));
  }

  function buildFileRow(params, file, token) {
    const row = document.createElement('div');
    row.className = 'border border-slate-200 rounded mb-2 overflow-hidden';

    // Desktop: one line of chevron, status, counts, then the path. Phone (max-sm): a grid of two lines, the file name
    // first and the counts with the directory under it. The name spans the row, so it is never cut short.
    const header = document.createElement('button');
    header.type = 'button';
    header.className =
      'w-full flex items-center gap-3 px-3 py-2 bg-slate-50 hover:bg-slate-100 text-left ' +
      'max-sm:grid max-sm:grid-cols-[auto_auto_5.5rem_minmax(0,1fr)] max-sm:gap-x-3 max-sm:gap-y-0.5';

    const chevron = document.createElement('span');
    chevron.textContent = '▶';
    chevron.className = 'text-slate-400 text-xs shrink-0 transition-transform max-sm:row-span-2';

    const badge = statusBadge(file.status);
    badge.classList.add('max-sm:row-span-2');

    // The line counts sit in a fixed-width pair, so the + and - figures of every row line up in one column.
    const counts = document.createElement('span');
    counts.className = 'flex font-mono text-xs whitespace-nowrap shrink-0 max-sm:row-start-2 max-sm:col-start-3';
    const add = document.createElement('span');
    add.className = 'w-10 text-right ' + ADDED_COUNT_CLASS;
    add.textContent = `+${file.additions}`;
    const del = document.createElement('span');
    del.className = 'ml-1.5 w-10 text-left text-red-600';
    del.textContent = `-${file.deletions}`;
    counts.append(add, del);

    const pathEl = document.createElement('span');
    pathEl.title = file.path;
    pathEl.className = 'flex flex-1 min-w-0 font-mono text-sm whitespace-nowrap max-sm:contents';
    const slash = file.path.lastIndexOf('/');
    if (slash >= 0) {
      const dir = document.createElement('span');
      dir.textContent = file.path.slice(0, slash + 1);
      dir.className = 'min-w-0 truncate text-slate-500 max-sm:row-start-2 max-sm:col-start-4';
      pathEl.append(dir);
    }
    const name = document.createElement('span');
    name.textContent = file.path.slice(slash + 1);
    name.className =
      'shrink-0 font-semibold text-slate-900 max-sm:col-span-2 max-sm:col-start-3 ' +
      'max-sm:whitespace-normal max-sm:break-all';
    pathEl.append(name);

    header.append(chevron, badge, counts, pathEl);

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
        body.dispatchEvent(new CustomEvent('cbdiff:file-collapsed', {
          bubbles: true,
          detail: { body, file },
        }));
        return;
      }
      chevron.style.transform = 'rotate(90deg)';
      body.classList.remove('hidden');
      loadFileInto(body, params, file, token, false);
    });

    row.append(header, body);
    return row;
  }

  // The comparison's totals and range, above the file list. The form's status line stays free for Open in code-server.
  function renderSummary(data) {
    const sep = data.mode === 'three-dot' ? '...' : '..';
    const count = document.createElement('span');
    count.className = 'text-base font-semibold text-slate-800';
    count.textContent = `${data.total_files} ${data.total_files === 1 ? 'file' : 'files'} changed`;
    const add = document.createElement('span');
    add.className = 'ml-1.5 ' + ADDED_COUNT_CLASS;
    add.textContent = `+${data.total_additions.toLocaleString('en-US')}`;
    const del = document.createElement('span');
    del.className = 'ml-1.5 text-red-600';
    del.textContent = `-${data.total_deletions.toLocaleString('en-US')}`;
    count.append(add, del);
    const range = document.createElement('span');
    range.className = 'font-mono text-xs text-slate-500 break-words';
    range.textContent = `${data.base}${sep}${data.head} · ${data.mode}`;
    summaryEl.append(count, range);
    summaryEl.classList.remove('hidden');
  }

  async function compare(params) {
    clearError();
    clearOutput();
    startEl.classList.add('hidden');
    show(loadingEl);
    const token = ++comparisonToken;
    const qs = diffQuery(params);

    const fetched = await fetchJson(`/api/git/diff/files?${qs.toString()}`, () => hide(loadingEl));
    if (!fetched) return;
    const { resp, data } = fetched;

    hide(loadingEl);
    if (token !== comparisonToken) return;

    if (!resp.ok) {
      setError(responseDetail(data, resp.status));
      return;
    }

    comparison = {
      repo: params.repo,
      base: data.base,
      head: data.head,
      mode: data.mode,
      headSha: data.head_sha,
    };
    window.dispatchEvent(new CustomEvent('cbdiff:compared', { detail: { comparison } }));

    const files = data.files || [];
    renderSummary(data);

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

  // "repo", "base and head", "repo, base, and head": the fields the form shows
  // empty, so the message never names one it has a value for.
  function joinedFields(fields) {
    if (fields.length < 3) return fields.join(' and ');
    return fields.slice(0, -1).join(', ') + ', and ' + fields[fields.length - 1];
  }

  form.addEventListener('submit', (e) => {
    e.preventDefault();
    const params = currentParams();
    const empty = ['repo', 'base', 'head'].filter((field) => !params[field]);
    if (empty.length) {
      setError(`${joinedFields(empty)} ${empty.length === 1 ? 'is' : 'are'} required`);
      return;
    }
    const event = new CustomEvent('cbdiff:before-compare', { cancelable: true, detail: { params } });
    if (!window.dispatchEvent(event)) return;
    writeQuery(params);
    compare(params);
  });

  repoInput.addEventListener('change', () => {
    loadBranches(repoInput.value.trim());
  });

  window.__cbdiffPage = {
    getComparison: () => comparison,
  };

  // Init from URL.
  (async () => {
    const q = readQuery();
    repoInput.value = q.repo;
    baseInput.value = q.base;
    headInput.value = q.head;
    modeSelect.value = q.mode;
    formatSelect.value = q.outputFormat;

    const named = q.named;
    if (!named) startEl.classList.remove('hidden');
    await loadRepos();
    if (q.repo) await loadBranches(q.repo);

    if (named) {
      compare(q);
    }
  })();
})();
