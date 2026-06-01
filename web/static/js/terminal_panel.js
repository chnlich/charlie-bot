// ---------------------------------------------------------------------------
// Host-global terminal frontend — mounts xterm.js into #tab-terminal and
// bridges /ws/terminal pty_input / pty_output / pty_resize / pty_exit.
// ---------------------------------------------------------------------------
(function() {
  let term = null;
  let fitAddon = null;
  let resizeObs = null;
  let socket = null;
  let reconnectTimer = null;
  let reconnectDelay = 1000;
  let terminalOpen = false;

  function getContainer() {
    return document.getElementById('tab-terminal');
  }

  function encodeBytesB64(strOrBytes) {
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
  }

  function decodeB64ToBytes(b64) {
    const binary = atob(b64);
    const bytes = new Uint8Array(binary.length);
    for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
    return bytes;
  }

  function sendJson(obj) {
    if (!socket || socket.readyState !== WebSocket.OPEN) return;
    try {
      socket.send(JSON.stringify(obj));
    } catch (err) {
      console.warn('Terminal ws send failed', err);
    }
  }

  function sendInput(data) {
    sendJson({type: 'pty_input', data: encodeBytesB64(data)});
  }

  function sendResize(cols, rows) {
    sendJson({type: 'pty_resize', cols, rows});
  }

  function fitAndSendResize() {
    if (!terminalOpen || !fitAddon || !term) return;
    try {
      fitAddon.fit();
      if (term.cols && term.rows) sendResize(term.cols, term.rows);
    } catch (err) {
      console.warn('Terminal fit failed', err);
    }
  }

  function scheduleFitAndSendResize() {
    requestAnimationFrame(() => requestAnimationFrame(fitAndSendResize));
  }

  function ensureMount() {
    const container = getContainer();
    if (!container) return false;
    if (!window.Terminal || !window.FitAddon) {
      console.error('xterm.js or FitAddon not loaded');
      return false;
    }
    if (term) {
      scheduleFitAndSendResize();
      term.focus();
      return true;
    }

    term = new Terminal({
      cursorBlink: true,
      fontSize: 13,
      fontFamily: '"Fira Code", ui-monospace, SFMono-Regular, Menlo, Consolas, monospace',
      theme: {background: '#000000', foreground: '#e2e8f0'},
      scrollback: 50000,
      convertEol: false,
      allowProposedApi: true,
    });
    fitAddon = new FitAddon.FitAddon();
    term.loadAddon(fitAddon);
    term.open(container);
    term.focus();
    term.onData(sendInput);

    let lastTouchY = null;
    container.addEventListener('touchstart', e => {
      if (e.touches && e.touches.length > 0) lastTouchY = e.touches[0].screenY;
    }, {passive: true});
    container.addEventListener('touchmove', e => {
      if (lastTouchY == null) return;
      if (!e.changedTouches || e.changedTouches.length === 0) return;
      const y = e.changedTouches[0].screenY;
      const deltaY = y - lastTouchY;
      lastTouchY = y;
      if (term && typeof term.scrollLines === 'function') {
        term.scrollLines(-Math.round(deltaY / 10));
      }
    }, {passive: true});
    container.addEventListener('touchend', () => { lastTouchY = null; }, {passive: true});
    container.addEventListener('touchcancel', () => { lastTouchY = null; }, {passive: true});

    scheduleFitAndSendResize();
    if (document.fonts && document.fonts.ready) {
      document.fonts.ready.then(fitAndSendResize);
    }

    resizeObs = new ResizeObserver(fitAndSendResize);
    resizeObs.observe(container);
    window.addEventListener('resize', scheduleFitAndSendResize);
    return true;
  }

  function connect() {
    if (socket && (socket.readyState === WebSocket.OPEN || socket.readyState === WebSocket.CONNECTING)) return;
    if (reconnectTimer) {
      clearTimeout(reconnectTimer);
      reconnectTimer = null;
    }
    const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
    let url = `${proto}//${location.host}/ws/terminal`;
    const accessKey = localStorage.getItem('charliebot_access_key');
    if (accessKey) url += '?token=' + encodeURIComponent(accessKey);

    socket = new WebSocket(url);
    socket.onopen = () => {
      reconnectDelay = 1000;
      ensureMount();
      scheduleFitAndSendResize();
    };
    socket.onmessage = e => {
      let data;
      try { data = JSON.parse(e.data); } catch { return; }
      if (data.type === 'ping') return;
      if (data.type === 'pty_output') {
        if (!term) return;
        try {
          term.write(decodeB64ToBytes(data.data || ''));
        } catch (err) {
          console.warn('terminal pty_output decode failed', err);
        }
      } else if (data.type === 'pty_exit') {
        if (term) term.write('\r\n\x1b[33m[terminal detached]\x1b[0m\r\n');
      }
    };
    socket.onclose = () => {
      socket = null;
      if (!terminalOpen) return;
      reconnectTimer = setTimeout(connect, reconnectDelay);
      reconnectDelay = Math.min(reconnectDelay * 2, 30000);
    };
    socket.onerror = () => {
      if (socket) socket.close();
    };
  }

  function show() {
    terminalOpen = true;
    const container = getContainer();
    if (container) container.classList.remove('hidden');
    if (!ensureMount()) return;
    connect();
    scheduleFitAndSendResize();
    term.focus();
  }

  function hide() {
    terminalOpen = false;
    const container = getContainer();
    if (container) container.classList.add('hidden');
  }

  globalThis.TerminalPanel = {
    show,
    hide,
  };
})();
