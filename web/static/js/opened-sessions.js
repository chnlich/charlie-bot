// The browser's record of the CharlieBot chat pages it has opened, kept in localStorage, and the open case
// of this page: "A" when this browser opened this session before, "B" when it opened a chat page before but
// not this session, "C" when it has opened no chat page before. This is the one owner of the case. The page
// reads it at its own start, before the page records its open, so the case names the browser's state when
// the page was requested. The page's page_load report carries it, and a stored copy keys on the same
// session ids.
const OpenedSessions = (function() {
  const RECORD_KEY = 'charliebot_opened_pages';

  // `openCase` is null when the browser refuses localStorage: the page then reports no case, and the
  // console names the failure.
  // The session the page opens: the embedded one, or the one the shell's URL names.
  const sessionId = SESSION_ID !== null ? SESSION_ID : new URLSearchParams(location.search).get('session') || null;
  let openCase = null;
  try {
    const now = Date.now();
    const record = JSON.parse(localStorage.getItem(RECORD_KEY) || 'null');
    if (sessionId === null) openCase = null;
    else if (record === null) openCase = 'C';
    else openCase = Object.hasOwn(record.sessions, sessionId) ? 'A' : 'B';
    const next = record === null ? {first_open_ms: now, sessions: {}} : record;
    if (sessionId !== null) next.sessions[sessionId] = now;
    localStorage.setItem(RECORD_KEY, JSON.stringify(next));
  } catch (error) {
    console.warn('opened-sessions: the open case is not recorded', error);
  }

  return {
    openCase: () => openCase,
  };
})();
