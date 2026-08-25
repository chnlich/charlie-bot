// ---------------------------------------------------------------------------
// Shared base64 byte-bridge helpers for the xterm.js terminal surfaces
// (tui_session.js, terminal_panel.js): pty_input frames carry base64 UTF-8
// bytes, pty_output frames decode back to bytes for term.write.
// ---------------------------------------------------------------------------
globalThis.encodeBytesB64 = function(strOrBytes) {
  // term.onData passes a string of utf-8-ish characters; encode as UTF-8
  // bytes and then base64.
  let bytes;
  if (typeof strOrBytes === 'string') {
    bytes = new TextEncoder().encode(strOrBytes);
  } else {
    bytes = strOrBytes;
  }
  let binary = '';
  const chunk = 0x8000;
  for (let i = 0; i < bytes.length; i += chunk) {
    binary += String.fromCharCode.apply(null, bytes.subarray(i, i + chunk));
  }
  return btoa(binary);
};

globalThis.decodeB64ToBytes = function(b64) {
  const binary = atob(b64);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
  return bytes;
};
