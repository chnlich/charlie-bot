// ---------------------------------------------------------------------------
// Platform detection — single source of truth for mobile/touch/foldable
// ---------------------------------------------------------------------------
const platform = (function() {
  const ua = navigator.userAgent;
  const isTouchDevice = /Android|iPhone|iPad/i.test(ua) || 'ontouchstart' in window;
  const _callbacks = [];

  function _mode() {
    const w = window.innerWidth;
    if (w <= 320) return 'cover';   // Z Fold 5 cover screen ~280px
    if (w <= 768) return 'mobile';  // Z Fold 5 unfolded ~717px, phones
    return 'desktop';
  }

  let _currentMode = _mode();

  function _update() {
    const next = _mode();
    if (next === _currentMode) return;
    const prev = _currentMode;
    _currentMode = next;
    _callbacks.forEach(fn => fn(next, prev));
  }

  window.addEventListener('resize', _update);
  if (window.visualViewport) {
    window.visualViewport.addEventListener('resize', _update);
  }

  return {
    /** 'cover' | 'mobile' | 'desktop' */
    get mode() { return _currentMode; },

    /** true on phones / tablets / foldables (any non-desktop) */
    get isMobile() { return _currentMode !== 'desktop'; },

    /** true only on Z Fold cover screen (<=320px) */
    get isCover() { return _currentMode === 'cover'; },

    /** true when hardware is touch-capable */
    get isTouch() { return isTouchDevice; },

    /** Desktop: Enter sends. Mobile/cover: Enter inserts newline. */
    get enterSendsMessage() { return _currentMode === 'desktop'; },

    /** 'overlay' on mobile/cover, 'pinned' on desktop */
    get sidebarMode() { return _currentMode === 'desktop' ? 'pinned' : 'overlay'; },

    /** 'fullscreen' on mobile/cover, 'side' on desktop */
    get panelMode() { return _currentMode === 'desktop' ? 'side' : 'fullscreen'; },

    /** Register callback: fn(newMode, prevMode) */
    onChange(fn) { _callbacks.push(fn); },
  };
})();
