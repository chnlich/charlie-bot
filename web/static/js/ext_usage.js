// ---------------------------------------------------------------------------
// External tool usage strip (multi-provider, multi-account: Claude, Codex).
// The strip DOM is built dynamically from the payload: one self-describing row
// per account, with a provider pill on each row. A quota bar is identified by
// the window length the provider reported, never by the position the limit
// arrived in, so a provider dropping or adding a window changes which bars
// appear instead of relabelling the ones that remain.
// ---------------------------------------------------------------------------
// Per-row element refs + captured data, rebuilt on every render so the 60s
// client-side refresh can recompute labels without a server round-trip.
let _extUsageRows = [];

// ---------------------------------------------------------------------------
// Collapse state: one in-memory boolean, evaluated once on DOMContentLoaded.
// localStorage["ext_usage_strip_collapsed_v1"] holds "1" (collapsed) or "0"
// (expanded). A missing or invalid value falls back to the platform default
// and is never written back — only a user toggle writes. Any storage failure
// silently degrades to the in-memory boolean (same guard shape as the
// sessionStorage draft handling in artifact-comments.js).
// ---------------------------------------------------------------------------
const EXT_USAGE_COLLAPSED_KEY = 'ext_usage_strip_collapsed_v1';
let _extUsageCollapsed = false;

function _readCollapsedPreference() {
  try {
    const raw = localStorage.getItem(EXT_USAGE_COLLAPSED_KEY);
    if (raw === '1') return true;
    if (raw === '0') return false;
  } catch (e) {
    // storage refused the read — treat as no recorded preference
  }
  return null;
}

function _writeCollapsedPreference(collapsed) {
  try {
    localStorage.setItem(EXT_USAGE_COLLAPSED_KEY, collapsed ? '1' : '0');
  } catch (e) {
    // storage refused the write — the in-memory boolean still applies
  }
}

// A user toggle updates the in-memory boolean immediately, persists it
// (guarded), and reapplies the collapsed class + chip glyph without
// rebuilding any rows — the strip's children never change on a toggle.
function _setExtUsageCollapsed(collapsed) {
  _extUsageCollapsed = collapsed;
  _writeCollapsedPreference(collapsed);
  const strip = document.getElementById('ext-usage-strip');
  if (strip) _syncExtUsageChrome(strip);
}

// The collapsed class mirrors the in-memory boolean on every render, and the
// chip tracks the strip: hidden whenever the strip is, glyph ▸ collapsed /
// ▾ expanded. Called from renderExtUsage and from toggle clicks.
function _syncExtUsageChrome(strip) {
  strip.classList.toggle('collapsed', _extUsageCollapsed);
  const chip = document.getElementById('ext-usage-toggle');
  if (!chip) return;
  chip.classList.toggle('hidden', strip.classList.contains('hidden'));
  chip.textContent = _extUsageCollapsed ? '▸' : '▾';
  chip.title = _extUsageCollapsed ? 'Show quota details' : 'Hide quota details';
}

function _parseTimestampMs(isoString) {
  if (!isoString) return NaN;
  return new Date(isoString).getTime();
}

// When a provider's numbers were sampled. Claude answers a live query, so its
// fetch time is the sample time; Codex is scraped from a rollout log whose last
// token_count event can be days old.
function _sampledAtMs(providerData) {
  const observed = _parseTimestampMs(providerData.token_count_observed_at);
  if (Number.isFinite(observed)) return observed;
  return _parseTimestampMs(providerData.fetched_at);
}

function _windowLabel(windowMinutes) {
  if (!Number.isFinite(windowMinutes) || windowMinutes <= 0) return '?';
  if (windowMinutes % 1440 === 0) return (windowMinutes / 1440) + 'd';
  if (windowMinutes % 60 === 0) return (windowMinutes / 60) + 'h';
  return windowMinutes + 'm';
}

// A scraped reading can outlive the window it describes. Either the window's
// reset has already passed while the sample predates it, or — decidable without
// any reset timestamp — the reading is older than the window is long.
function _isExpiredReading(providerData, win) {
  if (providerData.provider !== 'codex') return false;
  const sampled = _sampledAtMs(providerData);
  if (!Number.isFinite(sampled)) return false;

  const reset = _parseTimestampMs(win.resets_at);
  if (Number.isFinite(reset) && reset <= Date.now() && sampled < reset) return true;

  if (Number.isFinite(win.window_minutes)) {
    return Date.now() - sampled > win.window_minutes * 60000;
  }
  return false;
}

function _formatLocalHMS(ms) {
  const d = new Date(ms);
  const hh = String(d.getHours()).padStart(2, '0');
  const mm = String(d.getMinutes()).padStart(2, '0');
  const ss = String(d.getSeconds()).padStart(2, '0');
  return hh + ':' + mm + ':' + ss;
}

function _formatAge(ms) {
  const elapsed = ms > 0 ? ms : 0;
  const minutes = Math.floor(elapsed / 60000);
  if (minutes < 60) return minutes + 'm';
  const hours = Math.floor(elapsed / 3600000);
  if (hours < 24) return hours + 'h';
  return Math.floor(elapsed / 86400000) + 'd';
}

function _asOfText(providerData) {
  const sampled = _sampledAtMs(providerData);
  if (!Number.isFinite(sampled)) return '';
  return 'as of ' + _formatAge(Date.now() - sampled) + ' ago';
}

// Short windows reset too often for a countdown to mean anything, so they show
// the sampled and reset clock times instead.
function _formatClockWindow(providerData, win) {
  const sampled = _sampledAtMs(providerData);
  const sampledHMS = Number.isFinite(sampled) ? _formatLocalHMS(sampled) : '?';
  return '(' + sampledHMS + ' – ' + _formatLocalHMS(_parseTimestampMs(win.resets_at)) + ')';
}

function _formatCountdown(win) {
  let diff = _parseTimestampMs(win.resets_at) - Date.now();
  if (!Number.isFinite(diff)) return '—';
  if (diff < 0) return 'reset overdue';
  const days = Math.floor(diff / 86400000);
  diff -= days * 86400000;
  const hours = Math.floor(diff / 3600000);
  diff -= hours * 3600000;
  const minutes = Math.floor(diff / 60000);
  if (days > 0) return 'resets in ' + days + 'd ' + hours + 'h';
  if (hours > 0) return 'resets in ' + hours + 'h ' + minutes + 'm';
  return 'resets in ' + minutes + 'm';
}

function _formatWindowReset(providerData, win) {
  if (!win.resets_at) return '—';
  if (Number.isFinite(win.window_minutes) && win.window_minutes < 1440) {
    return _formatClockWindow(providerData, win);
  }
  return _formatCountdown(win);
}

function _barColor(pct) {
  if (pct > 80) return 'bg-red-500';
  if (pct >= 50) return 'bg-yellow-500';
  return 'bg-emerald-500';
}

// Text-colour sibling of the bar thresholds: the committed stylesheet has no
// text-…-500 utilities and no plain emerald-400 text utility, so the summary
// percentage maps each bar hue to its nearest existing -400 text class.
// (Class-name literals must stay out of this comment: the Tailwind scanner
// reads comments as candidate tokens.)
const _BAR_TEXT_COLORS = {
  'bg-red-500': 'text-red-400',
  'bg-yellow-500': 'text-yellow-400',
  'bg-emerald-500': 'text-green-400',
};

function _barTextColor(pct) {
  return _BAR_TEXT_COLORS[_barColor(pct)];
}

function _formatSpendUsd(value) {
  if (typeof value !== 'number' || !Number.isFinite(value)) return '—';
  return '$' + value.toFixed(2);
}

function _providerGroupLabel(provider) {
  if (provider === 'claude') return 'Claude';
  if (provider === 'codex') return 'Codex';
  throw new Error('unknown usage provider: ' + provider);
}

function _providerStateLabel(state) {
  if (state === 'business-unlimited') return 'business / unlimited';
  return '';
}

function _el(tag, className) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  return node;
}

function _providerPill(provider) {
  const cls = provider === 'claude' ? 'provider-claude' : 'provider-codex';
  const pill = _el('span', 'provider-pill ' + cls);
  pill.textContent = _providerGroupLabel(provider);
  return pill;
}

function _blankBar(bar) {
  bar.style.width = '0.0%';
  bar.className = PROGRESS_BAR_FILL_CLASS + ' bg-slate-600';
}

// Precedence: expired beats unknown beats a live number. Neither of the first
// two reaches the colour thresholds, because neither is a quota reading.
function _paintBucket(refs, win, providerData) {
  if (_isExpiredReading(providerData, win)) {
    _blankBar(refs.bar);
    refs.pctEl.textContent = '—';
    if (refs.resetEl) refs.resetEl.textContent = 'window reset — reading expired';
    return;
  }
  if (typeof win.utilization !== 'number' || !Number.isFinite(win.utilization)) {
    _blankBar(refs.bar);
    refs.pctEl.textContent = '?';
    if (refs.resetEl) refs.resetEl.textContent = _formatWindowReset(providerData, win);
    return;
  }
  const pct = win.utilization;
  refs.bar.style.width = Math.min(pct, 100).toFixed(1) + '%';
  refs.bar.className = PROGRESS_BAR_FILL_CLASS + ' ' + _barColor(pct);
  refs.pctEl.textContent = Math.round(pct) + '%';
  if (refs.resetEl) refs.resetEl.textContent = _formatWindowReset(providerData, win);
}

function _bucketFieldPrefix(displayLabel) {
  return displayLabel.slice(0, -1).toLowerCase().replace(/[^a-z0-9]+/g, '-');
}

function _buildBucket(row, win, providerData) {
  const label = _windowLabel(win.window_minutes);
  const scopeLabel = win.scope_label;
  const scoped = typeof scopeLabel === 'string' && scopeLabel !== '';
  const displayLabel = label + (scoped ? ' ' + scopeLabel : '') + ':';
  const fieldPrefix = _bucketFieldPrefix(displayLabel);
  const group = _el('div', 'flex items-center gap-1.5');
  const lbl = _el('span', '');
  lbl.textContent = displayLabel;
  group.appendChild(lbl);
  const barWrap = _el('div', 'w-24 h-1.5 bg-slate-700 rounded-full overflow-hidden');
  const bar = _el('div', PROGRESS_BAR_FILL_CLASS);
  bar.setAttribute('data-field', fieldPrefix + '-bar');
  barWrap.appendChild(bar);
  group.appendChild(barWrap);
  const pctEl = _el('span', '');
  pctEl.setAttribute('data-field', fieldPrefix + '-pct');
  group.appendChild(pctEl);
  let resetEl = null;
  if (!scoped) {
    resetEl = _el('span', 'text-slate-500');
    resetEl.setAttribute('data-field', fieldPrefix + '-reset');
    group.appendChild(resetEl);
  }
  row.appendChild(group);

  const refs = {bar: bar, pctEl: pctEl, resetEl: resetEl};
  _paintBucket(refs, win, providerData);
  return refs;
}

function _buildNoCapMarker(row) {
  const marker = _el('span', 'text-slate-500');
  marker.setAttribute('data-field', 'no-cap');
  marker.textContent = 'plan · no cap';
  row.appendChild(marker);
}

// A row that names an account without reporting a quota: not yet read, or read
// and failed. Both must short-circuit the quota path, where an empty windows list
// renders as "plan · no cap" — an uncapped-plan claim about an unknown quota.
function _buildStatusRow(key, providerData, field, text) {
  const row = _el('div', 'flex items-center gap-1.5 text-slate-600 opacity-60');
  row.setAttribute('data-key', key);
  row.appendChild(_providerPill(providerData.provider));
  const label = _el('span', 'text-slate-500 font-medium');
  label.textContent = providerData.account || key;
  row.appendChild(label);
  const note = _el('span', 'italic');
  note.setAttribute('data-field', field);
  note.textContent = text;
  row.appendChild(note);
  return {row: row, buckets: [], asOfEl: null};
}

function _buildRow(key, providerData) {
  if (providerData.pending) return _buildStatusRow(key, providerData, 'pending', 'loading');
  if (providerData.error) return _buildStatusRow(key, providerData, 'error', providerData.error);

  const row = _el('div', 'flex items-center gap-1.5');
  row.setAttribute('data-key', key);
  row.appendChild(_providerPill(providerData.provider));
  const label = _el('span', 'text-slate-300 font-medium');
  label.textContent = providerData.account || key;
  row.appendChild(label);

  const windows = Array.isArray(providerData.windows) ? providerData.windows : [];
  const buckets = [];
  if (windows.length === 0) {
    _buildNoCapMarker(row);
  } else {
    for (const win of windows) {
      buckets.push({win: win, refs: _buildBucket(row, win, providerData)});
    }
  }

  let asOfEl = null;
  if (providerData.provider === 'codex') {
    const state = providerData.rate_limits_state || '';
    if (state) {
      const badge = _el('span', 'px-1.5 py-0.5 rounded bg-slate-700 text-[10px] font-medium text-slate-300');
      badge.setAttribute('data-field', 'state');
      badge.textContent = _providerStateLabel(state);
      row.appendChild(badge);
    }
    const asOf = _asOfText(providerData);
    if (asOf) {
      asOfEl = _el('span', 'text-slate-500');
      asOfEl.setAttribute('data-field', 'as-of');
      asOfEl.textContent = asOf;
      row.appendChild(asOfEl);
    }
    const spend = providerData.spend || {};
    const spendGroup = _el('div', 'flex items-center gap-1.5 text-slate-400');
    const l24 = _el('span', '');
    l24.textContent = '24h:';
    const s24 = _el('span', '');
    s24.setAttribute('data-field', 'spend-24h');
    s24.textContent = _formatSpendUsd(spend.last_24h_usd);
    const dot = _el('span', 'text-slate-700');
    dot.textContent = '·';
    const l7 = _el('span', '');
    l7.textContent = '7d:';
    const s7 = _el('span', '');
    s7.setAttribute('data-field', 'spend-7d');
    s7.textContent = _formatSpendUsd(spend.last_7d_usd);
    spendGroup.appendChild(l24);
    spendGroup.appendChild(s24);
    spendGroup.appendChild(dot);
    spendGroup.appendChild(l7);
    spendGroup.appendChild(s7);
    row.appendChild(spendGroup);
  } else if (providerData.provider !== 'claude') {
    throw new Error('unknown usage provider: ' + providerData.provider);
  }
  return {row: row, buckets: buckets, asOfEl: asOfEl};
}

// The collapsed summary's worst reading per account: the maximum utilization
// across every window the provider reported — scoped windows included — with
// expired and unknown readings excluded. An account whose readings all lose to
// those filters shows "—"; the account-level states mirror _buildRow's
// short-circuits: pending → loading, error → error, no windows → no cap.
function _summaryReading(providerData) {
  if (providerData.pending) return { text: 'loading', pct: null };
  if (providerData.error) return { text: 'error', pct: null };
  const windows = Array.isArray(providerData.windows) ? providerData.windows : [];
  if (windows.length === 0) return { text: 'no cap', pct: null };
  let worst = null;
  for (const win of windows) {
    if (_isExpiredReading(providerData, win)) continue;
    if (typeof win.utilization !== 'number' || !Number.isFinite(win.utilization)) continue;
    if (worst === null || win.utilization > worst) worst = win.utilization;
  }
  if (worst === null) return { text: '—', pct: null };
  const pct = Math.round(worst);
  return { text: pct + '%', pct: pct };
}

// Summary row: the strip's first child on every render with providers, shown
// only in collapsed state (CSS keys off the strip's .collapsed class). One
// segment per account — provider pill + account name + worst reading — then a
// trailing ▸. Clicking anywhere on the row expands the strip.
function _buildSummaryRow(providers) {
  const row = _el('div', 'flex items-center gap-1.5');
  row.setAttribute('data-field', 'summary');
  row.addEventListener('click', () => {
    if (_extUsageCollapsed) _setExtUsageCollapsed(false);
  });
  let first = true;
  for (const [key, providerData] of Object.entries(providers)) {
    if (!first) {
      const sep = _el('span', 'text-slate-700');
      sep.textContent = '·';
      row.appendChild(sep);
    }
    first = false;
    const seg = _el('span', 'inline-flex items-center gap-1.5');
    seg.setAttribute('data-summary-account', key);
    seg.appendChild(_providerPill(providerData.provider));
    const name = _el('span', 'text-slate-300 font-medium');
    name.textContent = providerData.account || key;
    seg.appendChild(name);
    const reading = _summaryReading(providerData);
    const pctEl = _el('span', reading.pct === null ? 'text-slate-500' : _barTextColor(reading.pct));
    pctEl.setAttribute('data-field', 'summary-pct');
    pctEl.textContent = reading.text;
    seg.appendChild(pctEl);
    row.appendChild(seg);
  }
  const chevron = _el('span', 'ml-auto text-slate-500');
  chevron.setAttribute('data-field', 'summary-chevron');
  chevron.textContent = '▸';
  row.appendChild(chevron);
  return row;
}

function renderExtUsage(data) {
  const strip = document.getElementById('ext-usage-strip');
  if (!strip) return;

  const providers = (data && data.providers) || {};
  const nodes = [];
  const rows = [];
  if (Object.keys(providers).length > 0) {
    nodes.push(_buildSummaryRow(providers));
  }
  for (const [key, providerData] of Object.entries(providers)) {
    const built = _buildRow(key, providerData);
    nodes.push(built.row);
    if (built.buckets.length > 0 || built.asOfEl) {
      rows.push({providerData: providerData, buckets: built.buckets, asOfEl: built.asOfEl});
    }
  }

  strip.replaceChildren(...nodes);
  _extUsageRows = rows;
  if (nodes.length > 0) {
    strip.classList.remove('hidden');
  }
  _syncExtUsageChrome(strip);
}

// Repaint rather than patch text: a reading can cross into "expired" purely by
// the passage of time, which changes the bar as well as the label.
function _refreshResetTimers() {
  for (const entry of _extUsageRows) {
    for (const bucket of entry.buckets) {
      _paintBucket(bucket.refs, bucket.win, entry.providerData);
    }
    if (entry.asOfEl) {
      entry.asOfEl.textContent = _asOfText(entry.providerData);
    }
  }
}

document.addEventListener('DOMContentLoaded', () => {
  // Evaluate the collapsed default exactly once: a recorded preference wins; a
  // missing/invalid one falls back to the platform (mobile → collapsed, and
  // expanded when platform is unavailable — rather show data than hide it).
  // The platform-derived default is never written back to storage.
  const stored = _readCollapsedPreference();
  _extUsageCollapsed = stored !== null
    ? stored
    : (typeof platform !== 'undefined' && !!platform.isMobile);

  const chip = document.getElementById('ext-usage-toggle');
  if (chip) {
    chip.addEventListener('click', () => _setExtUsageCollapsed(!_extUsageCollapsed));
  }

  fetch('/api/ext-usage')
    .then(r => r.json())
    .then(data => renderExtUsage(data))
    .catch(err => console.warn('ext-usage fetch failed:', err));

  // Refresh countdown timers every 60s (client-side only)
  startPageTimer('ext-usage-reset-timers', _refreshResetTimers, 60000);

  // Re-fetch usage data every 10 minutes
  startPageTimer('ext-usage-poll', () => {
    fetch('/api/ext-usage')
      .then(r => r.json())
      .then(data => renderExtUsage(data))
      .catch(err => console.warn('ext-usage poll failed:', err));
  }, 600000);
});
