
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
Chat.expose([
  'showScrollToBottom',
  'hideScrollToBottom',
  'scrollToBottom',
]);

})();
