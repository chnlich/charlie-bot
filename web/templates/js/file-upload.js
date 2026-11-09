// ---------------------------------------------------------------------------
// File upload: the loader the document carries in place of the module.
// ---------------------------------------------------------------------------
// A reader meets the attachment UI only through the composer's Attach button, a
// paste that carries a file, or a screenshot paste. The upload code stays at
// web/static/js/file-upload.js, which the static route serves and the page tests
// read. This file is what the document carries: the loader's search path reads
// web/templates before web/static, and the page includes the same name.
//
// The first attachment action fetches the module: the Attach button's pick, a
// pasted screenshot, or an input-work window another feature opens. Until it
// lands, the composer's gate reads an empty attachment list and no upload in
// flight, which is what it would read anyway: a file can exist only once the
// module that uploads it has loaded. Each wired name is a function declaration,
// so the module's own declaration replaces this one when it runs, and a call
// that arrives while the download runs waits for it.
(function() {
  const SOURCE = '/static/js/file-upload.js';
  let loading = null;
  let ready = false;

  // The load's own rejection handler logs the failure and drops the memo, so the
  // next trigger retries. The original promise stays rejecting, so a caller can
  // tell a landed module from a failed download.
  function load() {
    if (loading === null) {
      loading = loadPageScripts('file-upload', [SOURCE]);
      loading.then(
        () => { ready = true; },
        (error) => {
          loading = null;
          console.error('the upload module did not load: ' + error.message);
        },
      );
    }
    return loading;
  }

  // load() logs a failed download; a call it loses waits for the next trigger.
  function forwardInputWork(name, message) {
    load().then(() => globalThis[name](message), () => {});
  }

  globalThis.getUploadedFilesForPayload = () => [];
  globalThis.toPayloadFiles = () => [];
  globalThis.clearSentUploadedFiles = () => {};
  globalThis.blockIfUploadsInFlight = () => false;
  globalThis.renderFileChips = () => {};
  globalThis.refreshSendLock = () => {};
  globalThis.inputWorkStarted = (message) => forwardInputWork('inputWorkStarted', message);
  globalThis.inputWorkSettled = (message) => forwardInputWork('inputWorkSettled', message);
  globalThis.handleFiles = (input) => {
    load().then(() => globalThis.handleFiles(input), () => {});
  };

  // A screenshot pasted before the module lands would be lost without this
  // listener; the module attaches its own when it loads, and this one stands
  // down once that happened.
  function onPaste(event) {
    if (ready) return;
    load().then(() => globalThis.handlePaste(event), () => {});
  }
  const input = document.getElementById('msg-input');
  if (input) input.addEventListener('paste', onPaste);
})();
