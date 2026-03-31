// ---------------------------------------------------------------------------
// Context panel (sidebar tab) — working repo & branch selector
// ---------------------------------------------------------------------------
let ctxRepo = null;   // { name: string, path: string } or null
let ctxBranch = null;  // string or null
let ctxProjects = null; // cached [{name, path}, ...]

function switchSidebarTab(tab) {
  const cmds = document.getElementById("sidebar-panel-commands");
  const ctx = document.getElementById("sidebar-panel-context");
  const footer = document.getElementById("slash-sidebar-footer");
  const tabCmds = document.getElementById("sidebar-tab-commands");
  const tabCtx = document.getElementById("sidebar-tab-context");
  if (!cmds || !ctx) return;
  if (tab === "context") {
    cmds.classList.add("hidden");
    ctx.classList.remove("hidden");
    if (footer) footer.classList.add("hidden");
    tabCmds?.classList.remove("active");
    tabCtx?.classList.add("active");
    populateCtxRepos();
  } else {
    cmds.classList.remove("hidden");
    ctx.classList.add("hidden");
    if (footer) footer.classList.remove("hidden");
    tabCmds?.classList.add("active");
    tabCtx?.classList.remove("active");
  }
}

async function populateCtxRepos() {
  const select = document.getElementById("ctx-repo-select");
  if (!select) return;
  if (!ctxProjects) {
    try {
      const res = await fetch("/api/sessions/projects");
      if (res.ok) ctxProjects = await res.json();
    } catch (err) {
      console.error("populateCtxRepos failed:", err);
      return;
    }
  }
  // Rebuild options preserving current selection
  select.innerHTML = '<option value="">None</option>';
  for (const p of (ctxProjects || [])) {
    const name = p.name || p.path.split("/").pop();
    const opt = document.createElement("option");
    opt.value = p.path;
    opt.textContent = name;
    opt.dataset.repoName = name;
    if (ctxRepo && ctxRepo.path === p.path) opt.selected = true;
    select.appendChild(opt);
  }
}

async function onCtxRepoChange() {
  const select = document.getElementById("ctx-repo-select");
  const path = select ? select.value : "";
  if (!path) {
    ctxRepo = null;
    ctxBranch = null;
    clearCtxBranches();
    updateContextChip();
    return;
  }
  const opt = select.selectedOptions[0];
  ctxRepo = { name: opt.dataset.repoName || path.split("/").pop(), path: path };
  ctxBranch = null;
  await populateCtxBranches(path);
  updateContextChip();
}

async function populateCtxBranches(repoPath) {
  const select = document.getElementById("ctx-branch-select");
  if (!select) return;
  select.innerHTML = '<option value="">Default</option>';
  try {
    const res = await fetch("/api/git/branches?repo=" + encodeURIComponent(repoPath));
    if (!res.ok) return;
    const branches = await res.json();
    for (const b of branches) {
      const opt = document.createElement("option");
      opt.value = b;
      opt.textContent = b;
      select.appendChild(opt);
    }
  } catch (err) {
    console.error("populateCtxBranches failed:", err);
  }
}

function clearCtxBranches() {
  const select = document.getElementById("ctx-branch-select");
  if (select) select.innerHTML = '<option value="">Default</option>';
}

function onCtxBranchChange() {
  const select = document.getElementById("ctx-branch-select");
  ctxBranch = select && select.value ? select.value : null;
  updateContextChip();
}

function updateContextChip() {
  const container = document.getElementById("context-chip");
  if (!container) return;
  if (!ctxRepo) {
    container.classList.add("hidden");
    container.innerHTML = "";
    return;
  }
  let text = ctxRepo.name;
  if (ctxBranch) text += " @ " + ctxBranch;
  container.innerHTML = '<span class="chip">' + escapeHtml(text) + ' <button onclick="clearWorkingContext()" title="Clear context">&times;</button></span>';
  container.classList.remove("hidden");
}

function clearWorkingContext() {
  ctxRepo = null;
  ctxBranch = null;
  const repoSel = document.getElementById("ctx-repo-select");
  if (repoSel) repoSel.value = "";
  clearCtxBranches();
  updateContextChip();
}

/**
 * Called from sendMessage flow. If context is set, prepend header to message content.
 * Returns the (possibly modified) message string.
 */
function applyWorkingContext(content) {
  if (!ctxRepo) return content;
  let header;
  if (ctxBranch) {
    header = "[Repo: " + ctxRepo.path + " | Branch: " + ctxBranch + "]";
  } else {
    header = "[Repo: " + ctxRepo.path + "]";
  }
  return header + "\n\n" + content;
}
