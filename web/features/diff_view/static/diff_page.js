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

  // The lines one expand click reveals. A gap this size or smaller expands in one click.
  const EXPAND_STEP = 20;

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

  // The expand strips render inside diff2html's tables, so their look comes from a style block here.
  // Each cbdx- class exists only in that block, so a rename has to move its rule with the name.
  installStyles();

  function installStyles() {
    const style = document.createElement('style');
    style.textContent = `
      .cbdx-gap-row > td { padding: 0; background: #f8fafc; border-top: 1px solid #e2e8f0; border-bottom: 1px solid #e2e8f0; }
      .cbdx-gap-cell { display: flex; align-items: center; gap: 6px; padding: 2px 8px; white-space: nowrap; }
      .cbdx-expand { display: inline-flex; align-items: center; min-height: 24px; border: 1px solid #cbd5e1;
        border-radius: 6px; padding: 2px 8px; background: white; color: #334155; font: 12px system-ui, sans-serif;
        cursor: pointer; }
      .cbdx-expand:hover { border-color: #2563eb; color: #2563eb; }
      .cbdx-expand:disabled { cursor: not-allowed; opacity: .55; }
      .cbdx-whole { margin-left: auto; }
      .cbdx-gap-count { color: #64748b; font-size: 11px; }
      .cbdx-gap-error { color: #b91c1c; font-size: 12px; }
      /* A phone's line-by-line table can stand wider than the screen, where a right-aligned control would
         sit past the visible edge. Below the page's phone breakpoint the strip keeps its controls together
         at the left. A strip too wide for the screen wraps onto a second line rather than hide one. */
      @media (max-width: 768px) {
        .cbdx-gap-cell { flex-wrap: wrap; }
        .cbdx-whole { margin-left: 0; }
      }
    `;
    document.head.appendChild(style);
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
    return ui;
  }

  // The "@@ -a,b +c,d @@" headers of the diff text, in order. git omits a count of one.
  function parseHunks(diffText) {
    const hunks = [];
    for (const line of String(diffText).split('\n')) {
      const match = line.match(/^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@/);
      if (match) {
        hunks.push({
          a: Number(match[1]),
          b: match[2] === undefined ? 1 : Number(match[2]),
          c: Number(match[3]),
          d: match[4] === undefined ? 1 : Number(match[4]),
        });
      }
    }
    return hunks;
  }

  // The hidden unchanged runs around the hunks: before the first hunk, between hunks, and after the last one.
  // Each gap carries both sides' inclusive line ranges and the index of the hunk that follows it.
  // The bottom gap carries hunks.length. Both sides' ranges hold the same lines, so both have the same length.
  function hiddenGaps(hunks, baseTotal, headTotal) {
    const gaps = [];
    let oldEnd = 0;
    let newEnd = 0;
    hunks.forEach((hunk, index) => {
      const oldLast = hunk.b > 0 ? hunk.a - 1 : hunk.a;
      const newLast = hunk.d > 0 ? hunk.c - 1 : hunk.c;
      if (oldLast >= oldEnd + 1) {
        gaps.push({oldLo: oldEnd + 1, oldHi: oldLast, newLo: newEnd + 1, newHi: newLast, hunkIndex: index});
      }
      oldEnd = hunk.b > 0 ? hunk.a + hunk.b - 1 : hunk.a;
      newEnd = hunk.d > 0 ? hunk.c + hunk.d - 1 : hunk.c;
    });
    if (hunks.length > 0 && headTotal >= newEnd + 1) {
      gaps.push({oldLo: oldEnd + 1, oldHi: baseTotal, newLo: newEnd + 1, newHi: headTotal, hunkIndex: hunks.length});
    }
    return gaps;
  }

  // The new-side range one control reveals: `up` the 20 lines above the gap's bottom edge, `down` the 20 below
  // its top edge, `whole` the gap itself. A gap of EXPAND_STEP lines or fewer reveals whole in one click.
  function revealRange(gap, direction) {
    const size = gap.newHi - gap.newLo + 1;
    if (direction === 'whole' || size <= EXPAND_STEP) return [gap.newLo, gap.newHi];
    if (direction === 'up') return [gap.newHi - EXPAND_STEP + 1, gap.newHi];
    return [gap.newLo, gap.newLo + EXPAND_STEP - 1];
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

  // One side's line span of the file at its resolved revision.
  async function fetchLines(params, path, side, start, end) {
    const qs = diffQuery(params);
    qs.set('path', path);
    qs.set('side', side);
    qs.set('start', String(start));
    qs.set('end', String(end));
    const resp = await fetch(`/api/git/diff/lines?${qs.toString()}`);
    const data = await resp.json();
    if (!resp.ok) {
      throw new Error(responseDetail(data, resp.status));
    }
    return data;
  }

  // A context row in the unified format: one row carrying both sides' numbers. The markup classes are the ones
  // diff2html renders, so the diff stylesheet and the comment decoration treat a revealed row like any other
  // context row.
  function buildUnifiedRow(oldLine, newLine, text, lang) {
    const row = document.createElement('tr');
    const numberCell = document.createElement('td');
    numberCell.className = 'd2h-code-linenumber d2h-cntx';
    const oldNumber = document.createElement('div');
    oldNumber.className = 'line-num1';
    oldNumber.textContent = oldLine;
    const newNumber = document.createElement('div');
    newNumber.className = 'line-num2';
    newNumber.textContent = newLine;
    numberCell.append(oldNumber, newNumber);
    const codeCell = document.createElement('td');
    codeCell.className = 'd2h-cntx';
    codeCell.appendChild(codeLineDiv('d2h-code-line', text, lang));
    row.append(numberCell, codeCell);
    return row;
  }

  // A context row in one side-by-side table: one row carrying its own side's number and text.
  function buildSideRow(line, text, lang) {
    const row = document.createElement('tr');
    const numberCell = document.createElement('td');
    numberCell.className = 'd2h-code-side-linenumber d2h-cntx';
    numberCell.textContent = line;
    const codeCell = document.createElement('td');
    codeCell.className = 'd2h-cntx';
    codeCell.appendChild(codeLineDiv('d2h-code-side-line', text, lang));
    row.append(numberCell, codeCell);
    return row;
  }

  function codeLineDiv(className, text, lang) {
    const div = document.createElement('div');
    div.className = className;
    const prefix = document.createElement('span');
    prefix.className = 'd2h-code-line-prefix';
    prefix.textContent = ' ';
    const code = document.createElement('span');
    code.className = lang ? `d2h-code-line-ctn hljs ${lang}` : 'd2h-code-line-ctn';
    if (text === '') {
      code.appendChild(document.createElement('br'));
    } else {
      code.textContent = text;
    }
    div.append(prefix, code);
    return div;
  }

  // The expand controls of one rendered file: a strip at each hidden gap, each strip carrying its expand up,
  // expand down and whole-file buttons. Revealed rows go in place with both sides' line numbers, and the strip
  // stays at the gap's top edge while its blocks of revealed rows keep their line order.
  function installExpandControls(body, container, file, params, data, token) {
    const hunks = parseHunks(data.diff);
    const gaps = hiddenGaps(hunks, data.base_total || 0, data.head_total || 0);
    if (hunks.length === 0 || gaps.length === 0) return;
    const wrapper = container.querySelector('.d2h-file-wrapper');
    if (!wrapper) return;
    const sideDiffs = wrapper.querySelectorAll('.d2h-file-side-diff');
    const unified = sideDiffs.length === 0;
    const tables = unified
      ? [wrapper.querySelector('.d2h-diff-table')]
      : Array.from(sideDiffs, (sideDiff) => sideDiff.querySelector('table'));
    const lang = wrapper.getAttribute('data-lang') || '';
    // The read pins the revisions the served diff was built between.
    // A ref that moves mid-view cannot pair revealed lines with the hunk headers they number against.
    const lineParams = {repo: params.repo, base: data.base_sha, head: data.head_sha, mode: params.mode};

    gaps.forEach((gap) => {
      gap.blocks = [];
      gap.cells = [];
      gap.stripRows = tables.map(() => buildStripRow(gap));
      renderGap(gap);
      renderStripContent(gap);
    });

    // The row the gap's region sits in front of: the hunk that follows it, or the table's end for the bottom gap.
    function anchorFor(gap, table) {
      if (gap.hunkIndex >= hunks.length) return null;
      let seen = -1;
      for (const row of table.querySelectorAll('tr')) {
        if (row.querySelector('td.d2h-info')) {
          seen += 1;
          if (seen === gap.hunkIndex) return row;
        }
      }
      return null;
    }

    // The strip's flex content sits in a div inside the cell. A td that is itself the flex container stops
    // sizing as a table cell, so its colspan no longer reaches across the row.
    function buildStripRow(gap) {
      const row = document.createElement('tr');
      row.className = 'cbdx-gap-row';
      const cell = document.createElement('td');
      cell.colSpan = 2;
      const strip = document.createElement('div');
      strip.className = 'cbdx-gap-cell';
      cell.appendChild(strip);
      row.appendChild(cell);
      gap.cells.push(strip);
      return row;
    }

    function stripButton(gap, label, direction) {
      const button = document.createElement('button');
      button.type = 'button';
      button.className = 'cbdx-expand';
      button.textContent = label;
      button.addEventListener('click', () => {
        if (direction === 'all') revealAll();
        else revealGap(gap, direction);
      });
      return button;
    }

    // The strip's buttons: both directions for a gap that outlives one click, the gap itself otherwise, and the
    // whole file from every strip.
    function renderStripContent(gap) {
      const size = gap.newHi - gap.newLo + 1;
      gap.cells.forEach((cell) => {
        cell.innerHTML = '';
        const directionButtons = size <= EXPAND_STEP
          ? [stripButton(gap, `Expand ${size} ${size === 1 ? 'line' : 'lines'}`, 'whole')]
          : [stripButton(gap, '↑ Expand up', 'up'), stripButton(gap, '↓ Expand down', 'down')];
        const fileButton = stripButton(gap, 'Whole file', 'all');
        fileButton.className = 'cbdx-expand cbdx-whole';
        const count = document.createElement('span');
        count.className = 'cbdx-gap-count';
        count.textContent = `${size} hidden ${size === 1 ? 'line' : 'lines'}`;
        const error = document.createElement('span');
        error.className = 'cbdx-gap-error';
        cell.append(...directionButtons, fileButton, count, error);
      });
    }

    function setStripDisabled(gap, disabled) {
      gap.cells.forEach((cell) => {
        cell.querySelectorAll('button.cbdx-expand').forEach((button) => {
          button.disabled = disabled;
        });
      });
    }

    function showGapError(gap, message) {
      gap.cells.forEach((cell) => {
        const error = cell.querySelector('.cbdx-gap-error');
        if (error) error.textContent = `Failed to load lines: ${message}`;
      });
    }

    // Moves the strip and every revealed block of the gap into the region in front of its anchor, in line order.
    function renderGap(gap) {
      tables.forEach((table, index) => {
        const tbody = table.querySelector('tbody');
        const anchor = anchorFor(gap, table);
        const stripRow = gap.stripRows[index];
        if (anchor) anchor.parentNode.insertBefore(stripRow, anchor);
        else tbody.appendChild(stripRow);
        let last = stripRow;
        gap.blocks.forEach((block) => {
          block.rows[index].forEach((row) => {
            last.parentNode.insertBefore(row, last.nextSibling);
            last = row;
          });
        });
      });
    }

    async function fetchLinesPair(gap, newLo, newHi) {
      const count = newHi - newLo + 1;
      const oldLo = gap.oldLo + (newLo - gap.newLo);
      const [head, base] = await Promise.all([
        fetchLines(lineParams, file.path, 'head', newLo, newHi),
        fetchLines(lineParams, file.old_path || file.path, 'base', oldLo, oldLo + count - 1),
      ]);
      if (head.lines.length === 0 || head.lines.length !== base.lines.length) {
        throw new Error('the two sides disagree on the revealed range');
      }
      return {head, base};
    }

    function buildBlock(pair) {
      const rows = tables.map((table, index) => {
        const oldSide = !unified && index === 0;
        return pair.head.lines.map((headLine, offset) => {
          const baseLine = pair.base.lines[offset];
          return unified
            ? buildUnifiedRow(baseLine.line, headLine.line, headLine.text, lang)
            : buildSideRow(oldSide ? baseLine.line : headLine.line, oldSide ? baseLine.text : headLine.text, lang);
        });
      });
      return {newLo: pair.head.start, newHi: pair.head.end, rows};
    }

    function updateGapBounds(gap, direction, newLo, newHi) {
      const count = newHi - newLo + 1;
      if (direction === 'up') {
        gap.newHi = newLo - 1;
        gap.oldHi -= count;
      } else if (direction === 'down') {
        gap.newLo = newHi + 1;
        gap.oldLo += count;
      } else {
        gap.newLo = newHi + 1;
        gap.newHi = newLo - 1;
        gap.oldLo += count;
        gap.oldHi -= count;
      }
    }

    function refreshStrip(gap) {
      if (gap.newLo > gap.newHi) {
        gap.stripRows.forEach((row) => row.remove());
        gap.stripRows = null;
        gap.cells = [];
        return;
      }
      renderStripContent(gap);
    }

    async function revealGap(gap, direction) {
      if (gap.inFlight || !gap.stripRows) return;
      const [newLo, newHi] = revealRange(gap, direction);
      gap.inFlight = true;
      setStripDisabled(gap, true);
      let pair;
      try {
        pair = await fetchLinesPair(gap, newLo, newHi);
      } catch (e) {
        showGapError(gap, e.message);
        return;
      } finally {
        gap.inFlight = false;
        setStripDisabled(gap, false);
      }
      if (token !== comparisonToken) return;
      const block = buildBlock(pair);
      gap.blocks.push(block);
      gap.blocks.sort((first, second) => first.newLo - second.newLo);
      updateGapBounds(gap, direction, newLo, newHi);
      renderGap(gap);
      refreshStrip(gap);
      if (body.__cbdiffUi) body.__cbdiffUi.highlightCode();
      body.dispatchEvent(new CustomEvent('cbdiff:file-rendered', {
        bubbles: true,
        detail: {body, container, file},
      }));
    }

    // The whole file: every gap's remaining hidden lines, each once.
    async function revealAll() {
      for (const gap of gaps) {
        if (gap.stripRows) await revealGap(gap, 'whole');
      }
    }
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
    body.__cbdiffUi = renderFileDiff(container, diffText);
    body.dispatchEvent(new CustomEvent('cbdiff:file-rendered', {
      bubbles: true,
      detail: { body, container, file },
    }));
    installExpandControls(body, container, file, params, data, token);
  }

  function buildFileRow(params, file, token) {
    const row = document.createElement('div');
    row.className = 'border border-slate-200 rounded mb-2 overflow-hidden';

    // Desktop: one line of chevron, status, counts, then the path. A phone lays the row out as the diff page's
    // `.diff-file-row` grid. The name and the counts share its first line. A path with a directory puts it on the
    // second line, so a file at the repository root keeps one line.
    const header = document.createElement('button');
    header.type = 'button';
    header.className =
      'diff-file-row w-full flex items-center gap-3 px-3 py-2 bg-slate-50 hover:bg-slate-100 text-left';

    const chevron = document.createElement('span');
    chevron.textContent = '▶';
    chevron.className = 'diff-file-chevron text-slate-400 text-xs shrink-0 transition-transform';

    const badge = statusBadge(file.status);
    badge.classList.add('diff-file-badge');

    // The line counts sit in a fixed-width pair, so the + and - figures of every row line up in one column.
    const counts = document.createElement('span');
    counts.className = 'diff-file-counts flex font-mono text-xs whitespace-nowrap shrink-0';
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
      dir.className = 'diff-file-dir min-w-0 truncate text-slate-500';
      pathEl.append(dir);
    }
    const name = document.createElement('span');
    name.textContent = file.path.slice(slash + 1);
    name.className = 'diff-file-name shrink-0 font-semibold text-slate-900';
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
    parseHunks,
    hiddenGaps,
    revealRange,
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
