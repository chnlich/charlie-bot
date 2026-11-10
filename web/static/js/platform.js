// ---------------------------------------------------------------------------
// Platform detection — single source of truth for mobile/touch/foldable
// ---------------------------------------------------------------------------
const platform = (function() {
  const ua = navigator.userAgent;
  const isTouchDevice = /Android|iPhone|iPad/i.test(ua) || 'ontouchstart' in window;
  const _callbacks = [];

  // The layout modes, one member per value `_mode` returns. Every return and
  // comparison names a member, so this object is the one home of these values.
  const PLATFORM_MODE = {
    COVER: 'cover',
    MOBILE: 'mobile',
    DESKTOP: 'desktop',
  };

  function _mode() {
    const w = window.innerWidth;
    if (w <= 320) return PLATFORM_MODE.COVER;   // Z Fold 5 cover screen ~280px
    if (w <= 768) return PLATFORM_MODE.MOBILE;  // Z Fold 5 unfolded ~717px, phones
    return PLATFORM_MODE.DESKTOP;
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

  // Track visual viewport height to handle mobile keyboard resize.
  // On Android Chrome, dvh/innerHeight do NOT shrink when the keyboard opens —
  // only visualViewport.height reflects the actual visible area.
  function _syncAppHeight() {
    const h = (window.visualViewport && isTouchDevice)
      ? window.visualViewport.height + 'px'
      : '100dvh';
    document.documentElement.style.setProperty('--app-h', h);
  }
  _syncAppHeight();

  if (window.visualViewport) {
    window.visualViewport.addEventListener('resize', () => { _update(); _syncAppHeight(); });
  }

  return {
    /** true on phones / tablets / foldables (any non-desktop) */
    get isMobile() { return _currentMode !== PLATFORM_MODE.DESKTOP; },

    /** Register callback: fn(newMode, prevMode) */
    onChange(fn) { _callbacks.push(fn); },
  };
})();
