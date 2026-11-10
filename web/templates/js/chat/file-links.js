// ---------------------------------------------------------------------------
// File links: the loader the document carries in place of the module.
// ---------------------------------------------------------------------------
// A message's file-server path, its retired `/files/` link and its same-origin file anchor become links, and a file
// the server no longer has gets a missing marker. The scan and its probes are needed only by a message that names
// such a path, so the document carries this loader and the scan stays at web/static/js/chat/file-links.js, which the
// static route serves and the page tests read.
//
// The forward answers every rendered message until the module arrives. A message that names a file path starts the
// download, and the roots that asked for it are marked when the module lands. The module replaces Chat.markFileLinks
// when it runs, and this forward then hands the call to it.
(function() {
  const Chat = globalThis.Chat;
  const SOURCE = '/static/js/chat/file-links.js';

  let loading = null;
  // The rendered roots that asked for the module, marked when it lands.
  let pending = [];

  function load() {
    if (loading === null) {
      loading = loadPageScripts('chat-file-links', [SOURCE]).then(() => {
        const roots = pending;
        pending = [];
        roots.forEach((root) => Chat.markFileLinks(root));
      }, (error) => {
        loading = null;
        console.error('the file links did not load: ' + error.message);
        // A caller that needs the module's helpers (the artifact cards) must not run without them.
        throw error;
      });
    }
    return loading;
  }

  // The file-server prefixes the module links: the served one and the two retired forms.
  const FILE_PATH = /\/(?:absolute_filepath|files|file)\//;
  const TOKEN = /[^\s]+/g;

  // Whether a written link reaches this page's own server. The module links and probes only a same-origin URL; a link
  // to another host or port is another server's, so it asks for nothing.
  function sameOrigin(href) {
    try {
      return new URL(href, window.location.href).origin === window.location.origin;
    } catch (error) {
      return false;
    }
  }

  // Whether one rendered root names a file path. A same-origin anchor is a candidate too: the module probes it for a
  // file the server no longer has, so it needs the module even when the path is not one of the prefixes. The
  // anchors the module never links — a fragment, a query, the page's own `/?session=` links and another origin's
  // URL — ask for nothing.
  function namesFileLink(root) {
    if (!root || typeof root.querySelectorAll !== 'function') return false;
    for (const prose of root.querySelectorAll('.prose-msg')) {
      if (prose.id === 'streaming-msg' || prose.closest('#streaming-msg')) continue;
      for (const anchor of prose.querySelectorAll('a[href]')) {
        const href = anchor.getAttribute('href') || '';
        if (!href || href.charAt(0) === '#' || href.charAt(0) === '?' || href.startsWith('/?')) continue;
        if (sameOrigin(href)) return true;
      }
      const text = prose.textContent || '';
      let token;
      TOKEN.lastIndex = 0;
      while ((token = TOKEN.exec(text)) !== null) {
        if (FILE_PATH.test(token[0]) && sameOrigin(token[0])) return true;
      }
    }
    return false;
  }

  // The page includes this file by the module's name, so the module's own definition of Chat.markFileLinks lands on
  // the page after this one: the call goes to the module once that has happened.
  function markFileLinks(root) {
    if (Chat.markFileLinks !== markFileLinks) {
      Chat.markFileLinks(root);
      return;
    }
    if (!namesFileLink(root)) return;
    if (!pending.includes(root)) pending.push(root);
    // The load reports its own failure; this root stays pending for the next need's retry.
    load().catch(() => {});
  }

  globalThis.loadFileLinks = load;
  Chat.markFileLinks = markFileLinks;
})();
