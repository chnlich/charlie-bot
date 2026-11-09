// ---------------------------------------------------------------------------
// Last known state: stored copies, the not-current mark, catch-up and the read
// ---------------------------------------------------------------------------
// The copy contract (the headers, the network-only request and the offline fact) is in view-requests.js. A
// stored list reaches this owner as a `charliebot:stored-list` event (filters.js).
//
// Catch-up of an open: the socket replays from the first screen's event position (`event_count`) and
// ends the replay with `catchup_complete`, which carries `event_total`. The content is current when the
// replay has ended and, for a copy, the sidebar list refresh has succeeded. Only then does the
// not-current mark clear and the session's read post.
//
// State: `catchUp` holds the open in progress, or null. Its fields:
// - sessionId: the session the catch-up belongs to;
// - storedPosition: the event position the first screen carried (null for a standalone list copy);
// - copyBytes: the first screen's JSON size when the page paints a copy, null for network content. The
//   socket replay of a copy may not carry more bytes than this (the gap is larger than the first screen);
// - replayBytes: the replay bytes received so far, across the sockets of this open;
// - socketPending, listPending: what still has to land before the content is current;
// - readPending: whether the open posts the session's read once current.

const LIST_REFRESH_RETRY_BASE_MS = 1000;
const LIST_REFRESH_RETRY_MAX_MS = 30000;

let catchUp = null;

// The copy's bytes as the page measures them: the JSON text of the parsed first screen.
function jsonBytes(value) {
  return new TextEncoder().encode(JSON.stringify(value)).length;
}

// The mark is one element of the chat column: `data-freshness="not-current"` while the content is a copy.
function setNotCurrent(on) {
  const mark = document.getElementById('freshness-mark');
  mark.classList.toggle('hidden', !on);
  if (on) mark.setAttribute('data-freshness', 'not-current');
  else mark.removeAttribute('data-freshness');
}

// A rejected first-screen request while offline: the switch's placeholder names the missing copy.
function showNoLocalCopy() {
  const row = document.getElementById('switch-placeholder');
  if (row) row.textContent = NO_LOCAL_COPY_LABEL;
}

function listStillPending(sessionId) {
  return catchUp !== null && catchUp.sessionId === sessionId && catchUp.listPending;
}

// Starts the catch-up of an open. A copy's open also starts the sidebar list refresh. A restart passes
// the list state it inherits and starts no second refresh, since the first one still runs.
function beginCatchUp({sessionId, storedPosition, copyBytes, listPending, readPending}) {
  catchUp = {
    sessionId,
    storedPosition,
    copyBytes,
    replayBytes: 0,
    socketPending: true,
    listPending,
    readPending,
  };
  setNotCurrent(copyBytes !== null || listPending);
  if (copyBytes !== null && listPending) requestListRefresh(sessionId, 0);
}

// A sidebar list answer came from a copy: its rows carry the mark until a list refresh succeeds.
function noteStoredList() {
  if (catchUp === null) {
    catchUp = {
      sessionId: SESSION_ID,
      storedPosition: null,
      copyBytes: null,
      replayBytes: 0,
      socketPending: false,
      listPending: false,
      readPending: false,
    };
  }
  setNotCurrent(true);
  if (!catchUp.listPending) {
    catchUp.listPending = true;
    requestListRefresh(catchUp.sessionId, 0);
  }
}

// One network list refresh. A failure retries on a backoff for as long as the catch-up waits on it.
function requestListRefresh(sessionId, attempt) {
  refreshCurrentList().then(
      () => onListRefreshed(sessionId),
      (err) => {
        console.error('sidebar list refresh failed:', err);
        if (!listStillPending(sessionId)) return;
        const delay = Math.min(LIST_REFRESH_RETRY_BASE_MS * 2 ** attempt, LIST_REFRESH_RETRY_MAX_MS);
        setTimeout(() => {
          if (listStillPending(sessionId)) requestListRefresh(sessionId, attempt + 1);
        }, delay);
      });
}

function onListRefreshed(sessionId) {
  if (!listStillPending(sessionId)) return;
  catchUp.listPending = false;
  finishCatchUp(catchUp);
}

// The copy is set aside: session-view.js fetches the first screen (it owns the first-screen render).
function requestFirstScreenRefetch(open) {
  document.dispatchEvent(new CustomEvent('charliebot:first-screen-refetch', {detail: {open}}));
}

// The socket's replay has ended. event_total is the session's event count at that point.
function onSocketCatchupComplete(eventTotal) {
  const open = catchUp;
  if (open === null || !open.socketPending || open.sessionId !== SESSION_ID) return;
  if (open.copyBytes !== null && open.storedPosition > eventTotal) {
    requestFirstScreenRefetch(open);
    return;
  }
  open.socketPending = false;
  finishCatchUp(open);
}

// One replayed frame of the socket, before its catch-up ends. A copy whose gap needs more bytes than its
// first screen is set aside for the first screen.
function onSocketReplayBytes(bytes) {
  const open = catchUp;
  if (open === null || !open.socketPending || open.copyBytes === null || open.sessionId !== SESSION_ID) return;
  open.replayBytes += bytes;
  if (open.replayBytes > open.copyBytes) requestFirstScreenRefetch(open);
}

function finishCatchUp(open) {
  if (open.socketPending || open.listPending) return;
  catchUp = null;
  setNotCurrent(false);
  document.dispatchEvent(new CustomEvent('charliebot:page-current', {detail: {sessionId: open.sessionId}}));
  if (open.readPending) markSessionRead(open.sessionId);
}

// The socket's events reach the owner here: websocket.js dispatches them for the current socket only. A stored
// sidebar list (filters.js) raises the mark the same way.
document.addEventListener('charliebot:client-socket-replay', (event) => onSocketReplayBytes(event.detail.bytes));
document.addEventListener('charliebot:client-socket-catchup',
    (event) => onSocketCatchupComplete(event.detail.eventTotal));
document.addEventListener('charliebot:stored-list', () => noteStoredList());
