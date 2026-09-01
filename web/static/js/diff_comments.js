// Line-anchored review comments for the /diff page.
(() => {
  const PREFIX = 'cbdc';
  const AUTH_MESSAGE = 'log in to comment';
  const outputEl = document.getElementById('diff-output');

  let currentComparison = window.__cbdiffPage.getComparison();
  let batchContext = null;
  let pending = [];
  let editorRow = null;
  let editorSpacerRow = null;
  let editorResizeObserver = null;
  let editorBody = null;
  let drag = null;
  let sending = false;
  let toast = null;

  let querySessionId = new URLSearchParams(window.location.search).get('session');
  querySessionId = querySessionId ? querySessionId.trim() : null;
  let querySessionStatus = querySessionId ? 'loading' : 'none';
  let dropdownSessionId = null;
  let sessions = [];
  let sessionLoadError = '';
  let sessionNotice = '';

  let tray;
  let trayHeader;
  let trayList;
  let targetSelect;
  let noticeEl;
  let overallInput;
  let reasonEl;
  let clearButton;
  let sendButton;

  window.__cbdcBuildBatchMessage = buildBatchMessage;
  window.__cbdcResolveTargetSession = resolveTargetSession;
  window.__cbdcMostRecentSessionId = mostRecentSessionId;

  installStyles();
  installTray();
  installListeners();
  initializeSessions().catch((error) => {
    console.error('Diff comment session loading failed:', error);
    sessionLoadError = error.message;
    querySessionStatus = 'error';
    refreshTray();
  });

  function resolveTargetSession(queryId, selectedId, queryStatus) {
    if (queryId && queryStatus === 'found') return queryId;
    if (queryId && queryStatus !== 'missing') return null;
    return selectedId || null;
  }

  function mostRecentSessionId(items) {
    if (items.length === 0) return null;
    return items.slice().sort((a, b) => Date.parse(b.updated_at) - Date.parse(a.updated_at))[0].id;
  }

  function appendQuote(lines, quote) {
    const parts = String(quote).slice(0, 400).split('\n');
    if (parts.length === 1) {
      lines.push(`   ▸ "${parts[0]}"`);
      return;
    }
    lines.push(`   ▸ "${parts[0]}`);
    for (let i = 1; i < parts.length - 1; i++) lines.push(`      ${parts[i]}`);
    lines.push(`      ${parts[parts.length - 1]}"`);
  }

  function buildBatchMessage(context, entries, overall) {
    const ordered = entries.slice().sort((a, b) => {
      const pathOrder = a.filePath.localeCompare(b.filePath);
      if (pathOrder !== 0) return pathOrder;
      return a.startLine - b.startLine;
    });
    const lines = [
      `[Diff comments · ${context.repo} · ${context.base}..${context.head} @ ${context.headSha.slice(0, 7)}] ` +
        `(${ordered.length})`,
    ];
    const overallText = overall.trim();
    if (overallText) lines.push(`overall: ${overallText}`);
    lines.push('');

    for (let i = 0; i < ordered.length; i++) {
      const entry = ordered[i];
      const range = entry.endLine === entry.startLine ? `${entry.startLine}` : `${entry.startLine}-${entry.endLine}`;
      const suggestion = entry.isSuggestion ? ' [suggestion]' : '';
      lines.push(`${i + 1}. ${entry.filePath}:${range} (${entry.side})${suggestion}`);
      appendQuote(lines, entry.quote);
      const commentLines = entry.comment.split('\n');
      if (entry.isSuggestion) {
        lines.push('   ↳ suggested replacement:');
        for (const line of commentLines) lines.push(`      ${line}`);
      } else {
        lines.push(`   ↳ ${commentLines[0]}`);
        for (let j = 1; j < commentLines.length; j++) lines.push(`      ${commentLines[j]}`);
      }
      if (i < ordered.length - 1) lines.push('');
    }
    return lines.join('\n');
  }

  function installStyles() {
    const style = document.createElement('style');
    style.textContent = `
      .${PREFIX}-add {
        position: absolute; left: 2px; top: 50%; transform: translateY(-50%); z-index: 2;
        width: 18px; height: 18px; border: 0; border-radius: 4px; padding: 0;
        background: #2563eb; color: white; font: bold 14px/18px system-ui, sans-serif;
        cursor: pointer; opacity: 0; transition: opacity .1s;
      }
      .${PREFIX}-line:hover .${PREFIX}-add, .${PREFIX}-add:focus { opacity: 1; }
      .${PREFIX}-marked > td { box-shadow: inset 3px 0 #2563eb; }
      .${PREFIX}-selected > td { background: #dbeafe !important; }
      .${PREFIX}-editor-row > td { padding: 0 !important; background: #f8fafc !important; }
      .${PREFIX}-editor-spacer > td { padding: 0 !important; }
      .${PREFIX}-editor {
        box-sizing: border-box; position: sticky; left: 0;
        display: flex; flex-direction: column; gap: 8px; padding: 10px;
        border-top: 1px solid #93c5fd; border-bottom: 1px solid #93c5fd;
      }
      .${PREFIX}-editor textarea, .${PREFIX}-tray textarea {
        box-sizing: border-box; width: 100%; resize: vertical; border: 1px solid #cbd5e1;
        border-radius: 6px; padding: 7px 8px; color: #0f172a; background: white;
        font: 12px/1.45 system-ui, sans-serif; outline: none;
      }
      .${PREFIX}-editor textarea { min-height: 86px; font-family: ui-monospace, SFMono-Regular, Menlo, monospace; }
      .${PREFIX}-editor textarea:focus, .${PREFIX}-tray textarea:focus {
        border-color: #2563eb; box-shadow: 0 0 0 2px rgba(37, 99, 235, .15);
      }
      .${PREFIX}-editor-footer, .${PREFIX}-tray-actions {
        display: flex; align-items: center; justify-content: flex-end; gap: 7px;
      }
      .${PREFIX}-suggestion { display: flex; align-items: center; gap: 5px; margin-right: auto; font-size: 12px; }
      .${PREFIX}-editor button, .${PREFIX}-tray button {
        border: 1px solid #cbd5e1; border-radius: 6px; padding: 5px 9px;
        background: white; color: #334155; font: 12px system-ui, sans-serif; cursor: pointer;
      }
      .${PREFIX}-editor .${PREFIX}-primary, .${PREFIX}-tray .${PREFIX}-send {
        border-color: #2563eb; background: #2563eb; color: white;
      }
      .${PREFIX}-tray button:disabled { cursor: not-allowed; opacity: .55; }
      .${PREFIX}-tray {
        position: fixed; right: 14px; bottom: 14px; z-index: 20;
        width: min(360px, calc(100vw - 28px)); max-height: min(680px, calc(100vh - 28px));
        display: none; flex-direction: column; gap: 8px; overflow: hidden;
        border: 1px solid #cbd5e1; border-radius: 9px; padding: 10px;
        background: white; color: #0f172a; box-shadow: 0 18px 50px rgba(15, 23, 42, .22);
        font: 12px/1.4 system-ui, sans-serif;
      }
      .${PREFIX}-tray-header { font-weight: 650; color: #334155; }
      .${PREFIX}-target { display: flex; flex-direction: column; gap: 4px; }
      .${PREFIX}-target select {
        width: 100%; border: 1px solid #cbd5e1; border-radius: 6px; padding: 5px 7px;
        background: white; color: #0f172a; font: 12px system-ui, sans-serif;
      }
      .${PREFIX}-notice { display: none; padding: 6px 7px; border-radius: 6px; background: #fff7ed; color: #9a3412; }
      .${PREFIX}-overall { display: flex; flex-direction: column; gap: 4px; color: #475569; }
      .${PREFIX}-overall textarea { min-height: 54px; }
      .${PREFIX}-list { display: flex; flex-direction: column; gap: 6px; overflow: auto; }
      .${PREFIX}-item {
        display: flex; align-items: flex-start; gap: 7px; border: 1px solid #e2e8f0;
        border-radius: 6px; padding: 7px;
      }
      .${PREFIX}-item-body { flex: 1; min-width: 0; }
      .${PREFIX}-quote {
        overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: #64748b; font-style: italic;
      }
      .${PREFIX}-preview {
        display: -webkit-box; overflow: hidden; margin-top: 3px; overflow-wrap: anywhere;
        color: #0f172a; white-space: pre-line; -webkit-box-orient: vertical; -webkit-line-clamp: 3;
      }
      .${PREFIX}-item-controls { display: flex; flex-direction: column; gap: 4px; }
      .${PREFIX}-item-controls button { padding: 3px 6px; font-size: 10px; }
      .${PREFIX}-edit { min-height: 58px; font-family: ui-monospace, SFMono-Regular, Menlo, monospace !important; }
      .${PREFIX}-reason { display: none; padding: 6px 7px; border-radius: 6px; background: #fef2f2; color: #b91c1c; }
      .${PREFIX}-toast {
        position: fixed; left: 50%; bottom: 18px; z-index: 30; transform: translateX(-50%);
        max-width: min(400px, calc(100vw - 24px)); border-radius: 999px; padding: 8px 12px;
        background: #166534; color: white; box-shadow: 0 10px 30px rgba(15, 23, 42, .25);
        font: 12px system-ui, sans-serif;
      }
      .${PREFIX}-toast-error { border-radius: 7px; background: #b91c1c; }
    `;
    document.head.appendChild(style);
  }

  function installListeners() {
    document.addEventListener('cbdiff:file-rendered', (event) => {
      decorateFile(event.detail.body, event.detail.file);
    });
    document.addEventListener('cbdiff:file-collapsed', (event) => {
      if (editorBody === event.detail.body) closeEditor();
    });
    window.addEventListener('cbdiff:compared', (event) => {
      currentComparison = event.detail.comparison;
    });
    window.addEventListener('cbdiff:before-compare', handleBeforeCompare);
    document.addEventListener('mousedown', startRangeDrag);
    document.addEventListener('mouseover', continueRangeDrag);
    document.addEventListener('mouseup', finishRangeDrag);
  }

  function handleBeforeCompare(event) {
    closeEditor();
    if (pending.length === 0) return;
    const params = event.detail.params;
    const changed = ['repo', 'base', 'head', 'mode'].some((key) => params[key] !== batchContext[key]);
    if (!changed) return;
    if (!window.confirm(`discard ${pending.length} pending comments?`)) {
      event.preventDefault();
      return;
    }
    clearAll();
  }

  function hunkHeadersByRow(table) {
    const headers = [];
    let current = '';
    const rows = table.querySelectorAll('tbody > tr');
    rows.forEach((row, index) => {
      const info = row.querySelector('.d2h-info .d2h-code-side-line');
      const text = info ? info.textContent.trim() : '';
      if (text.startsWith('@@')) current = text;
      headers[index] = current;
    });
    return headers;
  }

  function decorateFile(body, file) {
    body.dataset.cbdcFilePath = file.path;
    body.__cbdcFile = file;
    const sideDiffs = body.querySelectorAll('.d2h-file-side-diff');
    if (sideDiffs.length > 0) {
      const oldTable = sideDiffs[0].querySelector('table');
      const headers = hunkHeadersByRow(oldTable);
      sideDiffs.forEach((sideDiff, index) => {
        decorateSideTable(sideDiff.querySelector('table'), index === 0 ? 'old' : 'new', headers);
      });
    } else {
      body.querySelectorAll('.d2h-diff-table').forEach(decorateUnifiedTable);
    }
    refreshMarkers(file.path);
  }

  function decorateSideTable(table, side, headers) {
    let currentHunk = '';
    table.querySelectorAll('tbody > tr').forEach((row, index) => {
      const info = row.querySelector('.d2h-info .d2h-code-side-line');
      const infoText = info ? info.textContent.trim() : '';
      if (infoText.startsWith('@@')) currentHunk = infoText;
      if (headers[index]) currentHunk = headers[index];

      const numberCell = row.querySelector('.d2h-code-side-linenumber');
      if (!numberCell || numberCell.classList.contains('d2h-info') ||
          numberCell.classList.contains('d2h-emptyplaceholder')) return;
      const line = numberCell.textContent.trim();
      if (!line) return;
      decorateLine(row, numberCell, side, line, '', '', currentHunk);
    });
  }

  function decorateUnifiedTable(table) {
    let currentHunk = '';
    table.querySelectorAll('tbody > tr').forEach((row) => {
      const info = row.querySelector('.d2h-info .d2h-code-line');
      const infoText = info ? info.textContent.trim() : '';
      if (infoText.startsWith('@@')) currentHunk = infoText;

      const numberCell = row.querySelector('.d2h-code-linenumber');
      if (!numberCell || numberCell.classList.contains('d2h-info')) return;
      const oldLine = numberCell.querySelector('.line-num1').textContent.trim();
      const newLine = numberCell.querySelector('.line-num2').textContent.trim();
      if (!oldLine && !newLine) return;
      decorateLine(row, numberCell, '', '', oldLine, newLine, currentHunk);
    });
  }

  function decorateLine(row, numberCell, side, line, oldLine, newLine, hunkHeader) {
    row.classList.add(`${PREFIX}-line`);
    row.dataset.cbdcSide = side;
    row.dataset.cbdcLine = line;
    row.dataset.cbdcOldLine = oldLine;
    row.dataset.cbdcNewLine = newLine;
    row.dataset.cbdcHunk = hunkHeader;
    row.dataset.cbdcCode = row.querySelector('.d2h-code-line-ctn').textContent;

    const button = document.createElement('button');
    button.type = 'button';
    button.className = `${PREFIX}-add`;
    button.textContent = '+';
    button.title = 'Add a line comment';
    button.setAttribute('aria-label', 'Add a line comment');
    button.addEventListener('mousedown', (event) => {
      event.preventDefault();
      event.stopPropagation();
    });
    button.addEventListener('click', (event) => {
      event.preventDefault();
      event.stopPropagation();
      const body = row.closest('[data-cbdc-file-path]');
      openEditor(anchorForRow(row, body), row, body);
    });
    numberCell.appendChild(button);
  }

  function anchorForRow(row, body) {
    const side = row.dataset.cbdcSide || (row.dataset.cbdcNewLine ? 'new' : 'old');
    const line = Number(side === 'old' ? row.dataset.cbdcOldLine || row.dataset.cbdcLine
      : row.dataset.cbdcNewLine || row.dataset.cbdcLine);
    const file = body.__cbdcFile;
    const anchor = {
      filePath: file.path,
      side,
      startLine: line,
      endLine: line,
      hunkHeader: row.dataset.cbdcHunk,
      quote: '',
      comment: '',
      isSuggestion: false,
    };
    if (file.old_path) anchor.oldPath = file.old_path;
    return anchor;
  }

  function lineForSide(row, side) {
    const value = side === 'old' ? row.dataset.cbdcOldLine || row.dataset.cbdcLine
      : row.dataset.cbdcNewLine || row.dataset.cbdcLine;
    return value ? Number(value) : null;
  }

  function rowMatchesAnchor(row, anchor) {
    if (row.dataset.cbdcHunk !== anchor.hunkHeader) return false;
    if (row.dataset.cbdcSide && row.dataset.cbdcSide !== anchor.side) return false;
    const line = lineForSide(row, anchor.side);
    return line !== null && line >= anchor.startLine && line <= anchor.endLine;
  }

  function quoteForAnchor(body, anchor) {
    return Array.from(body.querySelectorAll(`.${PREFIX}-line`))
      .filter((row) => rowMatchesAnchor(row, anchor))
      .sort((a, b) => lineForSide(a, anchor.side) - lineForSide(b, anchor.side))
      .map((row) => row.dataset.cbdcCode)
      .join('\n')
      .slice(0, 400);
  }

  function findAnchorRow(body, anchor, line) {
    return Array.from(body.querySelectorAll(`.${PREFIX}-line`)).find((row) => (
      row.dataset.cbdcHunk === anchor.hunkHeader &&
      (!row.dataset.cbdcSide || row.dataset.cbdcSide === anchor.side) &&
      lineForSide(row, anchor.side) === line
    ));
  }

  function openEditor(anchor, row, body) {
    closeEditor();
    const targetRow = findAnchorRow(body, anchor, anchor.endLine) || row;
    const tr = document.createElement('tr');
    tr.className = `${PREFIX}-editor-row`;
    const td = document.createElement('td');
    td.colSpan = targetRow.children.length;
    const editor = document.createElement('div');
    editor.className = `${PREFIX}-editor`;
    const scrollPane = targetRow.closest('.d2h-file-side-diff, .d2h-file-diff');
    editor.style.width = `${scrollPane.clientWidth - 20}px`;
    const textarea = document.createElement('textarea');
    textarea.placeholder = 'Add a comment (Ctrl+Enter)';
    const footer = document.createElement('div');
    footer.className = `${PREFIX}-editor-footer`;
    const suggestionLabel = document.createElement('label');
    suggestionLabel.className = `${PREFIX}-suggestion`;
    const suggestionInput = document.createElement('input');
    suggestionInput.type = 'checkbox';
    suggestionLabel.append(suggestionInput, document.createTextNode('Suggestion'));
    const cancelButton = document.createElement('button');
    cancelButton.type = 'button';
    cancelButton.textContent = 'Cancel';
    const addButton = document.createElement('button');
    addButton.type = 'button';
    addButton.className = `${PREFIX}-primary`;
    addButton.textContent = 'Add';
    footer.append(suggestionLabel, cancelButton, addButton);
    editor.append(textarea, footer);
    td.appendChild(editor);
    tr.appendChild(td);
    targetRow.parentNode.insertBefore(tr, targetRow.nextSibling);
    editorRow = tr;
    editorBody = body;

    const sideDiff = targetRow.closest('.d2h-file-side-diff');
    if (sideDiff) {
      const sideDiffs = sideDiff.parentNode.querySelectorAll('.d2h-file-side-diff');
      const otherSideDiff = sideDiffs[sideDiff === sideDiffs[0] ? 1 : 0];
      const otherTargetRow = otherSideDiff.querySelector('tbody').children[targetRow.sectionRowIndex];
      const spacer = document.createElement('tr');
      spacer.className = `${PREFIX}-editor-spacer`;
      const spacerCell = document.createElement('td');
      spacerCell.colSpan = otherTargetRow.children.length;
      spacer.appendChild(spacerCell);
      otherTargetRow.parentNode.insertBefore(spacer, otherTargetRow.nextSibling);
      editorSpacerRow = spacer;
      editorResizeObserver = new ResizeObserver(() => {
        spacer.style.height = `${tr.getBoundingClientRect().height}px`;
      });
      editorResizeObserver.observe(editor);
    }

    const submit = () => {
      if (!textarea.value.trim()) {
        textarea.focus();
        return;
      }
      const isSuggestion = suggestionInput.checked;
      pending.push({
        ...anchor,
        quote: quoteForAnchor(body, anchor),
        comment: isSuggestion ? textarea.value : textarea.value.trim(),
        isSuggestion,
      });
      if (!batchContext) batchContext = {...currentComparison};
      closeEditor();
      refreshMarkers(anchor.filePath);
      refreshTray();
    };
    bindEditorKeys(textarea, closeEditor, submit);
    cancelButton.addEventListener('click', closeEditor);
    addButton.addEventListener('click', submit);
    textarea.focus();
  }

  function closeEditor() {
    if (editorResizeObserver) editorResizeObserver.disconnect();
    if (editorSpacerRow) editorSpacerRow.remove();
    if (editorRow) editorRow.remove();
    editorResizeObserver = null;
    editorSpacerRow = null;
    editorRow = null;
    editorBody = null;
  }

  // Every inline comment editor shares one key contract: Escape cancels,
  // Ctrl/Meta+Enter submits, and both keys consume the event.
  function bindEditorKeys(textarea, onCancel, onSubmit) {
    textarea.addEventListener('keydown', (event) => {
      if (event.key === 'Escape') {
        event.preventDefault();
        onCancel();
      } else if ((event.ctrlKey || event.metaKey) && event.key === 'Enter') {
        event.preventDefault();
        onSubmit();
      }
    });
  }

  function dragTargetFromEvent(event) {
    const cell = event.target.closest('.d2h-code-linenumber, .d2h-code-side-linenumber');
    if (!cell) return null;
    const row = cell.closest(`.${PREFIX}-line`);
    if (!row) return null;
    const body = row.closest('[data-cbdc-file-path]');
    return { body, anchor: anchorForRow(row, body) };
  }

  function startRangeDrag(event) {
    if (event.target.closest(`.${PREFIX}-add`)) return;
    const target = dragTargetFromEvent(event);
    if (!target) return;
    drag = { body: target.body, start: target.anchor, current: target.anchor, moved: false, cancelled: false };
    event.preventDefault();
  }

  function continueRangeDrag(event) {
    if (!drag) return;
    const target = dragTargetFromEvent(event);
    if (!target) return;
    const { body, anchor } = target;
    if (body !== drag.body || anchor.side !== drag.start.side || anchor.hunkHeader !== drag.start.hunkHeader) {
      drag.cancelled = true;
      clearRangeSelection();
      return;
    }
    if (anchor.startLine !== drag.start.startLine) drag.moved = true;
    drag.current = anchor;
    showRangeSelection();
  }

  function showRangeSelection() {
    clearRangeSelection();
    if (drag.cancelled) return;
    const startLine = Math.min(drag.start.startLine, drag.current.startLine);
    const endLine = Math.max(drag.start.startLine, drag.current.startLine);
    const anchor = {...drag.start, startLine, endLine};
    drag.body.querySelectorAll(`.${PREFIX}-line`).forEach((row) => {
      if (rowMatchesAnchor(row, anchor)) row.classList.add(`${PREFIX}-selected`);
    });
  }

  function clearRangeSelection() {
    outputEl.querySelectorAll(`.${PREFIX}-selected`).forEach((row) => row.classList.remove(`${PREFIX}-selected`));
  }

  function finishRangeDrag() {
    if (!drag) return;
    const completed = drag;
    drag = null;
    clearRangeSelection();
    if (!completed.moved || completed.cancelled) return;
    const startLine = Math.min(completed.start.startLine, completed.current.startLine);
    const endLine = Math.max(completed.start.startLine, completed.current.startLine);
    const anchor = {...completed.start, startLine, endLine};
    const row = findAnchorRow(completed.body, anchor, endLine);
    openEditor(anchor, row, completed.body);
  }

  function refreshMarkers(filePath) {
    outputEl.querySelectorAll('[data-cbdc-file-path]').forEach((body) => {
      if (body.dataset.cbdcFilePath !== filePath) return;
      body.querySelectorAll(`.${PREFIX}-marked`).forEach((row) => row.classList.remove(`${PREFIX}-marked`));
      pending.filter((entry) => entry.filePath === filePath).forEach((entry) => {
        body.querySelectorAll(`.${PREFIX}-line`).forEach((row) => {
          if (rowMatchesAnchor(row, entry)) row.classList.add(`${PREFIX}-marked`);
        });
      });
    });
  }

  function refreshAllMarkers() {
    const paths = new Set(Array.from(outputEl.querySelectorAll('[data-cbdc-file-path]'))
      .map((body) => body.dataset.cbdcFilePath));
    paths.forEach(refreshMarkers);
  }

  function installTray() {
    tray = document.createElement('aside');
    tray.className = `${PREFIX}-tray`;
    trayHeader = document.createElement('div');
    trayHeader.className = `${PREFIX}-tray-header`;

    const target = document.createElement('label');
    target.className = `${PREFIX}-target`;
    target.appendChild(document.createTextNode('Target session'));
    targetSelect = document.createElement('select');
    targetSelect.addEventListener('change', () => {
      querySessionId = null;
      querySessionStatus = 'none';
      dropdownSessionId = targetSelect.value || null;
      refreshTray();
    });
    target.appendChild(targetSelect);

    noticeEl = document.createElement('div');
    noticeEl.className = `${PREFIX}-notice`;
    const overall = document.createElement('label');
    overall.className = `${PREFIX}-overall`;
    overall.appendChild(document.createTextNode('Overall comment'));
    overallInput = document.createElement('textarea');
    overallInput.placeholder = 'Optional batch-level comment';
    overall.appendChild(overallInput);

    trayList = document.createElement('div');
    trayList.className = `${PREFIX}-list`;
    reasonEl = document.createElement('div');
    reasonEl.className = `${PREFIX}-reason`;

    const actions = document.createElement('div');
    actions.className = `${PREFIX}-tray-actions`;
    clearButton = document.createElement('button');
    clearButton.type = 'button';
    clearButton.textContent = 'Clear';
    clearButton.addEventListener('click', clearAll);
    sendButton = document.createElement('button');
    sendButton.type = 'button';
    sendButton.className = `${PREFIX}-send`;
    sendButton.addEventListener('click', sendBatch);
    actions.append(clearButton, sendButton);

    tray.append(trayHeader, target, noticeEl, overall, trayList, reasonEl, actions);
    document.body.appendChild(tray);
    refreshTray();
  }

  async function fetchSessions() {
    const response = await fetch('/api/sessions/', { credentials: 'same-origin' });
    if (!response.ok) throw new Error(`Session list fetch failed: HTTP ${response.status}`);
    return response.json();
  }

  async function fetchQuerySession(id) {
    const response = await fetch(`/api/sessions/${encodeURIComponent(id)}`, { credentials: 'same-origin' });
    if (response.status === 404) return { status: 'missing', session: null };
    if (!response.ok) throw new Error(`Session fetch failed: HTTP ${response.status}`);
    return { status: 'found', session: await response.json() };
  }

  async function initializeSessions() {
    const [listed, queryResult] = await Promise.all([
      fetchSessions(),
      querySessionId ? fetchQuerySession(querySessionId) : Promise.resolve(null),
    ]);
    sessions = listed.slice();
    dropdownSessionId = mostRecentSessionId(sessions);

    if (queryResult && queryResult.status === 'found') {
      querySessionStatus = 'found';
      if (!sessions.some((session) => session.id === queryResult.session.id)) sessions.push(queryResult.session);
    } else if (queryResult && queryResult.status === 'missing') {
      querySessionStatus = 'missing';
      sessionNotice = `Target session ${querySessionId} was not found. Choose another session.`;
    }
    populateSessionSelect();
    refreshTray();
  }

  function populateSessionSelect() {
    targetSelect.innerHTML = '';
    sessions.forEach((session) => {
      const option = document.createElement('option');
      option.value = session.id;
      option.textContent = session.name;
      targetSelect.appendChild(option);
    });
    const targetId = resolveTargetSession(querySessionId, dropdownSessionId, querySessionStatus);
    if (targetId) targetSelect.value = targetId;
    targetSelect.disabled = sessions.length === 0;
  }

  function targetSession() {
    const id = resolveTargetSession(querySessionId, dropdownSessionId, querySessionStatus);
    return id ? sessions.find((session) => session.id === id) : null;
  }

  function targetReason(target) {
    if (target) return '';
    if (sessionLoadError) return `Unable to load target sessions: ${sessionLoadError}`;
    if (querySessionStatus === 'loading') return 'Loading target session…';
    return 'No target session is available.';
  }

  function buildTrayItem(index, entry) {
    const item = document.createElement('div');
    item.className = `${PREFIX}-item`;
    const body = document.createElement('div');
    body.className = `${PREFIX}-item-body`;
    const quote = document.createElement('div');
    quote.className = `${PREFIX}-quote`;
    const range = entry.endLine === entry.startLine ? `${entry.startLine}` : `${entry.startLine}-${entry.endLine}`;
    quote.textContent = `${entry.filePath}:${range} (${entry.side}) · “${entry.quote.slice(0, 400)}”`;
    const preview = document.createElement('div');
    preview.className = `${PREFIX}-preview`;
    preview.textContent = entry.isSuggestion ? `suggested replacement:\n${entry.comment}` : entry.comment;
    body.append(quote, preview);

    const controls = document.createElement('div');
    controls.className = `${PREFIX}-item-controls`;
    const edit = document.createElement('button');
    edit.type = 'button';
    edit.textContent = 'Edit';
    edit.addEventListener('click', () => editEntry(index, preview));
    const remove = document.createElement('button');
    remove.type = 'button';
    remove.textContent = 'Remove';
    remove.addEventListener('click', () => removeEntry(index));
    controls.append(edit, remove);
    item.append(body, controls);
    return item;
  }

  function editEntry(index, preview) {
    const entry = pending[index];
    const textarea = document.createElement('textarea');
    textarea.className = `${PREFIX}-edit`;
    textarea.value = entry.comment;
    let done = false;

    const cancel = () => {
      if (done) return;
      done = true;
      refreshTray();
    };
    const save = () => {
      if (done) return;
      if (!textarea.value.trim()) {
        cancel();
        return;
      }
      done = true;
      entry.comment = entry.isSuggestion ? textarea.value : textarea.value.trim();
      refreshTray();
    };
    bindEditorKeys(textarea, cancel, save);
    textarea.addEventListener('blur', save);
    preview.parentNode.replaceChild(textarea, preview);
    textarea.focus();
    textarea.select();
  }

  function refreshTray() {
    const target = targetSession();
    const suffix = target ? ` → ${target.name}` : '';
    trayHeader.textContent = `Pending comments (${pending.length})${suffix}`;
    trayList.innerHTML = '';
    pending.forEach((entry, index) => trayList.appendChild(buildTrayItem(index, entry)));
    tray.style.display = pending.length > 0 ? 'flex' : 'none';

    noticeEl.textContent = sessionNotice;
    noticeEl.style.display = sessionNotice ? 'block' : 'none';
    const reason = targetReason(target);
    reasonEl.textContent = reason;
    reasonEl.style.display = reason ? 'block' : 'none';
    sendButton.textContent = target ? `Send to ${target.name}` : 'Send unavailable';
    sendButton.disabled = sending || pending.length === 0 || !target;
    clearButton.disabled = sending || pending.length === 0;
    targetSelect.disabled = sending || sessions.length === 0;
  }

  function removeEntry(index) {
    const filePath = pending[index].filePath;
    pending.splice(index, 1);
    if (pending.length === 0) {
      batchContext = null;
      overallInput.value = '';
    }
    refreshMarkers(filePath);
    refreshTray();
  }

  function clearAll() {
    pending = [];
    batchContext = null;
    overallInput.value = '';
    closeEditor();
    refreshAllMarkers();
    refreshTray();
  }

  async function postChatMessage(sessionId, content) {
    const response = await fetch(`/api/chat/${encodeURIComponent(sessionId)}/message`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      credentials: 'same-origin',
      body: JSON.stringify({ content, uploaded_files: [] }),
    });
    if (response.status === 401) throw new Error(AUTH_MESSAGE);
    if (!response.ok) throw new Error(`Comment post failed: HTTP ${response.status}`);
  }

  async function sendBatch() {
    const target = targetSession();
    if (!target || pending.length === 0) return;
    sending = true;
    refreshTray();
    const count = pending.length;
    try {
      await postChatMessage(target.id, buildBatchMessage(batchContext, pending, overallInput.value));
      sending = false;
      clearAll();
      showToast(`${count} comments sent to ${target.name}`, false);
    } catch (error) {
      console.error('Diff comment batch send failed:', error);
      sending = false;
      refreshTray();
      showToast(error.message, true);
    }
  }

  function showToast(message, isError) {
    if (toast) toast.remove();
    const node = document.createElement('div');
    node.className = `${PREFIX}-toast${isError ? ` ${PREFIX}-toast-error` : ''}`;
    node.textContent = message;
    document.body.appendChild(node);
    toast = node;
    window.setTimeout(() => {
      node.remove();
      if (toast === node) toast = null;
    }, 2400);
  }
})();
