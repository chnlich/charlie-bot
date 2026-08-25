// ---------------------------------------------------------------------------
// Shared xterm.js mount for the two terminal surfaces (tui_session.js,
// terminal_panel.js): both mount identical Terminal options, the same
// open/clipboard/focus sequence, and the same touch-drag scroll wiring.
// ---------------------------------------------------------------------------
globalThis.createTerminal = function(container) {
  const term = new Terminal({
    cursorBlink: true,
    fontSize: 13,
    fontFamily: '"Fira Code", ui-monospace, SFMono-Regular, Menlo, Consolas, monospace',
    theme: {background: '#000000', foreground: '#e2e8f0'},
    scrollback: 50000,
    convertEol: false,
    allowProposedApi: true,
  });
  const fitAddon = new FitAddon.FitAddon();
  term.loadAddon(fitAddon);
  term.open(container);
  wireTerminalClipboard(term);
  term.focus();
  return {term, fitAddon};
};

globalThis.wireTerminalTouchScroll = function(container, term, listenerOptions) {
  const opts = Object.assign({passive: true}, listenerOptions);
  let lastTouchY = null;
  container.addEventListener('touchstart', e => {
    if (e.touches && e.touches.length > 0) lastTouchY = e.touches[0].screenY;
  }, opts);
  container.addEventListener('touchmove', e => {
    if (lastTouchY == null) return;
    if (!e.changedTouches || e.changedTouches.length === 0) return;
    const y = e.changedTouches[0].screenY;
    const deltaY = y - lastTouchY;
    lastTouchY = y;
    if (term && typeof term.scrollLines === 'function') {
      term.scrollLines(-Math.round(deltaY / 10));
    }
  }, opts);
  container.addEventListener('touchend', () => { lastTouchY = null; }, opts);
  container.addEventListener('touchcancel', () => { lastTouchY = null; }, opts);
};
