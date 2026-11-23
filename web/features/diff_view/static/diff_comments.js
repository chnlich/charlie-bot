// Line-anchored review comments for the /diff page.
(() => {
  const PREFIX = 'cbdc';
  const FILE_BODY_SELECTOR = '[data-cbdc-file-path]';
  const QUOTE_LIMIT = 400;
  // The tray's offset from the viewport's bottom edge, and the gap in px between an open editor's bottom edge and the
  // tray's top edge.
  const TRAY_BOTTOM = 14;
  const TRAY_CLEARANCE = 12;
  const outputEl = document.getElementById('diff-output');

  let currentComparison = window.__cbdiffPage.getComparison();
  let batchContext = null;
  let pending = [];
  let editorRow = null;
  let editorSpacerRow = null;
  let editorResizeObserver = null;
  let editorPaneObserver = null;
  let editorBody = null;
  let drag = null;
  let sending = false;
  const showToast = createCommentToast(PREFIX, (node) => document.body.appendChild(node), 2400);

  let querySessionId = new URLSearchParams(window.location.search).get('session');
  querySessionId = querySessionId ? querySessionId.trim() : null;
  let querySessionStatus = querySessionId ? 'loading' : 'none';
  let querySession = null;
  let dropdownSessionId = null;
  let sessions = [];
  let sessionLoadError = '';
  let sessionNotice = '';
  let pickerError = '';
  let targetLoad = null;
  let sessionListLoad = null;
  let sessionsLoaded = false;
  let pickerLoading = false;

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
  window.__cbdcLoadTarget = loadSessionsOnce;

  installStyles();
  installTray();
  installListeners();

  function resolveTargetSession(queryId, selectedId, queryStatus) {
    if (queryId && queryStatus === 'found') return queryId;
    if (queryId && queryStatus !== 'missing') return null;
    return selectedId || null;
  }

  function mostRecentSessionId(items) {
    if (items.length === 0) return null;
    // The list's rows carry updated_at as the column form's epoch minutes, and a query session's detail row as an
    // ISO string; sessionUpdatedMs (utils.js) reads both.
    return items.slice().sort((a, b) => sessionUpdatedMs(b) - sessionUpdatedMs(a))[0].id;
  }

  // The line range an entry names: one line, or its first and last.
  function lineRange(startLine, endLine) {
    return endLine === startLine ? `${startLine}` : `${startLine}-${endLine}`;
  }

  // What an entry keeps of the typed comment: a suggestion keeps it verbatim, a note trims it.
  function storedComment(isSuggestion, value) {
    return isSuggestion ? value : value.trim();
  }

  function appendQuote(lines, quote) {
    const parts = String(quote).slice(0, QUOTE_LIMIT).split('\n');
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
      const range = lineRange(entry.startLine, entry.endLine);
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
      /* A diff line's only control is the line comment button in its number cell, and a touch
         screen reveals it by a tap. The 24 px pointer floor sets the button's box, and the number
         cell takes the line's height so that box fills one row. The buttons of two neighbouring
         lines meet at their edges, so a press belongs to the line it covers. A 21 px line box draws
         the 24 px row. The number column widens by the box's own width, so a four-digit number keeps
         its whole text beside it. Side by side the width runs 4em to 5.4em; line by line it runs
         7.5em to 8.85em with 1.7em of left padding. The code's left padding follows it in both.
         Diff2html gives each side of a side-by-side view its own table, so these widths reach every
         diff table and both sides stay level line by line. */
      .d2h-diff-table .d2h-code-line,
      .d2h-diff-table .d2h-code-side-line { line-height: 21px; }
      .d2h-diff-table .d2h-code-side-line { padding-left: 5.9em; padding-right: 4.5em;
        width: calc(100% - 10.4em); }
      .d2h-diff-table .d2h-code-side-linenumber { width: 5.4em; }
      .d2h-diff-table .d2h-code-line { padding-left: 9.35em; padding-right: 8em;
        width: calc(100% - 17.35em); }
      .d2h-diff-table .d2h-code-linenumber { width: 8.85em; padding-left: 1.7em; }
      .${PREFIX}-line > .d2h-code-linenumber,
      .${PREFIX}-line > .d2h-code-side-linenumber { height: 24px; }
      .${PREFIX}-add {
        display: inline-flex; align-items: center; justify-content: center;
        position: absolute; left: 2px; top: 50%; transform: translateY(-50%); z-index: 2;
        width: 24px; height: 24px; border: 0; border-radius: 5px; padding: 0;
        background: #2563eb; color: white; font: bold 14px/24px system-ui, sans-serif;
        cursor: pointer; opacity: 0; transition: opacity .1s;
      }
      /* The box fills its cell's height and the cell clips its overflow, so the focus ring draws
         inside the box: white on the blue fill reads 4.83:1. */
      .${PREFIX}-add:focus-visible { outline: 2px solid #fff; outline-offset: -3px; }
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
      /* A field's border is the only mark of its box. slate-300 draws 1.48:1 on the tray's
         white panel and 1.42:1 on the editor row's #f8fafc. Both fall under the 3:1 floor
         WCAG 1.4.11 sets for a UI component boundary. slate-500 draws 4.76:1, the tone the
         diff form's fields keep. The focus rule below paints blue over it. */
      .${PREFIX}-editor textarea, .${PREFIX}-tray textarea {
        box-sizing: border-box; width: 100%; resize: vertical; border: 1px solid #64748b;
        border-radius: 6px; padding: 7px 8px; color: #0f172a; background: white;
        font: 12px/1.45 system-ui, sans-serif; outline: none;
      }
      .${PREFIX}-editor textarea { min-height: 86px; font-family: ui-monospace, SFMono-Regular, Menlo, monospace; }
      .${PREFIX}-editor textarea:focus, .${PREFIX}-tray textarea:focus {
        border-color: #2563eb; box-shadow: 0 0 0 2px rgba(37, 99, 235, .15);
      }
      /* In the side-by-side format a phone's half pane is 157 px wide. The suggestion toggle and the
         two buttons cannot share one row there: without the wrap the row overflows past the screen's
         left edge and the table clips the toggle. */
      .${PREFIX}-editor-footer, .${PREFIX}-tray-actions {
        display: flex; align-items: center; justify-content: flex-end; gap: 7px; flex-wrap: wrap;
      }
      /* The suggestion toggle's label is its target. Its 13 px box and 12 px text draw 18 px tall on
         their own, under the 24 px pointer floor the editor's other controls keep. */
      .${PREFIX}-suggestion { display: flex; align-items: center; gap: 5px; margin-right: auto; font-size: 12px; min-height: 24px; }
      /* A secondary button fills white, so its border is the only mark of its box. slate-300 draws
         1.42:1 on the editor row and 1.48:1 on the tray's white panel. Both fall under the 3:1
         floor WCAG 1.4.11 sets for a UI component boundary. slate-500 draws 4.55:1 and 4.76:1,
         the tone the fields in both surfaces keep. The primary and send rules below paint blue
         over it. */
      .${PREFIX}-editor button, .${PREFIX}-tray button {
        border: 1px solid #64748b; border-radius: 6px; padding: 5px 9px;
        background: white; color: #334155; font: 12px system-ui, sans-serif; cursor: pointer;
      }
      .${PREFIX}-editor .${PREFIX}-primary, .${PREFIX}-tray .${PREFIX}-send {
        border-color: #2563eb; background: #2563eb; color: white;
      }
      .${PREFIX}-tray button:disabled { cursor: not-allowed; opacity: .55; }
      /* The tray stays within half the viewport. keepEditorAboveTray scrolls an open editor up to the tray's top
         edge, so the band above the tray must hold the editor, about 150 px with its footer. A 680 px tray on an
         844 px phone leaves about 100 px below the top bar, and the editor's footer then stays under the tray. */
      .${PREFIX}-tray {
        position: fixed; right: ${TRAY_BOTTOM}px; bottom: ${TRAY_BOTTOM}px; z-index: 20;
        width: min(360px, calc(100vw - 28px)); max-height: min(680px, 50vh);
        display: none; flex-direction: column; gap: 8px; overflow: hidden;
        border: 1px solid #cbd5e1; border-radius: 9px; padding: 10px;
        background: white; color: #0f172a; box-shadow: 0 18px 50px rgba(15, 23, 42, .22);
        font: 12px/1.4 system-ui, sans-serif;
      }
      /* The header names the target session, which the select below also names. A long name
         stays on this one clamped line: wrapped, it takes more height than the tray has and
         pushes the list and actions past the tray's overflow clip. */
      /* The header's overflow clip gives it no minimum height, so under the tray's half-viewport cap the flex column
         would shrink it below its line. flex: none keeps the line; the list, which scrolls, takes the shrink. */
      .${PREFIX}-tray-header {
        flex: none; font-weight: 650; color: #334155;
        overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
      }
      .${PREFIX}-target { display: flex; flex-direction: column; gap: 4px; }
      .${PREFIX}-target select {
        width: 100%; border: 1px solid #64748b; border-radius: 6px; padding: 5px 7px;
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
      /* The tray's per-comment Edit and Remove controls take the 24 px pointer floor. At 10 px text
         and 3 px padding they draw 19 px tall on their own, under the floor WCAG 2.5.8 sets. */
      .${PREFIX}-item-controls button { padding: 3px 6px; font-size: 10px; min-height: 24px; }
      .${PREFIX}-edit { min-height: 58px; font-family: ui-monospace, SFMono-Regular, Menlo, monospace !important; }
      .${PREFIX}-reason { display: none; padding: 6px 7px; border-radius: 6px; background: #fef2f2; color: #b91c1c; }
      .${PREFIX}-reason-loading { background: none; color: #64748b; }
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
    // A file re-renders its decoration when line expansion inserts rows. A decorated row keeps its marker.
    if (row.classList.contains(`${PREFIX}-line`)) return;
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
    button.innerHTML = cbIcon('comment', 12);
    button.title = 'Add a line comment';
    button.setAttribute('aria-label', 'Add a line comment');
    button.addEventListener('mousedown', (event) => {
      event.preventDefault();
      event.stopPropagation();
    });
    button.addEventListener('click', (event) => {
      event.preventDefault();
      event.stopPropagation();
      const body = row.closest(FILE_BODY_SELECTOR);
      openEditor(anchorForRow(row, body), row, body);
    });
    numberCell.appendChild(button);
  }

  function anchorForRow(row, body) {
    const side = row.dataset.cbdcSide || (row.dataset.cbdcNewLine ? 'new' : 'old');
    const line = Number(rawLineForSide(row, side));
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

  // The row's line number on one side of the diff: that side's own cell, else the unified cell.
  function rawLineForSide(row, side) {
    return side === 'old' ? row.dataset.cbdcOldLine || row.dataset.cbdcLine
      : row.dataset.cbdcNewLine || row.dataset.cbdcLine;
  }

  function lineForSide(row, side) {
    const value = rawLineForSide(row, side);
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
      .slice(0, QUOTE_LIMIT);
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
    // The pane's width changes under an open editor. A narrowed window or a rotated phone
    // leaves a one-shot width wider than the pane's clip, which puts the footer's right-aligned
    // Cancel and Add past the screen edge. The observer keeps the box on the pane.
    const sizeEditorToPane = () => { editor.style.width = `${scrollPane.clientWidth - 20}px`; };
    sizeEditorToPane();
    editorPaneObserver = new ResizeObserver(sizeEditorToPane);
    editorPaneObserver.observe(scrollPane);
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
        comment: storedComment(isSuggestion, textarea.value),
        isSuggestion,
      });
      if (!batchContext) batchContext = {...currentComparison};
      closeEditor();
      refreshMarkers(anchor.filePath);
      loadSessionsOnce();
      refreshTray();
    };
    bindEditorKeys(textarea, closeEditor, submit);
    cancelButton.addEventListener('click', closeEditor);
    addButton.addEventListener('click', submit);
    textarea.focus();
    keepEditorAboveTray(editor);
  }

  // The tray is fixed over the lower part of the page. The focus scroll can leave the textarea at the viewport's bottom
  // edge, which puts the editor's footer under the tray. Scroll up by the overlap, so the whole editor clears the tray.
  function keepEditorAboveTray(editor) {
    if (tray.style.display === 'none') return;
    const overlap = editor.getBoundingClientRect().bottom - (tray.getBoundingClientRect().top - TRAY_CLEARANCE);
    if (overlap > 0) window.scrollBy(0, overlap);
  }

  function closeEditor() {
    if (editorResizeObserver) editorResizeObserver.disconnect();
    if (editorPaneObserver) editorPaneObserver.disconnect();
    if (editorSpacerRow) editorSpacerRow.remove();
    if (editorRow) editorRow.remove();
    editorResizeObserver = null;
    editorPaneObserver = null;
    editorSpacerRow = null;
    editorRow = null;
    editorBody = null;
  }

  function dragTargetFromEvent(event) {
    const cell = event.target.closest('.d2h-code-linenumber, .d2h-code-side-linenumber');
    if (!cell) return null;
    const row = cell.closest(`.${PREFIX}-line`);
    if (!row) return null;
    const body = row.closest(FILE_BODY_SELECTOR);
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
    outputEl.querySelectorAll(FILE_BODY_SELECTOR).forEach((body) => {
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
    const paths = new Set(Array.from(outputEl.querySelectorAll(FILE_BODY_SELECTOR))
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
    // A focus, a press or a key on the picker opens it. The first open loads the list, and a failed load is retried
    // at the next open, which a select that keeps its focus reaches only through a press or a key.
    for (const type of ['focus', 'pointerdown', 'keydown']) targetSelect.addEventListener(type, openPicker);
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

  // The target picker's rows: the sidebar's column form of the same corpus. It carries the id, name and updated_at
  // this tray reads and no other field, at about a fifth of the row array's bytes for the same sessions.
  async function fetchSessions() {
    const response = await fetch('/api/sessions/?compact=1', { credentials: 'same-origin' });
    if (!response.ok) throw new Error(`Session list fetch failed: HTTP ${response.status}`);
    return expandSessionColumns(await response.json());
  }

  async function fetchQuerySession(id) {
    const response = await fetch(`/api/sessions/${encodeURIComponent(id)}`, { credentials: 'same-origin' });
    if (response.status === 404) return { status: 'missing', session: null };
    if (!response.ok) throw new Error(`Session fetch failed: HTTP ${response.status}`);
    return { status: 'found', session: await response.json() };
  }

  // The target loads on the first pending comment: the first moment the tray needs a target. A URL that names a
  // session resolves it by its single-session route, and Send enables when that answers. The full list loads only when
  // the URL names no session, when the named session is missing, or when the picker opens. A failed load clears its
  // promise, so the next pending comment retries it.
  function loadSessionsOnce() {
    if (targetLoad) return targetLoad;
    targetLoad = resolveTarget().catch((error) => {
      console.error('Diff comment session loading failed:', error);
      sessionLoadError = error.message;
      // A failed single-session route leaves the URL's session unresolved, so the picker waits for the next pending
      // comment to retry it. A failed list keeps the status it had, so the picker stays open for a retry.
      if (querySessionStatus === 'loading') querySessionStatus = 'error';
      targetLoad = null;
      refreshTray();
    });
    return targetLoad;
  }

  async function resolveTarget() {
    if (querySessionId) {
      const queryResult = await fetchQuerySession(querySessionId);
      if (queryResult.status === 'found') {
        querySessionStatus = 'found';
        querySession = queryResult.session;
        sessions = [querySession];
        sessionLoadError = '';
        sessionsLoaded = true;
        populateSessionSelect();
        refreshTray();
        return;
      }
      querySessionStatus = 'missing';
      sessionNotice = `Target session ${querySessionId} was not found. Choose another session.`;
    }
    await loadSessionList();
    refreshTray();
  }

  // The full list loads for the target or when the picker opens, and a loaded list is kept. The URL's session row stays
  // in it, so the picker names the session that the target resolved. A failed load clears its promise, so the next
  // pending comment or picker open retries it.
  function loadSessionList() {
    if (!sessionListLoad) {
      sessionListLoad = fetchSessions().then(
        (listed) => {
          sessions = listed.slice();
          if (querySession && !sessions.some((session) => session.id === querySession.id)) sessions.push(querySession);
          if (!dropdownSessionId) dropdownSessionId = mostRecentSessionId(sessions);
          sessionsLoaded = true;
          if (querySessionStatus !== 'error') sessionLoadError = '';
          pickerError = '';
          populateSessionSelect();
        },
        (error) => {
          sessionListLoad = null;
          throw error;
        },
      );
    }
    return sessionListLoad;
  }

  // The picker's list loads when the picker opens. A failed load clears its promise, so the next open retries it.
  function openPicker() {
    if (sessionListLoad) return;
    pickerLoading = true;
    refreshTray();
    loadSessionList()
      .catch((error) => {
        console.error('Diff comment session list failed:', error);
        pickerError = `Unable to load the session list: ${error.message}`;
      })
      .finally(() => {
        pickerLoading = false;
        refreshTray();
      });
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
  }

  function targetSession() {
    const id = resolveTargetSession(querySessionId, dropdownSessionId, querySessionStatus);
    return id ? sessions.find((session) => session.id === id) : null;
  }

  // A loading reason takes the muted style; the red reason style stays with the errors.
  function targetReason(target) {
    if (target) return pickerLoading ? { text: 'Loading sessions…', loading: true } : { text: '', loading: false };
    if (sessionLoadError) return { text: `Unable to load target sessions: ${sessionLoadError}`, loading: false };
    if (querySessionStatus === 'loading' || !sessionsLoaded) return { text: 'Loading target session…', loading: true };
    return { text: 'No target session is available.', loading: false };
  }

  function buildTrayItem(index, entry) {
    const item = document.createElement('div');
    item.className = `${PREFIX}-item`;
    const body = document.createElement('div');
    body.className = `${PREFIX}-item-body`;
    const quote = document.createElement('div');
    quote.className = `${PREFIX}-quote`;
    const range = lineRange(entry.startLine, entry.endLine);
    quote.textContent = `${entry.filePath}:${range} (${entry.side}) · “${entry.quote.slice(0, QUOTE_LIMIT)}”`;
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
    swapInInlineEditor(
        textarea, preview,
        (value) => {
          entry.comment = storedComment(entry.isSuggestion, value);
        },
        refreshTray);
  }

  function refreshTray() {
    const target = targetSession();
    const suffix = target ? ` → ${target.name}` : '';
    trayHeader.textContent = `Pending comments (${pending.length})${suffix}`;
    trayList.innerHTML = '';
    pending.forEach((entry, index) => trayList.appendChild(buildTrayItem(index, entry)));
    tray.style.display = pending.length > 0 ? 'flex' : 'none';

    const notice = [sessionNotice, pickerError].filter(Boolean).join(' ');
    noticeEl.textContent = notice;
    noticeEl.style.display = notice ? 'block' : 'none';
    const reason = targetReason(target);
    reasonEl.textContent = reason.text;
    reasonEl.className = reason.loading ? `${PREFIX}-reason ${PREFIX}-reason-loading` : `${PREFIX}-reason`;
    reasonEl.style.display = reason.text ? 'block' : 'none';
    // The header and the select name the target, so the button carries the action alone: its
    // label stays inside the tray whatever the session name's length.
    sendButton.textContent = 'Send';
    sendButton.disabled = sending || pending.length === 0 || !target;
    clearButton.disabled = sending || pending.length === 0;
    // The picker waits while the URL's session is unresolved, because the URL's choice wins; otherwise it stays enabled
    // while the list is absent, so a press retries a failed list load.
    targetSelect.disabled = sending || querySessionStatus === 'loading' || querySessionStatus === 'error';
    // While the tray shows, the page extends below its last row by the tray's height and offsets. Without that
    // extension, an editor on the last rows scrolls up only as far as the page continues below it.
    const trayBand = tray.offsetHeight + TRAY_BOTTOM + TRAY_CLEARANCE;
    document.body.style.paddingBottom = pending.length > 0 ? `${trayBand}px` : '';
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

  async function sendBatch() {
    const target = targetSession();
    if (!target || pending.length === 0) return;
    sending = true;
    refreshTray();
    const count = pending.length;
    try {
      const result = await postCommentMessage(target.id, buildBatchMessage(batchContext, pending, overallInput.value));
      sending = false;
      clearAll();
      if (result.dispatch_error) showToast(result.dispatch_error, true);
      else showToast(`${count} comments sent to ${target.name}`, false);
    } catch (error) {
      console.error('Diff comment batch send failed:', error);
      sending = false;
      refreshTray();
      showToast(error.message, true);
    }
  }
})();
