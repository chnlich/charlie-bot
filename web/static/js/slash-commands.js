// ---------------------------------------------------------------------------
// Slash command popup
// ---------------------------------------------------------------------------
let slashCommands = [];
let slashPopupIdx = -1;

// Chained mode state for /w (working context)
let slashChainedMode = null;
// Cache: {repoName: {path, branches[]}}
let slashWCache = { projects: null, branches: {} };

async function fetchSlashCommands() {
  try {
    const res = await fetch('/api/slash/commands');
    if (res.ok) slashCommands = await res.json();
  } catch (err) {
    console.error('fetchSlashCommands failed:', err);
  }
  // Add frontend-only /w command
  if (!slashCommands.find(c => c.name === 'w')) {
    slashCommands.push({ name: 'w', description: 'Set working repo & branch context' });
  }
}

function handleSlashInput(el) {
  const val = el.value;

  // If in chained mode, handle filtering for current step
  if (slashChainedMode) {
    _handleChainedInput(val);
    return;
  }

  if (val.startsWith('/') && !val.includes(' ') && val.length < 30) {
    showSlashPopup(val.slice(1));
  } else {
    hideSlashPopup();
  }
}

function _handleChainedInput(val) {
  const mode = slashChainedMode;
  if (mode.step === 'repo') {
    // User is typing after '/w ' — filter repos
    const prefix = '/w ';
    if (!val.startsWith(prefix)) { _exitChainedMode(); return; }
    const typed = val.slice(prefix.length);
    // If user typed a space, they're skipping
    if (typed.includes(' ')) { _exitChainedMode(); return; }
    const items = mode.repoList.filter(r => r.name.toLowerCase().startsWith(typed.toLowerCase()));
    _showChainedPopup(items.map(r => r.name), items.map(r => r.path));
  } else if (mode.step === 'branch') {
    const prefix = '/w ' + mode.selectedRepo + ' ';
    if (!val.startsWith(prefix)) { _exitChainedMode(); return; }
    const typed = val.slice(prefix.length);
    if (typed.includes(' ')) { _exitChainedMode(); return; }
    const items = mode.branchList.filter(b => b.toLowerCase().startsWith(typed.toLowerCase()));
    _showChainedPopup(items, null);
  }
}

function _showChainedPopup(names, paths) {
  const popup = document.getElementById('slash-popup');
  if (!popup) return;
  if (!names.length) { hideSlashPopup(); return; }
  popup.innerHTML = names.map((name, i) =>
    `<div class="slash-item${i === 0 ? ' active' : ''}" onclick="_selectChainedItem('${escapeHtml(name)}')" data-idx="${i}"` +
    (paths ? ` data-path="${escapeHtml(paths[i])}"` : '') + `>` +
    `<span class="slash-name">${escapeHtml(name)}</span>` +
    `</div>`
  ).join('');
  popup.classList.add('visible');
  slashPopupIdx = 0;
}

function _selectChainedItem(name) {
  const input = document.getElementById('msg-input');
  if (!input) return;
  const mode = slashChainedMode;
  if (!mode) return;

  if (mode.step === 'repo') {
    mode.selectedRepo = name;
    const repo = mode.repoList.find(r => r.name === name);
    mode.selectedRepoPath = repo ? repo.path : name;
    input.value = '/w ' + name + ' ';
    input.focus();
    autoResize(input);
    mode.step = 'branch';
    // Fetch branches
    _fetchBranches(mode.selectedRepoPath);
  } else if (mode.step === 'branch') {
    mode.selectedBranch = name;
    input.value = '/w ' + mode.selectedRepo + ' ' + name + ' ';
    input.focus();
    autoResize(input);
    _exitChainedMode();
  }
}

function _exitChainedMode() {
  slashChainedMode = null;
  hideSlashPopup();
}

async function _fetchBranches(repoPath) {
  if (slashWCache.branches[repoPath]) {
    const mode = slashChainedMode;
    if (mode) {
      mode.branchList = slashWCache.branches[repoPath];
      _showChainedPopup(mode.branchList, null);
    }
    return;
  }
  try {
    const res = await fetch('/api/git/branches?repo=' + encodeURIComponent(repoPath));
    if (!res.ok) { _exitChainedMode(); return; }
    const branches = await res.json();
    slashWCache.branches[repoPath] = branches;
    const mode = slashChainedMode;
    if (mode && mode.step === 'branch') {
      mode.branchList = branches;
      _showChainedPopup(branches, null);
    }
  } catch (err) {
    console.error('_fetchBranches failed:', err);
    _exitChainedMode();
  }
}

async function selectSlashCommand(name) {
  const input = document.getElementById('msg-input');
  if (!input) return;

  if (name === 'w') {
    // Enter chained mode
    input.value = '/w ';
    input.focus();
    autoResize(input);
    hideSlashPopup();
    slashChainedMode = { command: 'w', step: 'repo', repoList: [], branchList: [], selectedRepo: null, selectedBranch: null, selectedRepoPath: null };
    // Fetch projects
    try {
      let projects = slashWCache.projects;
      if (!projects) {
        const res = await fetch('/api/sessions/projects');
        if (!res.ok) { _exitChainedMode(); return; }
        projects = await res.json();
        slashWCache.projects = projects;
      }
      // projects is [{name, path}, ...] or similar
      const repoList = projects.map(p => ({ name: typeof p === 'string' ? p.split('/').pop() : (p.name || p.path.split('/').pop()), path: typeof p === 'string' ? p : p.path }));
      if (slashChainedMode) {
        slashChainedMode.repoList = repoList;
        _showChainedPopup(repoList.map(r => r.name), repoList.map(r => r.path));
      }
    } catch (err) {
      console.error('selectSlashCommand /w failed:', err);
      _exitChainedMode();
    }
    return;
  }

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
      if (slashChainedMode) {
        // In chained mode, select the active item directly
        const nameEl = active.querySelector('.slash-name');
        const name = nameEl ? nameEl.textContent : '';
        _selectChainedItem(name);
      } else {
        const nameEl = active.querySelector('.slash-name');
        const name = nameEl ? nameEl.textContent.slice(1) : '';
        selectSlashCommand(name);
      }
    }
    return true;
  }
  if (e.key === 'Escape') { e.preventDefault(); hideSlashPopup(); slashChainedMode = null; return true; }
  return false;
}

/**
 * Try to handle /w prefix on message submit. Returns true if handled.
 * Called from handleInputKey in app.js before normal slash dispatch.
 */
function handleSlashW(input) {
  const val = input.value.trim();
  if (!val.startsWith('/w ')) return false;
  const rest = val.slice(3).trim();
  if (!rest) return false;

  const tokens = rest.split(/\s+/);
  const repoName = tokens[0];

  // Look up full repo path from cached projects
  const projects = slashWCache.projects || [];
  const repo = projects.find(p => {
    const name = typeof p === 'string' ? p.split('/').pop() : (p.name || p.path.split('/').pop());
    return name === repoName;
  });
  if (!repo) return false;
  const repoPath = typeof repo === 'string' ? repo : repo.path;

  // Check if second token is a known branch
  let branch = null;
  let messageStart = 1;
  if (tokens.length > 1) {
    const cachedBranches = slashWCache.branches[repoPath] || [];
    if (cachedBranches.includes(tokens[1])) {
      branch = tokens[1];
      messageStart = 2;
    }
  }

  const message = tokens.slice(messageStart).join(' ').trim();
  if (!message) return false;

  // Rewrite input
  let header;
  if (branch) {
    header = `[Repo: ${repoPath} | Branch: ${branch}]`;
  } else {
    header = `[Repo: ${repoPath}]`;
  }
  input.value = header + '\n\n' + message;
  return true;
}
