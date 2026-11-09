// ---------------------------------------------------------------------------
// Worker summary card: the loader the document carries in place of the card.
// ---------------------------------------------------------------------------
// Only a worker session's For-you view holds this card; every other session
// shows none. The card's module stays at web/static/js/chat/worker-summary.js,
// which the static route serves and the page tests read. This file is what the
// document carries: the loader's search path reads web/templates before
// web/static, and the page includes the same name.
//
// The card's node lives here until the module adopts it: the For-you list asks
// workerSummaryCard for a node while the module may still be travelling, so
// this node stands in the list and the module fills the node already on the
// page. A summary that arrives before the module does is kept, and the module
// paints it on arrival.
(function() {
  const Chat = globalThis.Chat;
  const SOURCE = '/static/js/chat/worker-summary.js';

  const card = document.createElement('div');
  card.className = 'worker-summary w-full rounded-xl border border-slate-700 bg-slate-800/70 px-4 py-3 space-y-3 text-sm';

  // The summary the last session render or poll carried, and whether it was a
  // session mount, which resets the folds. Only a worker session carries one.
  let pending = null;
  let pendingMount = false;
  let loading = null;

  // A failed download drops the memo, so the next summary retries.
  function load() {
    if (loading === null) {
      loading = loadPageScripts('chat-worker-summary', [SOURCE]);
      loading.catch((error) => {
        loading = null;
        console.error('the worker summary card did not load: ' + error.message);
      });
    }
    return loading;
  }

  function applyWorkerSummary(summary) {
    pending = summary || null;
    pendingMount = false;
    if (pending) load();
  }

  function mountWorkerSummary(summary) {
    pending = summary || null;
    pendingMount = true;
    if (pending) load();
  }

  function workerSummaryCard() {
    return pending ? card : null;
  }

  // A click on a fold while the module travels does nothing: the card it would
  // fold is still being built.
  function toggleWorkerSummaryFold() {}

  // The module reads this entry to adopt the node and to paint the summary a
  // render or poll carried before it landed.
  Chat.__workerSummaryLoader = {
    card,
    take() {
      return {summary: pending, mount: pendingMount};
    },
  };

  Chat.ForYou.registerSummaryCardRenderer(workerSummaryCard);
  Chat.wire({applyWorkerSummary, mountWorkerSummary, workerSummaryCard, toggleWorkerSummaryFold}, {});

  // A worker session needs the card on its first screen, so its download starts
  // while the document is still parsing. Any other session never asks for it.
  const session = globalThis.SESSION_BOOTSTRAP && globalThis.SESSION_BOOTSTRAP.session;
  if (session && session.profile === 'worker') load();
})();
