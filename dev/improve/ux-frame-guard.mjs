// The WebSocket write guard of an improve-ux side. A click on a trial page must not write through a WebSocket to the
// trial's server, so the guard drops each frame that a page sends, at send time. It changes only the page's
// WebSocket.prototype.send. A read is untouched: the server's frames, the page's onmessage handlers and its event
// listeners keep receiving, and the page's subscribe frame still goes out, so the page's reads keep flowing.
//
// The rule is deny by default. The one frame that passes is `subscribe` on a session socket, which is the read path.
// The server takes no other frame from a session socket as input, and a chat message is a POST, which the driver's
// fetch intercept records. Every frame of a terminal socket (pty_input, pty_resize) and of a voice socket is a write,
// and a socket on any other path is dropped too. Each dropped frame is recorded on the page as
// `globalThis.__uxWriteGuard.dropped`, with its path and frame type.
//
// The driver installs the guard with installWriteGuard before the page's first navigation, so the guard runs before any
// page script opens a socket. `node --test dev/improve/ux-frame-guard.test.mjs` runs the guard against a fake socket.

// The guard as the page runs it. The page evaluates this function's source alone, so it names no module binding.
export function writeGuard() {
  const nativeSend = WebSocket.prototype.send;
  const record = { dropped: [] };
  globalThis.__uxWriteGuard = record;
  WebSocket.prototype.send = function send(data) {
    const path = String(this.url).replace(/^[a-z][a-z0-9+.-]*:\/\/[^/?#]*/i, '').split(/[?#]/)[0];
    if (path.startsWith('/ws/sessions/') && typeof data === 'string' && /^\{"type":"subscribe"[,}]/.test(data)) {
      return nativeSend.call(this, data);
    }
    const type = typeof data === 'string' ? (/"type":"([^"]*)"/.exec(data)?.[1] ?? 'text') : 'binary';
    record.dropped.push({ path, type });
  };
}

// installWriteGuard SEND installs the guard on the page of the CDP session that SEND sends to: SEND(method, params).
export async function installWriteGuard(send) {
  await send('Page.addScriptToEvaluateOnNewDocument', { source: `(${writeGuard})();` });
}
