// ---------------------------------------------------------------------------
// One fake staged session dir for the node --test vm render tests. It stands
// in for the server-rendered window.SESSIONS_ROOT (web/templates/index.html):
// every vm harness injects the same root, and tests that do path math use the
// one staged session id and its derived dir.
// ---------------------------------------------------------------------------
const SESSIONS_ROOT = '/home/user/.charliebot/sessions';
const SESSION_ID = 'sess-42';
const SESSION_DIR = SESSIONS_ROOT + '/' + SESSION_ID;

module.exports = { SESSIONS_ROOT, SESSION_ID, SESSION_DIR };
