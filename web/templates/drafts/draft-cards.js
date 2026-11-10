// ---------------------------------------------------------------------------
// Draft cards: the loader the document carries in place of the module.
// ---------------------------------------------------------------------------
// A draft card is the operator's own message waiting for the operator's click.
// A page carries one only when a draft message is on its first screen. The
// drafts page part includes this loader, and the module stays at
// web/features/drafts/static/draft-cards.js, which the static route serves and
// the page tests read. The template loader reads web/templates before the
// package trees, so the include name finds this file.
//
// The first draft message starts the download. Each placeholder keeps the
// message's place in the list, and the loader hands the module the messages it
// stood in for. The module's own renderers take the three roles over and fill
// the placeholders when it lands. A failed download drops the memo, so the next
// draft message retries.
(function() {
  const Chat = globalThis.Chat;
  const SOURCE = '/static/features/drafts/draft-cards.js';

  let loading = null;
  const pending = [];
  Chat.__draftCardsPending = pending;

  function placeholder(msg, sessionId) {
    const id = 'draft-pending-' + pending.length;
    pending.push({id, msg, sessionId});
    if (loading === null) {
      loading = loadPageScripts('drafts-draft-cards', [SOURCE]);
      loading.catch((error) => {
        loading = null;
        console.error('the draft cards did not load: ' + error.message);
      });
    }
    return '<div id="' + id + '"></div>';
  }

  Chat.replaceMessageRoleRenderer('draft', placeholder);
  Chat.replaceMessageRoleRenderer('draft_sent', placeholder);
  Chat.replaceMessageRoleRenderer('draft_failed', placeholder);
})();
