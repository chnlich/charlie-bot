// ---------------------------------------------------------------------------
// File upload: the loader the document carries in place of the module.
// ---------------------------------------------------------------------------
// A reader meets the attachment UI only through the composer's Attach button, a
// paste that carries an image, or an input-work window another feature opens.
// The upload code stays at web/static/js/file-upload.js, which the static route
// serves and the page tests read. This file is what the document carries: the
// loader's search path reads web/templates before web/static, and the page
// includes the same name.
//
// The first attachment action fetches the module: the Attach button's pick, a
// pasted image, or an input-work window. Until it lands, the work that trigger
// started stays here, held: the send gate refuses while a file waits to be
// uploaded or an input-work window is open. The send button shows it, so a
// message cannot leave without its attachment or during a transcription. Each
// wired name is a function declaration, so the module's own declaration
// replaces this one when it runs. The held work moves to the module in the
// same turn the download lands.
(function() {
  const SOURCE = '/static/js/file-upload.js';
  let loading = null;
  let ready = false;

  // The work the triggers started before the module landed: files waiting to
  // be uploaded, and input-work windows another feature opened, by message.
  let pendingFiles = 0;
  const pendingWork = new Map();

  // The sentence the module's gate shows for an open upload window.
  const UPLOAD_PENDING_MESSAGE = 'Please wait for the attachment upload to finish';

  // A failed download drops the memo and releases the held work, so the next
  // trigger retries and the composer never stays locked behind a module that
  // will not arrive.
  function load() {
    if (loading === null) {
      loading = loadPageScripts('file-upload', [SOURCE]);
      loading.then(
        () => { ready = true; },
        (error) => {
          loading = null;
          pendingFiles = 0;
          pendingWork.clear();
          globalThis.refreshSendLock();
          console.error('the upload module did not load: ' + error.message);
        },
      );
    }
    return loading;
  }

  function gateMessage() {
    if (pendingFiles > 0) return UPLOAD_PENDING_MESSAGE;
    return pendingWork.size ? pendingWork.keys().next().value : null;
  }

  // The send button's single disabled writer until the module lands, as the
  // module's refreshSendLock is after it. Once the module is evaluated this
  // global is the module's, which reads the counters it now owns.
  globalThis.refreshSendLock = function refreshSendLock() {
    const btn = document.getElementById('send-btn');
    if (!btn) return;
    if (gateMessage() === null) {
      btn.removeAttribute('disabled');
      btn.classList.remove('opacity-50', 'cursor-not-allowed');
      btn.classList.add('hover:bg-blue-500');
      return;
    }
    btn.setAttribute('disabled', '');
    btn.classList.add('opacity-50', 'cursor-not-allowed');
    btn.classList.remove('hover:bg-blue-500');
  };

  globalThis.getUploadedFilesForPayload = () => [];
  globalThis.toPayloadFiles = () => [];
  globalThis.clearSentUploadedFiles = () => {};
  globalThis.blockIfUploadsInFlight = () => {
    const message = gateMessage();
    if (message === null) return false;
    showToast(message, true);
    return true;
  };
  globalThis.renderFileChips = () => {};

  globalThis.inputWorkStarted = (message) => {
    pendingWork.set(message, (pendingWork.get(message) || 0) + 1);
    globalThis.refreshSendLock();
    load().then(() => {
      const open = pendingWork.get(message) || 0;
      for (let index = 0; index < open; index += 1) globalThis.inputWorkStarted(message);
      pendingWork.delete(message);
      globalThis.refreshSendLock();
    }, () => {});
  };
  globalThis.inputWorkSettled = (message) => {
    if (ready) {
      globalThis.inputWorkSettled(message);
      return;
    }
    const open = (pendingWork.get(message) || 0) - 1;
    if (open > 0) pendingWork.set(message, open);
    else pendingWork.delete(message);
    globalThis.refreshSendLock();
  };

  globalThis.handleFiles = (input) => {
    pendingFiles += input && input.files ? input.files.length : 0;
    globalThis.refreshSendLock();
    load().then(() => {
      pendingFiles = 0;
      globalThis.handleFiles(input);
      globalThis.refreshSendLock();
    }, () => {});
  };

  // A pasted image would be lost without this listener. The default action is
  // cancelled here, while the event dispatches, because the module's own
  // preventDefault would run after the paste has already inserted its text.
  // Only an image paste downloads the module; any other paste is the input's.
  function onPaste(event) {
    if (ready) return;
    const data = event.clipboardData;
    const items = data && data.items ? Array.from(data.items) : [];
    const images = items.filter((item) => item.kind === 'file' && item.type && item.type.startsWith('image/'));
    if (!images.length) return;
    event.preventDefault();
    pendingFiles += images.length;
    globalThis.refreshSendLock();
    load().then(() => {
      pendingFiles = 0;
      globalThis.handlePaste(event);
      globalThis.refreshSendLock();
    }, () => {});
  }
  const input = document.getElementById('msg-input');
  if (input) input.addEventListener('paste', onPaste);
})();
