// ---------------------------------------------------------------------------
// Shared clipboard wiring for the xterm.js terminal surfaces.
// Ctrl+Shift+C copies the current selection; Ctrl+Shift+V pastes.
// ---------------------------------------------------------------------------
globalThis.wireTerminalClipboard = function(term) {
  term.attachCustomKeyEventHandler(e => {
    if (e.type !== 'keydown' || !e.ctrlKey || !e.shiftKey) return true;
    if (e.code === 'KeyC' && term.hasSelection()) {
      e.preventDefault();                              // must preventDefault: return false alone does NOT stop the browser DevTools default
      navigator.clipboard.writeText(term.getSelection())
        .catch(err => console.warn('terminal copy failed', err));
      return false;                                    // do not forward to PTY
    }
    if (e.code === 'KeyV') {
      e.preventDefault();                              // stop the browser's native paste
      navigator.clipboard.readText()
        .then(t => term.paste(t))                      // term.paste -> bracketed-paste safe
        .catch(err => console.warn('terminal paste failed', err));
      return false;
    }
    return true;                                       // everything else passes through to the PTY
  });
};
