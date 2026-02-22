// ---------------------------------------------------------------------------
// Slash command popup
// ---------------------------------------------------------------------------
function isMobile() {
  return /Android|iPhone|iPad/i.test(navigator.userAgent) || ('ontouchstart' in window && window.innerWidth <= 768);
}
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

function selectSlashCommand(name) {
  const input = document.getElementById('msg-input');
  if (input) { input.value = '/' + name + ' '; input.focus(); autoResize(input); }
  hideSlashPopup();
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

async function executeSlashCommand(name, args) {
  if (!SESSION_ID) return;
  const input = document.getElementById('msg-input');
  if (input) { input.value = ''; input.style.height = 'auto'; }
  const displayText = args ? `/${name} ${args}` : `/${name}`;
  try {
    const res = await fetch(`/api/slash/${SESSION_ID}/execute`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ command: name, args: args }),
    });
    const data = await res.json();
    if (data.error) { showToast(data.error, true); return; }
    if (data.type === 'help') {
      const rows = (data.commands || []).map(c =>
        `| \`/${c.name}\` | ${escapeHtml(c.description || '')} |`
      ).join('\n');
      const md = `**Available slash commands**\n\n| Command | Description |\n|---------|-------------|\n${rows}`;
      appendMessage('assistant', md);
    } else if (data.type === 'shell_result') {
      appendMessage('user', displayText);
      const out = data.exit_code !== 0 && data.stderr ? data.stderr : (data.stdout || data.stderr || '(no output)');
      appendMessage('assistant', '```\n' + out + '\n```');
    } else if (data.type === 'prompt_dispatched') {
      appendMessage('user', displayText);
      startThinking();
    } else if (data.type === 'task_triggered') {
      appendMessage('system', `Scheduled task "${data.task}" triggered — session ${data.session_id.slice(0, 8)}, thread ${data.thread_id.slice(0, 8)}`);
    }
  } catch (err) {
    console.error('executeSlashCommand failed:', err);
    showToast('Slash command failed: ' + err.message, true);
  }
}

function handleInputKey(e) {
  const popup = document.getElementById('slash-popup');
  const popupVisible = popup && popup.classList.contains('visible');
  if (popupVisible) {
    if (e.key === 'ArrowDown') { e.preventDefault(); navigateSlashPopup(1); return; }
    if (e.key === 'ArrowUp') { e.preventDefault(); navigateSlashPopup(-1); return; }
    if (e.key === 'Tab' || e.key === 'Enter') {
      e.preventDefault();
      const active = popup.querySelector('.slash-item.active');
      if (active) {
        const nameEl = active.querySelector('.slash-name');
        const name = nameEl ? nameEl.textContent.slice(1) : '';
        selectSlashCommand(name);
      }
      return;
    }
    if (e.key === 'Escape') { e.preventDefault(); hideSlashPopup(); return; }
  }
  if (e.key === 'Enter' && !e.shiftKey && !isMobile()) {
    e.preventDefault();
    const input = document.getElementById('msg-input');
    const val = input ? input.value.trim() : '';
    if (val.startsWith('/')) {
      const spaceIdx = val.indexOf(' ');
      const name = spaceIdx === -1 ? val.slice(1) : val.slice(1, spaceIdx);
      const args = spaceIdx === -1 ? '' : val.slice(spaceIdx + 1).trim();
      executeSlashCommand(name, args);
    } else {
      sendMessage();
    }
  }
}
