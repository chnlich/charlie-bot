// ---------------------------------------------------------------------------
// The sidebar's "Move under…" picker (sidebar/session-move.js). A manager row's
// Settings menu opens it (session-menus.js). The picker lists the manager
// sessions the moved session can go under. A pick sends one PATCH that sets the
// moved session's task parent; "Top level" sends null. Each open reads both
// session list, the Workspace's GET /api/sessions/?compact=move column form, which
// holds every thread session too. A 200 closes the dialog, and the moved row
// reaches every open tab through the task_tree_changed push. A 409 keeps the
// dialog open and lists the server's blockers.
// ---------------------------------------------------------------------------
(function() {
  const Sidebar = globalThis.Sidebar;

// The dialog's footer: what the move does to the moved session. It shows before any pick.
const AFTER_MOVE_TEXT = 'After the move, this session follows the rules set by its new parent and the sessions above it, ' +
  'and its prompt names the new parent\'s goal. When this changes its prompt, a live conversation starts over ' +
  'on its next turn.';

const OVERLAY_ID = 'session-move-overlay';
const TOP_LEVEL_LABEL = 'Top level';
// The search box takes the Set Group dialog's boundary and focus tones; session-actions.js gives their contrast.
const SEARCH_BORDER = '#94a3b8';
const SEARCH_FOCUS_BORDER = '#c084fc';
const SEARCH_CLASS = 'dialog-text-field w-full bg-slate-700 border rounded-lg px-3 py-1.5 text-sm text-slate-200 focus:outline-none mb-3';
const LIST_CLASS = 'flex flex-col gap-1.5 mb-3 max-h-48 overflow-y-auto';
// A choice is a flex item of the scrolling list. truncate sets overflow to hidden, which gives the item a zero
// automatic minimum height, so without shrink-0 the list squeezes each row to a share of its height. shrink-0 keeps
// each row at its line height plus padding, and the list scrolls the rest. The dialog-choice-row
// class gives the row its 44 px box on a touch screen (sidebar-interactions.css).
const CHOICE_CLASS = 'dialog-choice-row w-full text-left px-3 py-2 rounded-lg text-sm bg-slate-700 hover:bg-slate-600 ' +
  'text-slate-300 transition-colors truncate shrink-0';
const ERROR_CLASS = 'flex flex-col gap-1 text-xs text-red-300';
const FOOTER_CLASS = 'mt-2 text-xs text-slate-400';
// The Set Group dialog's close control. The modal-close-btn class is styled by the page's close_button.html.
const CLOSE_BUTTON_CLASS = 'text-slate-500 hover:text-slate-300 p-1 modal-close-btn';

// The open picker: its overlay and the Escape listener that its close path removes. One picker is open at a time.
let openPicker = null;

function closeSessionMovePicker() {
  if (openPicker === null) return;
  openPicker.overlay.remove();
  document.removeEventListener('keydown', openPicker.onKeyDown);
  openPicker = null;
}

async function fetchSessionRows(url) {
  const res = await fetch(url);
  if (!res.ok) throw new Error(`Fetch ${url} failed: ${res.status}`);
  return res.json();
}

// The Workspace list, which holds every active row the picker can move, thread sessions included. It travels in
// `compact=move`'s column form, which carries the four fields this picker reads and no other. The column form is about
// a fifth of the row array's bytes for the same rows, and the picker is the only caller that needs the parent and
// profile fields the sidebar's own list leaves out.
async function fetchMoveRows() {
  const columns = await fetchSessionRows('/api/sessions/?compact=move');
  return Sidebar.expandSessionColumns(columns);
}

// The manager rows the moved session can go under. The moved session, its descendants and its current parent are out.
// A descendant is found by walking the task parents of the merged rows, so a row from either list counts.
function moveCandidates(rows, moved) {
  const childrenOf = new Map();
  for (const row of rows) {
    if (!row.task_parent_id) continue;
    if (!childrenOf.has(row.task_parent_id)) childrenOf.set(row.task_parent_id, []);
    childrenOf.get(row.task_parent_id).push(row.id);
  }
  const excluded = new Set([moved.id]);
  if (moved.task_parent_id) excluded.add(moved.task_parent_id);
  const toWalk = [moved.id];
  while (toWalk.length > 0) {
    for (const childId of childrenOf.get(toWalk.pop()) || []) {
      if (!excluded.has(childId)) {
        excluded.add(childId);
        toWalk.push(childId);
      }
    }
  }
  return rows.filter((row) => row.profile === 'manager' && !excluded.has(row.id));
}

// The picker's dialog in the Set Group chrome. Its footer is set here, so it is on screen before any pick.
function buildMovePickerDialog() {
  const chrome = Sidebar.modalChrome;
  const overlay = document.createElement('div');
  overlay.id = OVERLAY_ID;
  overlay.className = chrome.overlay;
  const dialog = document.createElement('div');
  dialog.className = chrome.dialog;

  // The header names the operation and, under it, the session that moves. The session's name sets no bound, so it
  // is the line that elides: the operation's words stay whole, the way the menu item that opened the dialog reads.
  const header = document.createElement('div');
  header.className = 'flex items-start justify-between gap-2 mb-3';
  const title = document.createElement('div');
  title.className = 'min-w-0';
  const heading = document.createElement('p');
  heading.className = 'session-move-heading text-sm text-slate-300 font-semibold';
  heading.textContent = 'Move under…';
  const subject = document.createElement('p');
  subject.className = 'session-move-subject text-xs text-slate-400 truncate';
  title.appendChild(heading);
  title.appendChild(subject);
  const closeButton = document.createElement('button');
  closeButton.type = 'button';
  closeButton.title = 'Close';
  closeButton.className = CLOSE_BUTTON_CLASS;
  closeButton.innerHTML = cbIcon('close', 16, 'block');
  closeButton.addEventListener('click', closeSessionMovePicker);
  header.appendChild(title);
  header.appendChild(closeButton);

  const search = document.createElement('input');
  search.type = 'text';
  search.placeholder = 'Search sessions';
  search.className = SEARCH_CLASS;
  search.style.borderColor = SEARCH_BORDER;
  search.addEventListener('focus', () => { search.style.borderColor = SEARCH_FOCUS_BORDER; });
  search.addEventListener('blur', () => { search.style.borderColor = SEARCH_BORDER; });

  const list = document.createElement('div');
  list.className = LIST_CLASS;
  const errors = document.createElement('div');
  errors.className = ERROR_CLASS;
  const footer = document.createElement('p');
  footer.className = FOOTER_CLASS;
  footer.textContent = AFTER_MOVE_TEXT;

  dialog.appendChild(header);
  dialog.appendChild(search);
  dialog.appendChild(list);
  dialog.appendChild(errors);
  dialog.appendChild(footer);
  overlay.appendChild(dialog);
  overlay.addEventListener('click', (event) => {
    if (event.target === overlay) closeSessionMovePicker();
  });

  const showErrors = (lines) => {
    errors.innerHTML = '';
    for (const line of lines) {
      const message = document.createElement('div');
      message.textContent = line;
      errors.appendChild(message);
    }
  };
  return {overlay, heading, subject, search, list, showErrors};
}

async function openSessionMovePicker(sessionId) {
  closeSessionMovePicker();
  const dialog = buildMovePickerDialog();
  const onKeyDown = (event) => {
    if (event.key === 'Escape') closeSessionMovePicker();
  };
  openPicker = {overlay: dialog.overlay, onKeyDown};
  document.addEventListener('keydown', onKeyDown);
  document.body.appendChild(dialog.overlay);
  dialog.search.focus();

  // One pick sends one PATCH: a pick in flight holds the next pick until its answer lands.
  let pending = false;
  const pick = async (parentId) => {
    if (pending) return;
    pending = true;
    dialog.showErrors([]);
    try {
      const res = await fetch('/api/sessions/' + sessionId, {
        method: 'PATCH',
        headers: JSON_HEADERS,
        body: JSON.stringify({task_parent_id: parentId}),
      });
      if (res.ok) {
        closeSessionMovePicker();
        return;
      }
      if (res.status !== 409) throw new Error(`Move failed: ${res.status}`);
      const body = await res.json();
      dialog.showErrors(body.detail.blockers);
    } catch (error) {
      console.error('Move under failed:', error);
      dialog.showErrors([error.message]);
    }
    pending = false;
  };
  // A session choice may elide its name, so its full name is also the hover text.
  const choiceButton = (label, parentId) => {
    const button = document.createElement('button');
    button.type = 'button';
    button.className = CHOICE_CLASS;
    button.textContent = label;
    if (parentId !== null) button.title = label;
    button.addEventListener('click', () => pick(parentId));
    return button;
  };

  try {
    const rows = await fetchMoveRows();
    const moved = rows.find((row) => row.id === sessionId);
    if (!moved) throw new Error(`Session ${sessionId} is not in the Workspace list`);
    dialog.subject.textContent = `“${moved.name}”`;
    dialog.subject.title = moved.name;
    const candidates = moveCandidates(rows, moved);
    // Top level sits first and stays while the search narrows the sessions: it names no session.
    const renderChoices = () => {
      const query = dialog.search.value.trim().toLowerCase();
      dialog.list.innerHTML = '';
      if (moved.task_parent_id) dialog.list.appendChild(choiceButton(TOP_LEVEL_LABEL, null));
      for (const row of candidates) {
        if (row.name.toLowerCase().includes(query)) dialog.list.appendChild(choiceButton(row.name, row.id));
      }
    };
    dialog.search.addEventListener('input', renderChoices);
    renderChoices();
  } catch (error) {
    console.error('Move under failed:', error);
    dialog.showErrors([error.message]);
  }
}

Sidebar.wire({openSessionMovePicker});

})();
