// ---------------------------------------------------------------------------
// Slash command sidebar form
// ---------------------------------------------------------------------------
let slashSidebarVisible = false;
let slashSidebarCommands = [];

// One class chain for the param form's select, textarea, and plain inputs keeps
// the three control variants painting alike; the textarea site appends
// ' resize-y' on top. Split only at token boundaries: Tailwind's content scan
// only generates classes it sees as complete tokens, so no literal may break
// inside a class name.
const SLASH_FORM_INPUT_CLASS =
  'w-full bg-slate-800 border border-slate-600 rounded-lg px-3 py-2 text-sm text-slate-200'
  + ' focus:outline-none focus:border-blue-500';

// Element ids defined in web/templates/index.html; the sidebar select and the
// params container each have several consumers below.
const SLASH_SIDEBAR_SELECT_ID = 'slash-sidebar-select';
const SLASH_SIDEBAR_PARAMS_ID = 'slash-sidebar-params';

function toggleSlashSidebar() {
  slashSidebarVisible = !slashSidebarVisible;
  const sidebar = document.getElementById('slash-sidebar');
  if (!sidebar) return;
  if (slashSidebarVisible) {
    sidebar.classList.add('visible');
    populateSlashSidebarSelect();
  } else {
    sidebar.classList.remove('visible');
  }
}

function closeSlashSidebar() {
  slashSidebarVisible = false;
  const sidebar = document.getElementById('slash-sidebar');
  if (sidebar) sidebar.classList.remove('visible');
}

async function populateSlashSidebarSelect() {
  // Re-use the already-fetched slashCommands from slash-commands.js, or fetch fresh
  if (slashCommands && slashCommands.length) {
    slashSidebarCommands = slashCommands;
  } else {
    try {
      const res = await fetch('/api/slash/commands');
      if (res.ok) {
        slashSidebarCommands = await res.json();
        slashCommands = slashSidebarCommands;
      }
    } catch (err) {
      console.error('Failed to fetch slash commands for sidebar:', err);
      return;
    }
  }
  const select = document.getElementById(SLASH_SIDEBAR_SELECT_ID);
  if (!select) return;
  select.innerHTML = '<option value="">Select a command...</option>';
  for (const cmd of slashSidebarCommands.filter(c => !c.frontendOnly)) {
    const opt = document.createElement('option');
    opt.value = cmd.name;
    opt.textContent = '/' + cmd.name + (cmd.description ? ' — ' + cmd.description : '');
    select.appendChild(opt);
  }
}

function onSlashSidebarCommandChange() {
  const select = document.getElementById(SLASH_SIDEBAR_SELECT_ID);
  const name = select ? select.value : '';
  const cmd = slashSidebarCommands.find(c => c.name === name);
  renderSlashSidebarParams(cmd);
  updateSlashSidebarPreview();
}

function renderSlashSidebarParams(cmd) {
  const container = document.getElementById(SLASH_SIDEBAR_PARAMS_ID);
  const runBtn = document.getElementById('slash-sidebar-run');
  if (!container) return;
  container.innerHTML = '';
  if (!cmd) {
    if (runBtn) runBtn.disabled = true;
    return;
  }
  if (runBtn) runBtn.disabled = false;

  const params = cmd.params || [];
  if (!params.length) {
    container.innerHTML = '<p class="text-xs text-slate-500">No parameters — just click Run.</p>';
    return;
  }

  for (const p of params) {
    const wrapper = document.createElement('div');
    const label = document.createElement('label');
    label.className = 'block text-xs text-slate-400 mb-1';
    label.textContent = p.label || p.name;
    if (p.required) {
      const reqBadge = document.createElement('span');
      reqBadge.className = 'param-badge param-badge-required';
      reqBadge.textContent = 'required';
      label.appendChild(reqBadge);
    }
    if (p.type === 'number') {
      const typeBadge = document.createElement('span');
      typeBadge.className = 'param-badge';
      typeBadge.textContent = 'integer';
      label.appendChild(typeBadge);
    } else if (p.type === 'select') {
      const typeBadge = document.createElement('span');
      typeBadge.className = 'param-badge';
      typeBadge.textContent = 'select';
      label.appendChild(typeBadge);
    }
    wrapper.appendChild(label);

    let input;
    if (p.type === 'select') {
      input = document.createElement('select');
      input.className = SLASH_FORM_INPUT_CLASS;
      const emptyOpt = document.createElement('option');
      emptyOpt.value = '';
      emptyOpt.textContent = p.placeholder || 'Select...';
      input.appendChild(emptyOpt);
      for (const o of (p.options || [])) {
        const opt = document.createElement('option');
        opt.value = o;
        opt.textContent = o;
        if (p.default === o) opt.selected = true;
        input.appendChild(opt);
      }
    } else if (p.type === 'checkbox') {
      input = document.createElement('input');
      input.type = 'checkbox';
      input.className = 'rounded bg-slate-800 border-slate-600';
      if (p.default === 'true') input.checked = true;
    } else if (p.type === 'text') {
      input = document.createElement('textarea');
      input.rows = 6;
      input.className = SLASH_FORM_INPUT_CLASS + ' resize-y';
      if (p.placeholder) input.placeholder = p.placeholder;
      if (p.default) input.value = p.default;
      input.addEventListener('input', function() { autoResize(this); });
    } else {
      input = document.createElement('input');
      input.type = p.type === 'number' ? 'number' : 'text';
      input.className = SLASH_FORM_INPUT_CLASS;
      if (p.placeholder) input.placeholder = p.placeholder;
      if (p.default) input.value = p.default;
    }
    input.dataset.paramName = p.name;
    input.addEventListener('input', updateSlashSidebarPreview);
    input.addEventListener('change', updateSlashSidebarPreview);
    wrapper.appendChild(input);
    container.appendChild(wrapper);
  }
}

function getSlashSidebarFormValues() {
  const container = document.getElementById(SLASH_SIDEBAR_PARAMS_ID);
  if (!container) return {};
  const values = {};
  container.querySelectorAll('[data-param-name]').forEach(el => {
    if (el.type === 'checkbox') {
      values[el.dataset.paramName] = el.checked ? 'true' : '';
    } else {
      values[el.dataset.paramName] = el.value;
    }
  });
  return values;
}

function assembleSlashArgs(cmdName, values) {
  if (cmdName === 'run') {
    return (values.task_name || '').trim();
  }
  if (cmdName === 'stop-improve' || cmdName === 'help') {
    return '';
  }
  // Generic: join all non-empty values with spaces
  return Object.values(values).filter(v => v.trim()).join(' ');
}

function updateSlashSidebarPreview() {
  const preview = document.getElementById('slash-sidebar-preview');
  const select = document.getElementById(SLASH_SIDEBAR_SELECT_ID);
  if (!preview || !select) return;
  const name = select.value;
  if (!name) {
    preview.textContent = '';
    return;
  }
  const values = getSlashSidebarFormValues();
  const args = assembleSlashArgs(name, values);
  preview.textContent = '/' + name + (args ? ' ' + args : '');
  preview.title = preview.textContent;
}

function submitSlashSidebarForm() {
  const select = document.getElementById(SLASH_SIDEBAR_SELECT_ID);
  if (!select || !select.value) return;
  const name = select.value;
  const values = getSlashSidebarFormValues();
  const args = assembleSlashArgs(name, values);
  executeSlashCommand(name, args);
  closeSlashSidebar();
  // Reset form
  select.value = '';
  const container = document.getElementById(SLASH_SIDEBAR_PARAMS_ID);
  if (container) container.innerHTML = '';
  const preview = document.getElementById('slash-sidebar-preview');
  if (preview) preview.textContent = '';
  const runBtn = document.getElementById('slash-sidebar-run');
  if (runBtn) runBtn.disabled = true;
}

// Keyboard: Ctrl+/ to toggle, Escape to close, Ctrl+Enter to submit
document.addEventListener('keydown', function(e) {
  if (e.ctrlKey && e.key === '/') {
    e.preventDefault();
    toggleSlashSidebar();
    return;
  }
  if (slashSidebarVisible) {
    if (e.key === 'Escape') {
      e.preventDefault();
      closeSlashSidebar();
      return;
    }
    if (e.ctrlKey && e.key === 'Enter') {
      e.preventDefault();
      submitSlashSidebarForm();
    }
  }
});
