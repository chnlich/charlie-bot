// ---------------------------------------------------------------------------
// Slash command popup
// ---------------------------------------------------------------------------
let slashCommands = [];
let slashPopupIdx = -1;

async function fetchSlashCommands() {
  try {
    const res = await fetch('/api/slash/commands');
    if (res.ok) slashCommands = await res.json();
  } catch (err) {
    console.error('fetchSlashCommands failed:', err);
  }
}

function handleSlashInput(el) {
  const val = el.value;
  if (val.startsWith('/') && !val.includes(' ') && val.length < 30) {
    showSlashPopup(val.slice(1));
  } else {
    hideSlashPopup();
  }
}

function selectSlashCommand(name) {
  const input = document.getElementById('msg-input');
  if (input) { input.value = '/' + name + ' '; input.focus(); autoResize(input); }
  hideSlashPopup();
}

function showSlashPopup(filter) {
  const popup = document.getElementById('slash-popup');
  if (!popup) return;
  const matches = slashCommands.filter(c => c.name.startsWith(filter));
  if (!matches.length) { hideSlashPopup(); return; }
  popup.innerHTML = matches.map((c, i) =>
    `<div class="slash-item${i === 0 ? ' active' : ''}" onclick="selectSlashCommand('${escapeHtml(c.name)}')" data-idx="${i}">` +
    `<span class="slash-name">/${escapeHtml(c.name)}</span>` +
    `<span class="slash-desc">${escapeHtml(c.description || '')}${c.args ? ' ' + escapeHtml(c.args) : ''}</span>` +
    `</div>`
  ).join('');
  popup.classList.add('visible');
  slashPopupIdx = 0;
}

function hideSlashPopup() {
  const popup = document.getElementById('slash-popup');
  if (popup) { popup.classList.remove('visible'); popup.innerHTML = ''; }
  slashPopupIdx = -1;
}

function navigateSlashPopup(direction) {
  const popup = document.getElementById('slash-popup');
  if (!popup) return;
  const items = popup.querySelectorAll('.slash-item');
  if (!items.length) return;
  items[slashPopupIdx]?.classList.remove('active');
  slashPopupIdx = (slashPopupIdx + direction + items.length) % items.length;
  const next = items[slashPopupIdx];
  next?.classList.add('active');
  next?.scrollIntoView({ block: 'nearest' });
}

async function executeSlashCommand(name, args, options = {}) {
  if (!SESSION_ID) return;
  if (uploadsInFlight > 0) {
    showToast('Please wait for uploads to finish', true);
    return;
  }
  const input = document.getElementById('msg-input');
  if (input) { input.value = ''; input.style.height = 'auto'; }
  if (DRAFT_KEY) localStorage.removeItem(DRAFT_KEY);
  const displayText = options.displayText || (args ? `/${name} ${args}` : `/${name}`);
  const uploadedFiles = Array.isArray(options.uploadedFiles) ? options.uploadedFiles : getUploadedFilesForPayload();
  const payloadFiles = uploadedFiles.map((file) => ({
    filename: file.filename,
    path: file.path,
    size: file.size,
  }));
  pendingUserMsg = true;
  try {
    const res = await fetch(`/api/slash/${SESSION_ID}/execute`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ command: name, args: args, uploaded_files: payloadFiles }),
    });
    const data = await res.json();
    if (data.error) {
      pendingUserMsg = false;
      showToast(data.error, true);
      return;
    }
    clearSentUploadedFiles(uploadedFiles.map((file) => file.id));
    appendMessage('user', displayText, false, new Date().toISOString(), payloadFiles);
    bumpCurrentSessionToTop();
    if (data.type === 'help') {
      const rows = (data.commands || []).map(c =>
        `| \`/${c.name}\` | ${escapeHtml(c.description || '')} |`
      ).join('\n');
      const md = `**Available slash commands**\n\n| Command | Description |\n|---------|-------------|\n${rows}`;
      appendMessage('assistant', md);
    } else if (data.type === 'shell_result') {
      const out = data.exit_code !== 0 && data.stderr ? data.stderr : (data.stdout || data.stderr || '(no output)');
      appendMessage('assistant', '```\n' + out + '\n```');
    } else if (data.type === 'prompt_dispatched') {
      startThinking();
    } else if (data.type === 'improve_stopped') {
      appendMessage('system', data.message || 'Improve loop will stop after current iteration');
    } else if (data.type === 'task_triggered') {
      appendMessage('system', `Scheduled task "${data.task}" triggered — session ${data.session_id.slice(0, 8)}, thread ${data.thread_id.slice(0, 8)}`);
    }
  } catch (err) {
    console.error('executeSlashCommand failed:', err);
    pendingUserMsg = false;
    showToast('Slash command failed: ' + err.message, true);
  }
}

function handleSlashPopupKey(e) {
  const popup = document.getElementById('slash-popup');
  const popupVisible = popup && popup.classList.contains('visible');
  if (!popupVisible) return false;
  if (e.key === 'ArrowDown') { e.preventDefault(); navigateSlashPopup(1); return true; }
  if (e.key === 'ArrowUp') { e.preventDefault(); navigateSlashPopup(-1); return true; }
  if (e.key === 'Tab' || e.key === 'Enter') {
    e.preventDefault();
    const active = popup.querySelector('.slash-item.active');
    if (active) {
      const nameEl = active.querySelector('.slash-name');
      const name = nameEl ? nameEl.textContent.slice(1) : '';
      selectSlashCommand(name);
    }
    return true;
  }
  if (e.key === 'Escape') { e.preventDefault(); hideSlashPopup(); return true; }
  return false;
}
