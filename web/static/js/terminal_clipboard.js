// ---------------------------------------------------------------------------
// Shared clipboard wiring for the xterm.js terminal surfaces.
// Copy-on-select: releasing a non-empty mouse selection copies it. Ctrl+Shift+V pastes.
// Ctrl+Shift+C is intentionally unused -- Chrome reserves it for the DevTools
// inspector and page JS cannot preventDefault it, so the mouse owns copy.
// ---------------------------------------------------------------------------
globalThis.wireTerminalClipboard = function(term) {
  term.element.addEventListener('mouseup', () => {
    const sel = term.getSelection();
    if (!sel.trim()) return;                          // skip empty/whitespace selections so the clipboard is not clobbered
    navigator.clipboard.writeText(sel)
      .catch(err => console.warn('terminal copy failed', err));
  });
  term.attachCustomKeyEventHandler(e => {
    if (e.type === 'keydown' && e.ctrlKey && e.shiftKey && e.code === 'KeyV') {
      e.preventDefault();                              // stop the browser's native paste
      navigator.clipboard.readText()
        .then(t => term.paste(t))                      // term.paste -> bracketed-paste safe
        .catch(err => console.warn('terminal paste failed', err));
      return false;
    }
    return true;                                       // Ctrl+C and everything else pass through to the PTY
  });
};
