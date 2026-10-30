// ---------------------------------------------------------------------------
// The displayed session
// ---------------------------------------------------------------------------
// One owner of the session this page displays — the session the header names.
// The header name, the tab title and the composer's To: badge are written
// here and nowhere else. The template renders all three before any script
// runs, and this module takes them over from the server-rendered globals.
// Every path that changes the page's session reports through
// setDisplayedSession: a sidebar click, a new session and the archive back to
// the welcome page. A render and the two rename writers refresh the name
// through refreshDisplayedSessionName, which repaints only the displayed
// session and never moves it.
//
// The module also holds the composer actions pressed while their session was
// still loading. bindComposerAction runs an action at once when its session
// is the loaded one and holds it otherwise; the switch's landing runs it
// through runHeldComposerActions, and a switch that will never land resolves
// it through abandonHeldComposerActions — a held message posts to the session
// it bound, a held Stop or attachment is dropped with a note.

const DISPLAYED_WELCOME_TITLE = 'CharlieBot@' + PAGE_HOSTNAME;

let displayedSessionId = SESSION_ID;
let displayedSessionName = null;

// The composer actions waiting for their session's switch to land. Each
// carries the session it binds; kind says how a switch that never lands
// treats it: 'message' posts, 'interrupt' drops.
let heldComposerActions = [];

function paintDisplayedSession() {
  const headerName = document.getElementById('header-session-name');
  if (headerName) headerName.textContent = displayedSessionName;
  document.title = displayedSessionName
    ? displayedSessionName + ' · CharlieBot@' + PAGE_HOSTNAME
    : DISPLAYED_WELCOME_TITLE;
  const badge = document.getElementById('composer-target-badge');
  if (badge) {
    badge.textContent = 'To: ' + displayedSessionName;
    badge.classList.toggle('hidden', !displayedSessionName);
  }
}

// The one writer of the displayed session. A click that moves on resolves the
// composer actions the previous click held first, so a held message posts
// before this click swaps the composer to the new draft.
function setDisplayedSession(sessionId, sessionName) {
  abandonHeldComposerActions(displayedSessionId);
  displayedSessionId = sessionId;
  displayedSessionName = sessionName;
  paintDisplayedSession();
}

// A render refreshes the displayed session's name; the two rename writers
// come through here too. A rename of another session repaints nothing.
function refreshDisplayedSessionName(sessionId, sessionName) {
  if (sessionId !== displayedSessionId) return;
  displayedSessionName = sessionName;
  paintDisplayedSession();
}

// The session a composer action binds at press: the one the page displays.
function composerTargetSession() {
  return {id: displayedSessionId, name: displayedSessionName};
}

// Runs a composer action at once when its session is the loaded one —
// returning the action's promise, as the composer paths always did — and
// holds it for the switch's landing otherwise. kind is 'message' for an
// action that posts a message (a send, /compact) and 'interrupt' for one that
// needs the session loaded (Stop, attach).
function bindComposerAction(sessionId, kind, run, drop) {
  if (sessionId === SESSION_ID) {
    return run();
  }
  heldComposerActions.push({sessionId, kind, run, drop});
}

// The switch to sessionId landed: the actions bound to it run now, against
// the freshly rendered view.
function runHeldComposerActions(sessionId) {
  const due = heldComposerActions.filter((held) => held.sessionId === sessionId);
  heldComposerActions = heldComposerActions.filter((held) => held.sessionId !== sessionId);
  due.forEach((held) => held.run());
}

// The switch to sessionId will never land — a newer click took the view, the
// bootstrap failed into a full page load, or the page moved to the welcome
// view. A held message posts to the session it bound; a held Stop or
// attachment is dropped with a note.
function abandonHeldComposerActions(sessionId) {
  const due = heldComposerActions.filter((held) => held.sessionId === sessionId);
  heldComposerActions = heldComposerActions.filter((held) => held.sessionId !== sessionId);
  due.forEach((held) => {
    if (held.kind === 'message') held.run();
    else held.drop();
  });
}
