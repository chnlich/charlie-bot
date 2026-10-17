(function() {
  const Chat = globalThis.Chat;

// ---------------------------------------------------------------------------
// The For-you view: the session page's reading layer for a root web session.
//
// For you shows the user's messages, the agent's web replies (web_reply), the
// web reply notices (web_reply_notice) and error rows, in place. Each run of
// other messages folds into one line. Everything shows the whole record, as the
// page always has. A web session (a manager session that is not a Slack or
// Discord thread), root or child, opens in For you; a thread session and a worker
// session open in Everything. A manual switch holds for that session until the
// page reloads.
//
// turn-engine.js builds the list from the store. This file holds the rules
// (what shows and where a fold falls), the row builders and the toggle.
// ---------------------------------------------------------------------------
const FOR_YOU = 'foryou';
const EVERYTHING = 'everything';
const SHOWN_ROLES = ['user', 'web_reply', 'web_reply_notice'];
// The aggregator writes a failed turn's event as a system row whose text starts
// with this prefix (message_aggregator.py: ASSISTANT_ERROR and ERROR).
const ERROR_PREFIX = 'Error: ';
// The manual view choice of each session on this page: a reload empties it, as
// the sidebar's expanded nodes reset.
const manualViews = new Map();

function isErrorMessage(msg) {
  return msg.role === 'system' && String(msg.content || '').startsWith(ERROR_PREFIX);
}

function showsInForYou(msg) {
  return SHOWN_ROLES.includes(msg.role) || isErrorMessage(msg);
}

// The For-you rows over the store's entries, in order: a shown message is one
// row; each run of other entries is one fold row, keyed by its first entry. A
// separator ends a turn and is not a message, so the fold's count leaves it out,
// and a run of separators alone gets no fold row.
function items(entries) {
  const out = [];
  let run = [];
  const closeRun = () => {
    const count = run.filter((entry) => entry.msg.role !== 'separator').length;
    if (count > 0) out.push({kind: 'fold', key: 'fold:' + run[0].msg.id, count, hidden: run});
    run = [];
  };
  for (const entry of entries) {
    if (showsInForYou(entry.msg)) {
      closeRun();
      out.push({kind: 'message', entry});
    } else {
      run.push(entry);
    }
  }
  closeRun();
  return out;
}

// The turn is running: the page's thinking state, set by startThinking and cleared
// by stopThinking (session-view.js).
function turnRunning() {
  return masterThinking;
}

function textSpan(className, text) {
  const span = document.createElement('span');
  span.className = className;
  span.textContent = text;
  return span;
}

function buildFoldLine(onToggle) {
  const btn = document.createElement('button');
  btn.type = 'button';
  btn.className = 'for-you-fold flex items-center gap-2 w-full py-1 text-xs text-slate-500 hover:text-slate-300 transition-colors';
  btn.onclick = onToggle;
  btn.appendChild(textSpan('for-you-fold-chevron w-3 text-center', ''));
  btn.appendChild(textSpan('for-you-fold-label', ''));
  return btn;
}

function fillFoldLine(btn, count, expanded) {
  btn.dataset.count = String(count);
  btn.setAttribute('aria-expanded', String(expanded));
  btn.querySelector('.for-you-fold-chevron').textContent = expanded ? '▾' : '▸';
  btn.querySelector('.for-you-fold-label').textContent = count + ' hidden messages';
}

function buildWorkingLine() {
  const el = document.createElement('div');
  el.className = 'for-you-working flex items-center gap-2 px-4 py-2 text-sm text-slate-400';
  el.appendChild(textSpan('w-2 h-2 rounded-full bg-blue-400 animate-pulse-dot', ''));
  el.appendChild(textSpan('', 'Working…'));
  return el;
}

// A web session: a manager session with no Slack or Discord origin. The status panel
// reads the same test.
function isWebSession(session) {
  return session.profile === 'manager' && session.slack_origin == null && session.discord_origin == null;
}

// A web session, root or child, opens in For you; a thread session and a worker
// session open in Everything.
function defaultView(session) {
  return isWebSession(session) ? FOR_YOU : EVERYTHING;
}

function viewFor(session) {
  return manualViews.get(session.id) || defaultView(session);
}

// The toggle's buttons carry aria-pressed, and their styles follow it.
function renderToggle(view) {
  document.querySelectorAll('[data-chat-view-option]').forEach((btn) => {
    btn.setAttribute('aria-pressed', String(btn.dataset.chatViewOption === view));
  });
}

// The toggle's switch. The choice holds for this session until the page reloads.
function setChatView(view) {
  const engine = Chat.activeTurnEngineFor(document.getElementById('messages'));
  if (!engine) throw new Error('No turn engine is mounted for the chat view');
  engine.setView(view);
  manualViews.set(SESSION_ID, view);
  renderToggle(view);
}

// A turn started or ended: the For you working line follows the running state.
function refresh() {
  const engine = Chat.activeTurnEngineFor(document.getElementById('messages'));
  if (engine) engine.refreshForYou();
}

// The turn engine projects these rules, so they register with it; the page reads the same object as
// Chat.ForYou.
const ForYou = {
  FOR_YOU,
  EVERYTHING,
  items,
  turnRunning,
  buildFoldLine,
  fillFoldLine,
  buildWorkingLine,
  isWebSession,
  defaultView,
  viewFor,
  renderToggle,
  refresh,
};
Chat.registerForYouRules(ForYou);
Chat.wire({setChatView}, {ForYou});

})();
