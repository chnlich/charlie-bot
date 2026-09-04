// ---------------------------------------------------------------------------
// Plan panel — full-content-area view for plan lineage review.
//
// Marker `cbpanel` is the single URL-fragment marker that activates the
// artifact comment tray inside this view's iframe. artifact-comments.js
// checks the same literal (`'cbpanel'`) in its framed guard. Keep both in
// sync when renaming. The standalone "Open in tab" path omits the marker so
// the comment tray activates via the top-level-page branch instead.
// ---------------------------------------------------------------------------
const PLAN_PANEL_MARKER = 'cbpanel';

const planPanel = (() => {
  let _registry = {plans: []};
  let _loaded = false;
  let _loadedSessionId = null;
  let _errors = [];
  let _selectedPlanId = null;
  let _selectedVersion = null;
  // 对比上一版 toggle. Reset to the selected version's default (on when a
  // predecessor exists) at every selection change; a user click overrides it
  // until the next change of plan or version.
  let _diffEnabled = false;
  let _loadedViewerKey = null;
  let _stale = true;
  let _fetchGeneration = 0;
  let _actionBarGeneration = 0;
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

  function _currentSessionId() {
    if (typeof SESSION_ID === 'undefined' || SESSION_ID == null) return null;
    return SESSION_ID;
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

  // The lineage version immediately before `version` (greatest v below it), or
  // null — this is what the 对比上一版 toggle and its ?diff= query key off.
  function predecessorVersion(plan, version) {
    if (!plan || !plan.versions) return null;
    var pred = null;
    for (var i = 0; i < plan.versions.length; i++) {
      var v = plan.versions[i];
      if (v.v < version && (pred === null || v.v > pred.v)) pred = v;
    }
    return pred;
  }

  function hasPredecessor(plan, version) {
    return predecessorVersion(plan, version) !== null;
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
      var clone = qEl.cloneNode(true);
      var fnInClone = clone.querySelector('span.fn');
      if (fnInClone) fnInClone.remove();
      var question = (clone.textContent || '').trim();
      result.push({n: n, question: question});
    }
    return result;
  }

  function formatPlanStateLabel(plan) {
    if (!plan) return '';
    if (plan.takeoff != null && plan.state === 'approved') {
      return 'approved \u00B7 v' + plan.takeoff.v;
    }
    return plan.state || '';
  }

  // urlKind must name the caller: it is all a missing SESSIONS_ROOT error has
  // to say which of the panel's surfaces hit the misconfiguration.
  function _filesUrl(file, sessionId, sessionsRoot, urlKind) {
    var root = sessionsRoot;
    if (typeof window !== 'undefined' && window.SESSIONS_ROOT && !root) {
      root = window.SESSIONS_ROOT;
    }
    if (!root) {
      throw new Error('SESSIONS_ROOT not available for ' + urlKind);
    }
    return '/files' + root + '/' + sessionId + '/' + file;
  }

  // diffFile, when set, rides as the ?diff= query *before* the #cbsession=…
  // fragment, so the fragment stays last in every address this panel builds.
  // The file is a session-relative POSIX path, so its slashes stay literal.
  function encodeDiffFile(diffFile) {
    return encodeURIComponent(diffFile).replace(/%2F/g, '/');
  }

  function buildIframeUrl(file, sessionId, sessionsRoot, diffFile) {
    var url = _filesUrl(file, sessionId, sessionsRoot, 'plan panel iframe URL');
    if (diffFile) url += '?diff=' + encodeDiffFile(diffFile);
    return url +
      '#cbsession=' + encodeURIComponent(sessionId) +
      '&' + PLAN_PANEL_MARKER + '=1';
  }

  function buildIframeUrlFromVersion(plan, version, sessionId, sessionsRoot, diffOn) {
    var ver = _findVersion(plan, version);
    if (!ver) return null;
    var pred = diffOn ? predecessorVersion(plan, version) : null;
    return buildIframeUrl(ver.file, sessionId, sessionsRoot, pred ? pred.file : null);
  }

  // Standalone URL for the "Open in tab" action: real /files URL with the
  // cbsession fragment but WITHOUT the cbpanel marker. The comment tray
  // activates via the top-level-page branch of artifact-comments.js (the
  // framed guard is skipped because the page is not in an iframe).
  function buildStandaloneUrl(file, sessionId, sessionsRoot, diffFile) {
    var url = _filesUrl(file, sessionId, sessionsRoot, 'plan standalone URL');
    if (diffFile) url += '?diff=' + encodeDiffFile(diffFile);
    return url + '#cbsession=' + encodeURIComponent(sessionId);
  }

  function buildStandaloneUrlFromVersion(plan, version, sessionId, sessionsRoot, diffOn) {
    var ver = _findVersion(plan, version);
    if (!ver) return null;
    var pred = diffOn ? predecessorVersion(plan, version) : null;
    return buildStandaloneUrl(ver.file, sessionId, sessionsRoot, pred ? pred.file : null);
  }

  function _buildFilesFetchUrl(file, sessionId, sessionsRoot) {
    return _filesUrl(file, sessionId, sessionsRoot, 'plan panel files fetch');
  }

  // Full literal class strings: Tailwind's content scan only generates
  // classes it sees as complete tokens, so these stay whole literals, never
  // `bg-${color}-900`-style fragments.
  var STATE_BADGE_APPROVED = 'bg-green-900 text-green-300';
  var STATE_BADGE_NEUTRAL = 'bg-gray-700 text-gray-400';

  function stateBadgeClass(stateStr) {
    var s = String(stateStr || '');
    if (s === 'approved') return STATE_BADGE_APPROVED;
    if (s === 'awaiting approval') return 'bg-blue-900 text-blue-300';
    if (s === 'in flight') return 'bg-slate-700 text-slate-300';
    if (s.indexOf('approved') === 0) return STATE_BADGE_APPROVED;
    if (s === 'superseded' || s === 'abandoned') return STATE_BADGE_NEUTRAL;
    return STATE_BADGE_NEUTRAL;
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

  function _findVersion(plan, version) {
    if (!plan || !plan.versions) return null;
    for (var i = 0; i < plan.versions.length; i++) {
      if (plan.versions[i].v === version) return plan.versions[i];
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
      var label = '#' + p.id + ' ' + escapeHtmlAttr(p.title || '(untitled)') +
        ' [' + escapeHtmlAttr(formatPlanStateLabel(p)) + ']';
      return '<option value="' + escapeHtmlAttr(p.id) + '"' +
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
      return '<option value="' + escapeHtmlAttr(v.v) + '"' +
        (v.v === _selectedVersion ? ' selected' : '') +
        '>v' + escapeHtmlAttr(v.v) + '</option>';
    }).join('');
  }

  function _renderStaleNotice() {
    var notice = _getEl('plan-stale-notice');
    if (!notice) return;
    var plan = _findPlan(_selectedPlanId);
    if (!plan || _selectedVersion == null) {
      notice.classList.add('hidden');
      return;
    }
    notice.classList.toggle('hidden', !isStaleVersion(plan, _selectedVersion));
  }

  // Shown only when the selected version has a predecessor in the lineage, so
  // version 1 never offers the comparison.
  function _renderDiffToggle() {
    var wrap = _getEl('plan-diff-toggle-wrap');
    if (!wrap) return;
    var plan = _findPlan(_selectedPlanId);
    var show = plan != null && _selectedVersion != null && hasPredecessor(plan, _selectedVersion);
    wrap.classList.toggle('hidden', !show);
    var box = _getEl('plan-diff-toggle');
    if (box) box.checked = _diffEnabled;
  }

  function _viewerKey(planId, version, sessionId, diffOn) {
    if (planId == null || version == null) return null;
    return String(sessionId) + ':' + String(planId) + ':' + String(version) + ':' + (diffOn ? 'diff' : 'clean');
  }

  function _renderViewer() {
    var iframe = _getEl('plan-viewer');
    if (!iframe) return;
    var plan = _findPlan(_selectedPlanId);
    if (!plan || _selectedVersion == null) {
      iframe.src = '';
      _loadedViewerKey = null;
      return;
    }
    // The session component is always read live so a session switch reloads
    // the iframe even when (planId, version) is unchanged across sessions.
    // The diff toggle state is part of the key for the same reason: flipping
    // it must reload the iframe between the annotated and the clean page.
    var sid = _currentSessionId();
    var key = _viewerKey(_selectedPlanId, _selectedVersion, sid, _diffEnabled);
    if (key === _loadedViewerKey) return;
    try {
      iframe.src = buildIframeUrlFromVersion(plan, _selectedVersion, sid, undefined, _diffEnabled);
      _loadedViewerKey = key;
    } catch (e) {
      console.error('plan-panel: failed to build iframe URL', e);
      iframe.src = '';
      _loadedViewerKey = null;
    }
  }

  async function _renderActionBar() {
    var generation = ++_actionBarGeneration;
    var bar = _getEl('plan-action-bar');
    if (!bar) return;
    bar.innerHTML = '';
    var plan = _findPlan(_selectedPlanId);
    if (!plan || _selectedVersion == null) return;
    var ver = _findVersion(plan, _selectedVersion);
    if (!ver) return;
    var url;
    try {
      url = _buildFilesFetchUrl(ver.file, SESSION_ID);
    } catch (e) {
      console.error('plan-panel: action bar URL failed', e);
      return;
    }
    var resp;
    try {
      resp = await fetch(url);
    } catch (e) {
      console.error('plan-panel: action bar fetch failed', e);
      return;
    }
    if (generation !== _actionBarGeneration) return;
    if (!resp.ok) return;
    var html = await resp.text();
    if (generation !== _actionBarGeneration) return;
    var doc = new DOMParser().parseFromString(html, 'text/html');
    var forks = parseOpenForks(doc);
    if (generation !== _actionBarGeneration) return;
    bar.innerHTML = '';
    if (!forks.length) return;
    forks.forEach(function(fork) {
      var chip = document.createElement('button');
      chip.type = 'button';
      chip.className = 'px-2 py-1 text-xs rounded bg-slate-700 hover:bg-slate-600 text-slate-200 transition-colors';
      var question = fork.question || '';
      var hint = question.length > 40 ? question.slice(0, 40) + '\u2026' : question;
      chip.textContent = hint;
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
    var panel = _getEl('tab-plans');
    if (!panel) return;
    var empty = _getEl('plan-empty-state');
    var viewer = _getEl('plan-viewer');
    var plans = _registry.plans || [];
    if (!plans.length) {
      if (empty) empty.style.display = '';
      if (viewer) { viewer.src = ''; viewer.style.display = 'none'; }
      _loadedViewerKey = null;
      var sel = _getEl('plan-selector');
      if (sel) sel.innerHTML = '';
      var vsel = _getEl('plan-version-selector');
      if (vsel) vsel.innerHTML = '';
      var notice = _getEl('plan-stale-notice');
      if (notice) notice.classList.add('hidden');
      var diffWrap = _getEl('plan-diff-toggle-wrap');
      if (diffWrap) diffWrap.classList.add('hidden');
      var bar = _getEl('plan-action-bar');
      if (bar) bar.innerHTML = '';
      return;
    }
    if (empty) empty.style.display = 'none';
    if (viewer) viewer.style.display = '';
    _renderSelector();
    _renderVersionSwitcher();
    _renderStaleNotice();
    _renderDiffToggle();
    _renderViewer();
  }

  // -- Data: single commit point ------------------------------------------

  // The only writer of _registry / _loaded / _loadedSessionId. Discards the
  // write when the session has changed (sid !== current) or when a newer
  // fetch generation has superseded this one (monotonic guard against stale
  // completions from a slow/older fetch landing after a newer one).
  function _commitRegistry(sid, data, generation) {
    if (sid !== _currentSessionId()) return false;
    if (generation !== _fetchGeneration) return false;
    _registry = data || {plans: []};
    _errors = Array.isArray(data && data.errors) ? data.errors : [];
    if (_errors.length) {
      console.warn('plan-panel: registry returned ' + _errors.length + ' error(s)', _errors);
    }
    _loaded = true;
    _loadedSessionId = sid;
    return true;
  }

  // Second writer of the cached registry, complementing _commitRegistry: this
  // one only *clears*, and it never writes _loadedSessionId—leaving the
  // session mismatch in place is what makes the next onTabShown retry.
  function _resetForSessionChange() {
    _registry = {plans: []};
    _errors = [];
    _loaded = false;
    _selectedPlanId = null;
    _selectedVersion = null;
    _diffEnabled = false;
    _loadedViewerKey = null;
    ++_actionBarGeneration;      // discard in-flight action-bar renders for the old session
    _highlightTabBadge(false);   // fallback for any path that skips the hook
    render();                    // empty state, synchronously
  }

  async function _fetchRegistry(sid) {
    var resp = await fetch('/api/sessions/' + encodeURIComponent(sid) + '/plans');
    if (!resp.ok) {
      throw new Error('plan registry fetch failed: HTTP ' + resp.status);
    }
    return resp.json();
  }

  // Refetch and commit under a fresh generation. False means either the fetch
  // failed or the commit was superseded (see the _commitRegistry guard), and
  // the caller must bail without touching the selection or the DOM.
  async function _refetchRegistry(sid, failLabel) {
    var generation = ++_fetchGeneration;
    try {
      var data = await _fetchRegistry(sid);
      return _commitRegistry(sid, data, generation);
    } catch (e) {
      console.error(failLabel, e);
      return false;
    }
  }

  function ensureLoaded() {
    var sid = _currentSessionId();
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
      _commitRegistry(null, {plans: []}, ++_fetchGeneration);
      return _loadPromise;
    }
    var generation = ++_fetchGeneration;
    _loadPromise = (async () => {
      try {
        var data = await _fetchRegistry(sid);
        _commitRegistry(sid, data, generation);
      } catch (e) {
        console.error('plan-panel ensureLoaded failed:', e);
        _commitRegistry(sid, {plans: []}, generation);
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
        _diffEnabled = hasPredecessor(found, lv.v);
      }
      return;
    }
    var sel = selectDefaultLineage(plans);
    if (sel) {
      _selectedPlanId = sel.planId;
      _selectedVersion = sel.version;
      _diffEnabled = hasPredecessor(_findPlan(sel.planId), sel.version);
    } else {
      _selectedPlanId = null;
      _selectedVersion = null;
      _diffEnabled = false;
    }
  }

  async function refresh() {
    var sid = _currentSessionId();
    if (!sid) return;
    if (!await _refetchRegistry(sid, 'plan-panel refresh failed:')) return;
    _ensureSelection();
    render();
    await _renderActionBar();
  }

  async function onPlanUpdated(planId) {
    var sid = _currentSessionId();
    if (!sid) return;
    var prev = _registry;
    if (!await _refetchRegistry(sid, 'plan-panel onPlanUpdated fetch failed:')) return;
    _refreshPlanCardBadges();
    var isNew = detectNewPlanOrVersion(prev, _registry);
    if (isNew) {
      _selectedPlanId = isNew.planId;
      var plan = _findPlan(isNew.planId);
      var lv = _latestVersion(plan);
      _selectedVersion = lv ? lv.v : isNew.version;
      _diffEnabled = hasPredecessor(plan, _selectedVersion);
      _highlightTabBadge(true);
      // No forced tab switch — keep the badge highlight + selection jump only.
    } else {
      _ensureSelection();
    }
    render();
    await _renderActionBar();
  }

  function _refreshPlanCardBadges() {
    if (typeof Chat === 'undefined' || !Chat || typeof Chat.updatePlanCardBadges !== 'function') return;
    try {
      Chat.updatePlanCardBadges(_registry);
    } catch (e) {
      console.warn('plan-panel: updatePlanCardBadges failed', e);
    }
  }

  function onReconnect() {
    _stale = true;
  }

  function onActiveSessionChanged() {
    // Sole owner of clearing the cached selection / loaded viewer key and
    // blanking the iframe on a session change. onReconnect() no longer handles
    // session mismatches — it only marks stale on a WS reconnect.
    _selectedPlanId = null;
    _selectedVersion = null;
    _loadedViewerKey = null;
    var iframe = _getEl('plan-viewer');
    if (iframe) iframe.src = '';
    _highlightTabBadge(false);
    return refresh();
  }

  function invalidate() {
    _stale = true;
  }

  async function onTabShown() {
    // Fallback for the paths the hook cannot fix: its refresh() failed, or
    // SESSION_ID went null (refresh() returns immediately). Reset first so the
    // panel never shows the previous session, then re-fetch.
    var sid = _currentSessionId();
    var switched = (_loadedSessionId !== sid);
    if (switched) _resetForSessionChange();
    if (!_stale && !switched) return;
    _stale = false;
    await refresh();
  }

  // -- Selection ----------------------------------------------------------

  function selectPlan(planId) {
    _selectedPlanId = planId != null ? Number(planId) : null;
    var plan = _findPlan(_selectedPlanId);
    var lv = _latestVersion(plan);
    _selectedVersion = lv ? lv.v : null;
    _diffEnabled = hasPredecessor(plan, _selectedVersion);
    _renderVersionSwitcher();
    _renderStaleNotice();
    _renderDiffToggle();
    _renderViewer();
    _renderActionBar();
  }

  function selectVersion(version) {
    _selectedVersion = version != null ? Number(version) : null;
    _diffEnabled = hasPredecessor(_findPlan(_selectedPlanId), _selectedVersion);
    _renderStaleNotice();
    _renderDiffToggle();
    _renderViewer();
    _renderActionBar();
  }

  function openPlan(planId, v) {
    // switchTab synchronously runs onTabShown, which may reset the selection on
    // a session mismatch — so switch first, then assign what the user clicked.
    if (typeof switchTab === 'function') switchTab('chat-plans');
    _selectedPlanId = planId != null ? Number(planId) : null;
    if (v != null) _selectedVersion = Number(v);
    _diffEnabled = hasPredecessor(_findPlan(_selectedPlanId), _selectedVersion);
    _renderSelector();
    _renderVersionSwitcher();
    _renderStaleNotice();
    _renderDiffToggle();
    _renderViewer();
    _renderActionBar();
  }

  // Toggle click handler (the checkbox's onchange). Flipping changes the
  // viewer key, so the iframe reloads between the annotated and the clean page.
  function setDiffEnabled(on) {
    _diffEnabled = !!on;
    _renderDiffToggle();
    _renderViewer();
  }

  // -- Iframe load hook ----------------------------------------------------

  function initViewerLoadHook() {
    var iframe = _getEl('plan-viewer');
    if (!iframe) return;
    iframe.addEventListener('load', function() {
      _highlightTabBadge(false);
    });
  }

  // -- Open in tab --------------------------------------------------------

  function openCurrentInTab() {
    var plan = _findPlan(_selectedPlanId);
    if (!plan || _selectedVersion == null) return;
    try {
      var url = buildStandaloneUrlFromVersion(plan, _selectedVersion, SESSION_ID, undefined, _diffEnabled);
      if (!url) return;
      if (typeof window !== 'undefined' && typeof window.open === 'function') {
        window.open(url, '_blank', 'noopener,noreferrer');
      }
    } catch (e) {
      console.error('plan-panel: openCurrentInTab failed', e);
    }
  }

  // -- Init ---------------------------------------------------------------

  function init() {
    initViewerLoadHook();
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
    onActiveSessionChanged,
    invalidate,
    onTabShown,
    selectPlan,
    selectVersion,
    openPlan,
    setDiffEnabled,
    openCurrentInTab,
    ensureLoaded,
    ready,
    getRegistrySnapshot,
    init,
    selectDefaultLineage,
    isStaleVersion,
    hasPredecessor,
    predecessorVersion,
    detectNewPlanOrVersion,
    parseOpenForks,
    formatPlanStateLabel,
    buildIframeUrl,
    buildIframeUrlFromVersion,
    buildStandaloneUrl,
    buildStandaloneUrlFromVersion,
    stateBadgeClass,
    currentSessionId: _currentSessionId,
  };
})();
