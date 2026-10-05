
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
  const engine = Chat.TurnEngine && container ? Chat.TurnEngine.activeFor(container) : null;
  if (engine) {
    // The engine owns the pin state: the jump re-arms following and marks its
    // own write so the echo is not read as user activity.
    engine.jumpToBottom();
  } else if (container) {
    container.scrollTop = container.scrollHeight;
  }
  hideScrollToBottom();
}

// Shared post-render scroll rule. *wasAtBottom* is the pin state each render
// path captured before mutating the container: reading it afterwards would
// see the grown scrollHeight. A render landing while the reader is pinned
// keeps them at the bottom; one landing while they scrolled up raises the
// jump button instead of yanking them down. Under the turn engine the pin
// state is the engine's user-intent flag, and the jump routes through it so
// the write stays an engine write.
function restoreBottomPin(container, wasAtBottom, forceScroll) {
  const engine = Chat.TurnEngine && container ? Chat.TurnEngine.activeFor(container) : null;
  if (engine) {
    if (forceScroll || wasAtBottom) {
      engine.jumpToBottom();
    } else {
      showScrollToBottom();
    }
    return;
  }
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
      // Under the turn engine the button tracks the user's pin intent (a
      // reader paused inside the 150px geometry band is still reading);
      // legacy views keep the geometry band.
      const engine = Chat.TurnEngine ? Chat.TurnEngine.activeFor(container) : null;
      if (engine ? engine.pinnedIntent : shouldAutoScroll(container)) hideScrollToBottom();
    });
  }
});

Chat.wire({
  showScrollToBottom,
  scrollToBottom,
  restoreBottomPin,
}, {
  hideScrollToBottom,
});

})();
