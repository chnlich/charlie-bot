
(function() {
  const Chat = globalThis.Chat;

// ---------------------------------------------------------------------------
// Scroll-to-bottom floating button
// ---------------------------------------------------------------------------
function showScrollToBottom() {
  const btn = document.getElementById('scroll-to-bottom');
  if (btn && btn.classList.contains('hidden')) btn.classList.remove('hidden');
}

function hideScrollToBottom() {
  const btn = document.getElementById('scroll-to-bottom');
  if (btn && !btn.classList.contains('hidden')) btn.classList.add('hidden');
}

function scrollToBottom() {
  const container = document.getElementById('messages');
  if (container) container.scrollTop = container.scrollHeight;
  hideScrollToBottom();
}

// Shared post-render scroll rule. *wasAtBottom* is the pin state each render
// path captured before mutating the container: reading it afterwards would
// see the grown scrollHeight. A render landing while the reader is pinned
// keeps them at the bottom; one landing while they scrolled up raises the
// jump button instead of yanking them down.
function restoreBottomPin(container, wasAtBottom, forceScroll) {
  if (forceScroll || wasAtBottom) {
    container.scrollTop = container.scrollHeight;
  } else {
    showScrollToBottom();
  }
}


// Hide the button when user scrolls back to bottom
document.addEventListener('DOMContentLoaded', () => {
  Chat.initializeRoundRatings();
  const container = document.getElementById('messages');
  if (container) {
    container.addEventListener('scroll', () => {
      if (shouldAutoScroll(container)) hideScrollToBottom();
    });
  }
});

Chat.showScrollToBottom = showScrollToBottom;
Chat.hideScrollToBottom = hideScrollToBottom;
Chat.scrollToBottom = scrollToBottom;
Chat.restoreBottomPin = restoreBottomPin;
Chat.expose([
  'showScrollToBottom',
  'hideScrollToBottom',
  'scrollToBottom',
  'restoreBottomPin',
]);

})();
