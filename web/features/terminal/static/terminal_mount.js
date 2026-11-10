// ---------------------------------------------------------------------------
// Shared xterm.js mount for the terminal surfaces (terminal_panel.js). It
// carries the Terminal options, the open/clipboard/focus sequence, the
// touch-drag scroll wiring, the fit-then-send-resize timing, and the pty_input
// / pty_resize message shapes.
// ---------------------------------------------------------------------------

// Both mounts bail when the xterm.js script tags failed to load; a missing
// Terminal global would otherwise surface as a TypeError deep in createTerminal.
globalThis.terminalScriptsReady = function() {
  if (window.Terminal && window.FitAddon) return true;
  console.error('xterm.js or FitAddon not loaded');
  return false;
};

globalThis.createTerminal = function(container) {
  const term = new Terminal({
    cursorBlink: true,
    fontSize: 13,
    fontFamily: '"Fira Code", ui-monospace, SFMono-Regular, Menlo, Consolas, monospace',
    theme: {background: '#000000', foreground: '#e2e8f0'},
    // Terminal text is 13px, so it takes the 4.5:1 floor. xterm's minimumContrastRatio lifts a
    // non-dim cell's foreground to that floor; the shell's black-on-green status line measures
    // 3.58:1 in the default palette. xterm gives a dim cell half the ratio by design.
    minimumContrastRatio: 4.5,
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

globalThis.fitTerminalAndSendResize = function(term, fitAddon, sendResize) {
  try {
    fitAddon.fit();
    if (term.cols && term.rows) sendResize(term.cols, term.rows);
  } catch (err) {
    console.warn('terminal fit failed', err);
  }
};

// Cell metrics are only valid after the container's layout has painted, so
// every caller (mount, font load, container resize) defers across two frames.
globalThis.scheduleAfterTerminalPaint = function(fn) {
  requestAnimationFrame(() => requestAnimationFrame(fn));
};

// The pty message shapes are the ws contract; both surfaces emit them over
// their own socket wrapper, so the wrapper is the only parameter.
globalThis.makePtySenders = function(sendJson) {
  return {
    sendInput(data) {
      sendJson({type: 'pty_input', data: encodeBytesB64(data)});
    },
    sendResize(cols, rows) {
      sendJson({type: 'pty_resize', cols, rows});
    },
  };
};

// The fit glue both surfaces share: fit the surface's terminal, then push the
// fitted grid over its socket. The surfaces keep their own terminal refs; the
// bridge reads them through the getters instead of holding copies. The mayFit
// callback carries the surface's extra guard: the panel only fits while its
// tab is open. The onFitted callback runs after each fit the guard admitted,
// so a surface can act on the measured grid. The panel opens its socket only
// once the grid is known, so the pty starts at the size the user sees.
globalThis.makeTerminalFitBridge = function(sendResize, getTerm, getFitAddon, mayFit, onFitted) {
  function fitAndSendResize() {
    if (mayFit && !mayFit()) return;
    const term = getTerm();
    const fitAddon = getFitAddon();
    if (!fitAddon || !term) return;
    fitTerminalAndSendResize(term, fitAddon, sendResize);
    if (onFitted) onFitted();
  }
  function scheduleFitAndSendResize() {
    scheduleAfterTerminalPaint(fitAndSendResize);
  }
  return {fitAndSendResize, scheduleFitAndSendResize};
};

// Post-mount relayout: fit once after layout paint, again when the mono font
// (and so xterm's cell metrics) settles, and on every container resize.
// Returns the observer so a surface that remounts can disconnect the old one.
globalThis.wireTerminalRelayout = function(container, fitAndSendResize) {
  scheduleAfterTerminalPaint(fitAndSendResize);
  if (document.fonts && document.fonts.ready) {
    document.fonts.ready.then(fitAndSendResize);
  }
  const resizeObs = new ResizeObserver(fitAndSendResize);
  resizeObs.observe(container);
  return resizeObs;
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
