// ---------------------------------------------------------------------------
// Shared clipboard wiring for the xterm.js terminal surfaces.
// Copy-on-select: releasing a non-empty mouse selection copies it. Ctrl+Shift+V pastes.
// Ctrl+Shift+C is intentionally unused -- Chrome reserves it for the DevTools
// inspector and page JS cannot preventDefault it, so the mouse owns copy.
// ---------------------------------------------------------------------------
globalThis.wireTerminalClipboard = function(term) {
  const ownerDocument = term.element.ownerDocument;
  let badge = null;
  let badgeTimer = null;

  function showCopyFeedback(message) {
    if (!badge) {
      const container = term.element.parentElement;
      container.style.position = 'relative';
      badge = ownerDocument.createElement('div');
      Object.assign(badge.style, {
        position: 'absolute',
        top: '8px',
        right: '8px',
        zIndex: '10',
        padding: '4px 8px',
        borderRadius: '4px',
        background: 'rgba(15, 23, 42, 0.9)',
        color: '#e2e8f0',
        fontSize: '12px',
        pointerEvents: 'none',
      });
      container.appendChild(badge);
    }
    badge.textContent = message;
    badge.style.display = 'block';
    clearTimeout(badgeTimer);
    badgeTimer = setTimeout(() => { badge.style.display = 'none'; }, 1000);
  }

  function copyText(text) {
    navigator.clipboard.writeText(text)
      .then(() => showCopyFeedback('Copied'))
      .catch(err => {
        console.warn('terminal copy failed', err);
        showCopyFeedback('Copy failed');
      });
  }

  term.parser.registerOscHandler(52, data => {
    const separator = data.indexOf(';');
    const payload = separator === -1 ? data : data.substring(separator + 1);
    if (!payload || payload === '?') return true;
    try {
      const binary = atob(payload);
      const bytes = new Uint8Array(binary.length);
      for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
      copyText(new TextDecoder('utf-8').decode(bytes));
    } catch (err) {
      console.warn('terminal OSC 52 decode failed', err);
    }
    return true;
  });

  function copySelection() {
    const sel = term.getSelection();
    if (!sel.trim()) return;                          // skip empty/whitespace selections so the clipboard is not clobbered
    copyText(sel);
  }
  term.loadAddon({
    activate() { ownerDocument.addEventListener('mouseup', copySelection); },
    dispose() {
      ownerDocument.removeEventListener('mouseup', copySelection);
      clearTimeout(badgeTimer);
      if (badge) badge.remove();
    },
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
