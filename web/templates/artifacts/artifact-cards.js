// ---------------------------------------------------------------------------
// Artifact cards: the loader the chat page's document carries in place of the module.
// ---------------------------------------------------------------------------
// A message shows an artifact card only when it names an artifact page, so a transcript that names none never needs
// the renderer's bytes. This file replaces the include of artifacts/artifact-cards.js, because the template loader
// reads web/templates before the package's templates.
//
// The forward answers every rendered message until the module arrives: it names an artifact page itself, and the
// first message that names one loads the module from there. The module registers its own post-processor when it
// loads, and this forward then does nothing, so no message is processed twice.
globalThis.loadArtifactCards = function() {
  return loadPageScripts('artifacts-cards', ['/static/features/artifacts/artifact-cards.js']);
};

// An artifact page is an artifacts/<name>.html path, in an anchor's href or in the message's text. A message that
// only says "the artifacts/ directory" names none, and it fetches nothing.
const ARTIFACT_PAGE_SOURCE = /artifacts\/[^/\s"'<>]+\.html/;

Chat.registerMessagePostProcessor(function(root) {
  if (typeof Chat.embedLinkedHtmlArtifacts === 'function') return;
  const anchors = Array.from(root.querySelectorAll('[href*="artifacts/"]'));
  const namesArtifact = anchors.some((anchor) => ARTIFACT_PAGE_SOURCE.test(anchor.getAttribute('href') || ''))
    || ARTIFACT_PAGE_SOURCE.test(root.textContent || '');
  if (!namesArtifact) return;
  loadArtifactCards()
    .then(() => Chat.embedLinkedHtmlArtifacts(root))
    .catch((error) => console.error('artifact cards:', error));
});
