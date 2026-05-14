// ---------------------------------------------------------------------------
// TUI session frontend — mounts xterm.js into #tui-container when the active
// session's backend.type === 'tui-cli', and bridges WS pty_input / pty_output
// / pty_resize / pty_exit messages to the terminal.
// ---------------------------------------------------------------------------
(function() {
  let term = null;
  let fitAddon = null;
  let resizeObs = null;
  let activeSessionId = null;
  let manualStopBannerShown = false;

  function getContainer() {
    return document.getElementById('tui-container');
  }

  function isTuiBackendType(t) {
    return t === 'tui-cli';
  }

  function activeBackendType() {
    return globalThis.ACTIVE_BACKEND_TYPE || '';
  }

  function isTuiActive() {
    return isTuiBackendType(activeBackendType());
  }

  function setChatChromeHidden(hidden) {
    // Hide the message list. We keep the tab buttons, header, and worker tab
    // intact so the user can still see workers/usage if any.
    const ids = ['messages'];
    for (const id of ids) {
      const el = document.getElementById(id);
      if (el) el.classList.toggle('hidden', hidden);
    }
    // The input bar is the direct sibling of #messages — we identify it by
    // the textarea inside.
    const input = document.getElementById('msg-input');
    if (input) {
      const inputBar = input.closest('.border-t');
      if (inputBar) inputBar.classList.toggle('hidden', hidden);
    }
    const streaming = document.getElementById('streaming-msg');
    if (streaming) streaming.classList.toggle('hidden', hidden);
  }

  function encodeBytesB64(strOrBytes) {
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
  }

  function decodeB64ToBytes(b64) {
    const binary = atob(b64);
    const bytes = new Uint8Array(binary.length);
    for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
    return bytes;
  }

  function wsSendJson(obj) {
    if (typeof ws !== 'undefined' && ws && ws.readyState === WebSocket.OPEN) {
      try {
        ws.send(JSON.stringify(obj));
      } catch (err) {
        console.warn('TUI ws send failed', err);
      }
    }
  }

  function sendInput(data) {
    wsSendJson({type: 'pty_input', data: encodeBytesB64(data)});
  }

  function sendResize(cols, rows) {
    wsSendJson({type: 'pty_resize', cols, rows});
  }

  function fitAndSendResize() {
    if (!fitAddon || !term) return;
    try {
      fitAddon.fit();
      if (term.cols && term.rows) sendResize(term.cols, term.rows);
    } catch (err) {
      console.warn('TUI terminal fit failed', err);
    }
  }

  function scheduleFitAndSendResize() {
    requestAnimationFrame(() => requestAnimationFrame(fitAndSendResize));
  }

  function ensureMount(sessionId) {
    const container = getContainer();
    if (!container) return false;
    if (!window.Terminal || !window.FitAddon) {
      console.error('xterm.js or FitAddon not loaded');
      return false;
    }
    if (term && activeSessionId === sessionId) {
      // Already mounted for this session.
      scheduleFitAndSendResize();
      return true;
    }
    unmount();
    activeSessionId = sessionId;
    manualStopBannerShown = false;

    container.classList.remove('hidden');
    setChatChromeHidden(true);

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

    // Defer once until after the initial layout/paint, then again.
    scheduleFitAndSendResize();

    // Re-fit after monospace font has loaded; xterm cell metrics depend on it.
    if (document.fonts && document.fonts.ready) {
      document.fonts.ready.then(fitAndSendResize);
    }

    resizeObs = new ResizeObserver(() => {
      fitAndSendResize();
    });
    resizeObs.observe(container);
    return true;
  }

  function unmount() {
    if (resizeObs) {
      try { resizeObs.disconnect(); } catch (e) {}
      resizeObs = null;
    }
    if (term) {
      try { term.dispose(); } catch (e) {}
      term = null;
    }
    fitAddon = null;
    activeSessionId = null;
    const container = getContainer();
    if (container) {
      container.classList.add('hidden');
      container.innerHTML = '';
    }
    setChatChromeHidden(false);
  }

  function onPtyOutput(b64) {
    if (!term) return;
    try {
      const bytes = decodeB64ToBytes(b64);
      term.write(bytes);
    } catch (err) {
      console.warn('pty_output decode failed', err);
    }
  }

  function onPtyExit() {
    if (!term) return;
    if (manualStopBannerShown) return;
    term.write('\r\n\x1b[33m[claude exited — refresh the page to restart]\x1b[0m\r\n');
  }

  function showStoppedBanner() {
    manualStopBannerShown = true;
    const message = 'claude stopped — reopen to resume';
    if (term) {
      term.write('\r\n\x1b[33m' + message + '\x1b[0m\r\n');
      return;
    }
    const container = getContainer();
    if (!container) return;
    container.classList.remove('hidden');
    container.innerHTML = '<div class="h-full flex items-center justify-center text-sm text-amber-300">' + message + '</div>';
  }

  function syncBackend(backendType, sessionId) {
    globalThis.ACTIVE_BACKEND_TYPE = backendType || '';
    if (isTuiBackendType(backendType)) {
      ensureMount(sessionId);
    } else {
      unmount();
    }
  }

  // Hook into the WS lifecycle: when the socket opens, if we're on a TUI
  // session, mount the terminal so it's ready to receive pty_output (the
  // server starts streaming once the WS receives our cursor message).
  function onWsOpenIfTui() {
    if (!isTuiActive() || !SESSION_ID) return;
    ensureMount(SESSION_ID);
  }

  // Public API.
  globalThis.TuiSession = {
    isTuiActive,
    isTuiBackendType,
    syncBackend,
    ensureMount,
    unmount,
    onPtyOutput,
    onPtyExit,
    onWsOpenIfTui,
    showStoppedBanner,
  };

  // Initial mount on page load if the SSR-rendered active session is a TUI.
  document.addEventListener('DOMContentLoaded', () => {
    if (isTuiActive() && SESSION_ID) {
      ensureMount(SESSION_ID);
    }
  });
})();
