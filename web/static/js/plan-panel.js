// ---------------------------------------------------------------------------
// Plan panel — persistent right-side split panel for plan lineage review.
//
// Marker `cbpanel` is the single URL-fragment marker that activates the
// artifact comment tray inside this panel's iframe. artifact-comments.js
// checks the same literal (`'cbpanel'`) in its framed guard. Keep both in
// sync when renaming.
// ---------------------------------------------------------------------------
const PLAN_PANEL_MARKER = 'cbpanel';

const planPanel = (() => {
  let _registry = {plans: []};
  let _loaded = false;
  let _loadedSessionId = null;
  let _selectedPlanId = null;
  let _selectedVersion = null;
  let _loadPromise = null;
  let _loadPromiseSessionId = null;

  // -- Pure helpers (exported for testing) --------------------------------

  function _isApproved(plan) {
    return plan && plan.takeoff != null && plan.closed == null;
  }

  function _isClosed(plan) {
    return plan && plan.closed != null;
  }

  function _latestVersion(plan) {
    if (!plan || !plan.versions || !plan.versions.length) return null;
    return plan.versions[plan.versions.length - 1];
  }

  function selectDefaultLineage(plans) {
    if (!plans || !plans.length) return null;
    var open = plans.filter(function(p) {
      return !_isClosed(p) && !_isApproved(p);
    });
    var target = open.length ? open[open.length - 1] : plans[plans.length - 1];
    var latest = _latestVersion(target);
    if (!latest) return null;
    return {planId: target.id, version: latest.v};
  }

  function isStaleVersion(plan, version) {
    if (!plan || !plan.versions) return false;
    var latest = _latestVersion(plan);
    if (!latest) return false;
    return version < latest.v;
  }

  function detectNewPlanOrVersion(prev, next) {
    var prevPlans = (prev && prev.plans) || [];
    var nextPlans = (next && next.plans) || [];
    var prevById = {};
    for (var i = 0; i < prevPlans.length; i++) {
      prevById[prevPlans[i].id] = prevPlans[i];
    }
    for (var j = 0; j < nextPlans.length; j++) {
      var np = nextPlans[j];
      var pp = prevById[np.id];
      if (!pp) {
        var lv = _latestVersion(np);
        if (lv) return {planId: np.id, version: lv.v};
        continue;
      }
      var prevVersions = {};
      for (var k = 0; k < pp.versions.length; k++) {
        prevVersions[pp.versions[k].v] = true;
      }
      for (var m = 0; m < np.versions.length; m++) {
        if (!prevVersions[np.versions[m].v]) {
          return {planId: np.id, version: np.versions[m].v};
        }
      }
    }
    return null;
  }

  function parseOpenForks(doc) {
    if (!doc || !doc.querySelectorAll) return [];
    var forks = doc.querySelectorAll('div.fork');
    var result = [];
    for (var i = 0; i < forks.length; i++) {
      var fork = forks[i];
      var fnEl = fork.querySelector('span.fn');
      var qEl = fork.querySelector('p.q');
      var recEl = fork.querySelector('p.rec');
      var resolvedEl = fork.querySelector('p.resolved');
      if (!fnEl || !qEl || !recEl || resolvedEl) continue;
      var n = (fnEl.textContent || '').trim();
      var question = (qEl.textContent || '').trim();
      result.push({n: n, question: question});
    }
    return result;
  }

  function buildIframeUrl(file, sessionId, userHome) {
    var home = userHome;
    if (typeof window !== 'undefined' && window.USER_HOME && !home) {
      home = window.USER_HOME;
    }
    if (!home) {
      throw new Error('USER_HOME not available for plan panel iframe URL');
    }
    var absPath = home + '/.charliebot/sessions/' + sessionId + '/' + file;
    return '/files' + absPath + '#cbsession=' + encodeURIComponent(sessionId) +
      '&' + PLAN_PANEL_MARKER + '=1';
  }

  function buildIframeUrlFromVersion(plan, version, sessionId, userHome) {
    if (!plan || !plan.versions) return null;
    var ver = null;
    for (var i = 0; i < plan.versions.length; i++) {
      if (plan.versions[i].v === version) { ver = plan.versions[i]; break; }
    }
    if (!ver) return null;
    return buildIframeUrl(ver.file, sessionId, userHome);
  }

  function stateBadgeClass(stateStr) {
    var s = String(stateStr || '');
    if (s === 'approved') return 'bg-green-900 text-green-300';
    if (s === 'awaiting approval') return 'bg-blue-900 text-blue-300';
    if (s === 'in flight') return 'bg-slate-700 text-slate-300';
    if (s === 'needs amendment') return 'bg-yellow-900 text-yellow-300';
    if (s.indexOf('verify failed') !== -1) return 'bg-red-900 text-red-300';
    if (s.indexOf('approved') === 0) return 'bg-green-900 text-green-300';
    if (s === 'superseded' || s === 'abandoned') return 'bg-gray-700 text-gray-400';
    return 'bg-gray-700 text-gray-400';
  }

  function _esc(s) {
    return String(s == null ? '' : s)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;');
  }

  // -- DOM helpers --------------------------------------------------------

  function _getEl(id) {
    return document.getElementById(id);
  }

  function _findPlan(id) {
    var plans = _registry.plans || [];
    for (var i = 0; i < plans.length; i++) {
      if (String(plans[i].id) === String(id)) return plans[i];
    }
    return null;
  }

  // -- Rendering ----------------------------------------------------------

  function _renderSelector() {
    var sel = _getEl('plan-selector');
    if (!sel) return;
    var plans = _registry.plans || [];
    if (!plans.length) {
      sel.innerHTML = '<option value="">No plans</option>';
      return;
    }
    sel.innerHTML = plans.map(function(p) {
      var label = '#' + p.id + ' ' + _esc(p.title || '(untitled)') +
        ' [' + _esc(p.state || '') + ']';
      return '<option value="' + _esc(p.id) + '"' +
        (String(p.id) === String(_selectedPlanId) ? ' selected' : '') +
        '>' + label + '</option>';
    }).join('');
  }

  function _renderVersionSwitcher() {
    var sel = _getEl('plan-version-selector');
    if (!sel) return;
    var plan = _findPlan(_selectedPlanId);
    if (!plan || !plan.versions || !plan.versions.length) {
      sel.innerHTML = '';
      sel.style.display = 'none';
      return;
    }
    sel.style.display = '';
    sel.innerHTML = plan.versions.map(function(v) {
      return '<option value="' + _esc(v.v) + '"' +
        (v.v === _selectedVersion ? ' selected' : '') +
        '>v' + _esc(v.v) + '</option>';
    }).join('');
  }

  function _renderStaleNotice() {
    var notice = _getEl('plan-stale-notice');
    if (!notice) return;
    var plan = _findPlan(_selectedPlanId);
    if (!plan || _selectedVersion == null) {
      notice.style.display = 'none';
      return;
    }
    if (isStaleVersion(plan, _selectedVersion)) {
      notice.style.display = '';
    } else {
      notice.style.display = 'none';
    }
  }

  function _renderViewer() {
    var iframe = _getEl('plan-viewer');
    if (!iframe) return;
    var plan = _findPlan(_selectedPlanId);
    if (!plan || _selectedVersion == null) {
      iframe.src = '';
      return;
    }
    try {
      iframe.src = buildIframeUrlFromVersion(plan, _selectedVersion, SESSION_ID);
    } catch (e) {
      console.error('plan-panel: failed to build iframe URL', e);
      iframe.src = '';
    }
  }

  function _renderActionBar() {
    var bar = _getEl('plan-action-bar');
    if (!bar) return;
    bar.innerHTML = '';
    var iframe = _getEl('plan-viewer');
    if (!iframe) return;
    try {
      var doc = iframe.contentDocument;
    } catch (e) {
      return;
    }
    if (!doc) return;
    var forks = parseOpenForks(doc);
    if (!forks.length) return;
    forks.forEach(function(fork) {
      var chip = document.createElement('button');
      chip.type = 'button';
      chip.className = 'px-2 py-1 text-xs rounded bg-slate-700 hover:bg-slate-600 text-slate-200 transition-colors';
      var hint = fork.question.slice(0, 40);
      chip.textContent = 'Trade-off ' + fork.n + ': ' + hint;
      chip.title = 'Prefill: Trade-off ' + fork.n;
      chip.addEventListener('click', function() {
        _prefillChat('Trade-off ' + fork.n + ': ');
      });
      bar.appendChild(chip);
    });
  }

  function _prefillChat(text) {
    var input = _getEl('msg-input');
    if (!input) return;
    input.value = text;
    input.focus();
    if (typeof autoResize === 'function') autoResize(input);
  }

  function _highlightTabBadge(on) {
    var btn = _getEl('btn-chat-plans');
    if (!btn) return;
    var dot = btn.querySelector('.plan-tab-dot');
    if (on) {
      if (!dot) {
        dot = document.createElement('span');
        dot.className = 'plan-tab-dot ml-1 inline-block w-2 h-2 rounded-full bg-blue-400';
        btn.appendChild(dot);
      }
      dot.style.display = '';
    } else {
      if (dot) dot.style.display = 'none';
    }
  }

  function render() {
    var panel = _getEl('plan-panel');
    if (!panel) return;
    var empty = _getEl('plan-empty-state');
    var viewer = _getEl('plan-viewer');
    var plans = _registry.plans || [];
    if (!plans.length) {
      if (empty) empty.style.display = '';
      if (viewer) { viewer.src = ''; viewer.style.display = 'none'; }
      var vsel = _getEl('plan-version-selector');
      if (vsel) vsel.innerHTML = '';
      var notice = _getEl('plan-stale-notice');
      if (notice) notice.style.display = 'none';
      var bar = _getEl('plan-action-bar');
      if (bar) bar.innerHTML = '';
      return;
    }
    if (empty) empty.style.display = 'none';
    if (viewer) viewer.style.display = '';
    _renderSelector();
    _renderVersionSwitcher();
    _renderStaleNotice();
    _renderViewer();
  }

  // -- Data ---------------------------------------------------------------

  async function _fetchRegistry() {
    var resp = await fetch('/api/sessions/' + encodeURIComponent(SESSION_ID) + '/plans');
    if (!resp.ok) {
      throw new Error('plan registry fetch failed: HTTP ' + resp.status);
    }
    return resp.json();
  }

  function ensureLoaded() {
    var sid = (typeof SESSION_ID === 'undefined') ? null : SESSION_ID;
    // Reuse an in-flight/completed load only when it is for the current
    // session. Session switching is an in-place SPA swap (SESSION_ID changes
    // without a page reload), so a promise/snapshot from the previous session
    // must not be reused: otherwise the chat embed path would decide
    // registered-vs-not against the previous session's registry and mis-render
    // another session's plan cards (file names like plan_01.html collide).
    if (_loadPromise && _loadPromiseSessionId === sid) return _loadPromise;
    _loadPromiseSessionId = sid;
    if (!sid) {
      _loadPromise = Promise.resolve();
      _loaded = true;
      _loadedSessionId = null;
      return _loadPromise;
    }
    _loadPromise = (async () => {
      try {
        var data = await _fetchRegistry();
        _registry = data || {plans: []};
        _loaded = true;
        _loadedSessionId = sid;
      } catch (e) {
        console.error('plan-panel ensureLoaded failed:', e);
        _registry = {plans: []};
        _loaded = true;
        _loadedSessionId = sid;
      }
    })();
    return _loadPromise;
  }

  function ready() {
    return ensureLoaded();
  }

  function getRegistrySnapshot() {
    return _registry;
  }

  function _ensureSelection() {
    var plans = _registry.plans || [];
    var found = _findPlan(_selectedPlanId);
    if (found) {
      var lv = _latestVersion(found);
      if (lv && (_selectedVersion == null || _selectedVersion > lv.v)) {
        _selectedVersion = lv.v;
      }
      return;
    }
    var sel = selectDefaultLineage(plans);
    if (sel) {
      _selectedPlanId = sel.planId;
      _selectedVersion = sel.version;
    } else {
      _selectedPlanId = null;
      _selectedVersion = null;
    }
  }

  async function refresh() {
    try {
      var data = await _fetchRegistry();
      _registry = data || {plans: []};
      _loaded = true;
      _loadedSessionId = SESSION_ID;
    } catch (e) {
      console.error('plan-panel refresh failed:', e);
      _registry = {plans: []};
    }
    _ensureSelection();
    render();
  }

  async function onPlanUpdated(planId) {
    var prev = _registry;
    try {
      var data = await _fetchRegistry();
      _registry = data || {plans: []};
      _loaded = true;
      _loadedSessionId = SESSION_ID;
    } catch (e) {
      console.error('plan-panel onPlanUpdated fetch failed:', e);
      return;
    }
    _refreshPlanCardBadges();
    var isNew = detectNewPlanOrVersion(prev, _registry);
    if (isNew) {
      _selectedPlanId = isNew.planId;
      var plan = _findPlan(isNew.planId);
      var lv = _latestVersion(plan);
      _selectedVersion = lv ? lv.v : isNew.version;
      _highlightTabBadge(true);
      if (typeof switchTab === 'function') switchTab('chat-plans');
    } else {
      _ensureSelection();
    }
    render();
  }

  function _refreshPlanCardBadges() {
    if (typeof Chat === 'undefined' || !Chat || typeof Chat.updatePlanCardBadges !== 'function') return;
    try {
      Chat.updatePlanCardBadges(_registry);
    } catch (e) {
      console.warn('plan-panel: updatePlanCardBadges failed', e);
    }
  }

  async function onReconnect() {
    if (!_loaded) return;
    if (_loadedSessionId !== SESSION_ID) {
      _registry = {plans: []};
      _selectedPlanId = null;
      _selectedVersion = null;
      if (typeof _plansLoaded !== 'undefined') _plansLoaded = false;
    }
    await refresh();
  }

  // -- Selection ----------------------------------------------------------

  function selectPlan(planId) {
    _selectedPlanId = planId != null ? Number(planId) : null;
    var plan = _findPlan(_selectedPlanId);
    var lv = _latestVersion(plan);
    _selectedVersion = lv ? lv.v : null;
    _renderVersionSwitcher();
    _renderStaleNotice();
    _renderViewer();
  }

  function selectVersion(version) {
    _selectedVersion = version != null ? Number(version) : null;
    _renderStaleNotice();
    _renderViewer();
  }

  function openPlan(planId, v) {
    _selectedPlanId = planId != null ? Number(planId) : null;
    if (v != null) _selectedVersion = Number(v);
    if (typeof switchTab === 'function') switchTab('chat-plans');
    _renderSelector();
    _renderVersionSwitcher();
    _renderStaleNotice();
    _renderViewer();
  }

  // -- Iframe load hook ----------------------------------------------------

  function initViewerLoadHook() {
    var iframe = _getEl('plan-viewer');
    if (!iframe) return;
    iframe.addEventListener('load', function() {
      _highlightTabBadge(false);
      _renderActionBar();
    });
  }

  // -- Resize (mirrors initBacklogResize) ---------------------------------

  function initPlanResize() {
    var handle = _getEl('plan-resize-handle');
    var panel = _getEl('plan-panel');
    if (!handle || !panel) return;
    var container = panel.parentElement;
    var saved = localStorage.getItem('plan-panel-pct');
    if (saved) panel.style.width = saved + '%';

    var startX, startW, containerW;
    handle.addEventListener('mousedown', function(e) {
      e.preventDefault();
      startX = e.clientX;
      containerW = container.offsetWidth;
      startW = panel.offsetWidth;
      handle.classList.add('active');
      document.body.classList.add('resizing');

      function onMove(ev) {
        var delta = startX - ev.clientX;
        var w = Math.min(Math.max(startW + delta, containerW * 0.2), containerW * 0.8);
        panel.style.width = w + 'px';
      }
      function onUp() {
        handle.classList.remove('active');
        document.body.classList.remove('resizing');
        var pct = (panel.offsetWidth / container.offsetWidth * 100).toFixed(1);
        localStorage.setItem('plan-panel-pct', pct);
        panel.style.width = pct + '%';
        document.removeEventListener('mousemove', onMove);
        document.removeEventListener('mouseup', onUp);
      }
      document.addEventListener('mousemove', onMove);
      document.addEventListener('mouseup', onUp);
    });
  }

  // -- Init ---------------------------------------------------------------

  function init() {
    initViewerLoadHook();
    initPlanResize();
    ensureLoaded();
  }

  if (typeof document !== 'undefined') {
    if (document.readyState === 'loading') {
      document.addEventListener('DOMContentLoaded', init);
    } else {
      init();
    }
  }

  return {
    refresh,
    render,
    onPlanUpdated,
    onReconnect,
    selectPlan,
    selectVersion,
    openPlan,
    ensureLoaded,
    ready,
    getRegistrySnapshot,
    init,
    selectDefaultLineage,
    isStaleVersion,
    detectNewPlanOrVersion,
    parseOpenForks,
    buildIframeUrl,
    buildIframeUrlFromVersion,
    stateBadgeClass,
  };
})();
