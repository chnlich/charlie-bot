
(function() {
  const Chat = globalThis.Chat;

  // ---------------------------------------------------------------------------
  // Turn window engine — the #messages DOM is a projection of one contiguous
  // turn window near the viewport, with one height spacer above and below.
  //
  // Fetched messages live in an in-memory store; HTML build / markdown /
  // math / artifact / timestamp post-process run in scroll-gated idle slices
  // producing ready fragments. Collapsed turn bodies and out-of-window turns
  // are never in the DOM. Open/override, expanded `N steps` bars and open
  // recap panels live in a registry keyed by turn key (head|conclusion|
  // separator message ids). Spacer heights come from measurements once a turn
  // has materialized, from estimates otherwise, with scroll position corrected
  // whenever a measurement refines an estimate above the viewport.
  //
  // Scroll-path frames (rAF after a scroll event, inside the trailing
  // scroll-activity window) do only window bookkeeping, batched
  // measure-then-mutate DOM work, spacer updates and READY-fragment swaps.
  // The pre-render queue starts no new slice while scroll is active and never
  // uses a forcing requestIdleCallback timeout; unready turns entering the
  // window appear as estimated-height placeholder rows and are replaced when
  // ready. Queue order is nearer-to-viewport first; a manual fold-open jumps
  // the queue and renders that single turn synchronously.
  // ---------------------------------------------------------------------------
  const WINDOW_MARGIN_SCREENS = 2;
  // Safety bound, not the operative policy: the ±2-screen margins size the
  // window in pixels, and the cap only binds when rows are so thin that even
  // the margins hold more turns than this (small viewports, 4k screens). If
  // the cap binds where it shouldn't, its smaller window also collapses the
  // scroll hysteresis runway and forces a migration per wheel impulse.
  const MAX_WINDOW_TURNS = 256;
  const SCROLL_QUIET_MS = 200;
  const FRAME_BUDGET_MS = 16.7;
  const FOLDED_ROW_DEFAULT_EST = 30;
  const OPEN_BASE_EST = 64;
  const LINE_HEIGHT_EST = 21;
  const CHARS_PER_LINE_EST = 75;
  const FOLDED_MEASURE_MEMORY = 32;

  const STIMULUS_ROLES = ['user', 'scheduled_trigger', 'worker_summary'];

  function activeEngines() {
    if (!globalThis.__turnEngineInstances) globalThis.__turnEngineInstances = new WeakMap();
    return globalThis.__turnEngineInstances;
  }

  function supportsTurnEngine(container) {
    return Boolean(
      container
      && typeof document.createTreeWalker === 'function'
      && typeof document.createElement === 'function'
      && typeof container.addEventListener === 'function'
    );
  }

  function isStable(msg) {
    return msg.id != null && msg.id !== '' && Boolean(msg.role);
  }

  function contentChars(entry) {
    return String(entry.msg.content || '').length;
  }

  // The 9953270 rule over message JSON: any separator-terminated span with a
  // non-empty body is a turn; head = last user message before the conclusion,
  // else last stimulus, else body[0]. An empty-body bare separator stays flat.
  function describeSpan(span) {
    const stable = span.filter((entry) => isStable(entry.msg));
    const separator = stable[stable.length - 1];
    if (!separator) return null;
    const body = stable.slice(0, -1);
    let conclusion = null;
    for (let i = body.length - 1; i >= 0; i--) {
      if (body[i].msg.role === 'assistant') { conclusion = body[i]; break; }
    }
    let stimulus = null;
    const limit = conclusion ? body.indexOf(conclusion) : body.length;
    for (let i = limit - 1; i >= 0; i--) {
      if (body[i].msg.role === 'user') { stimulus = body[i]; break; }
      if (!stimulus && STIMULUS_ROLES.includes(body[i].msg.role)) stimulus = body[i];
    }
    const head = stimulus || body[0];
    if (!head) return null;
    const fold = conclusion
      ? body.slice(body.indexOf(head) + 1, body.indexOf(conclusion))
      : [];
    return {head, conclusion, separator, fold};
  }

  function turnKeyOf(turn) {
    return [
      String(turn.head.msg.id),
      turn.conclusion ? String(turn.conclusion.msg.id) : '',
      String(turn.separator.msg.id),
    ].join('|');
  }

  function rowSpecOf(turn) {
    return {
      headRole: turn.head.msg.role,
      headText: String(turn.head.msg.content || ''),
      headTs: turn.head.msg.timestamp || null,
      conclusionText: turn.conclusion ? String(turn.conclusion.msg.content || '') : null,
      thinkingSeconds: turn.separator.msg.thinking_seconds != null
        ? turn.separator.msg.thinking_seconds
        : null,
      foldCount: turn.fold.length,
    };
  }

  function makeStats() {
    return {
      prerenderAtoms: [],
      prerenderAtomMaxMs: 0,
      prerenderAtomsOverBudget: [],
      slicesDeferredForScroll: 0,
      slicesStartedDuringScrollWindow: 0,
      placeholderAppearances: 0,
      windowMigrations: 0,
      derivations: 0,
      segmentMaterializations: 0,
      rederivesOfSettledTurns: 0,
      measuredTurnCount: 0,
      scrollFramesConsidered: 0,
      scrollFramesSkipped: 0,
      scrollFramesMigrated: 0,
    };
  }

  class TurnEngine {
    constructor(container, sessionId) {
      this.container = container;
      this.sessionId = sessionId;
      this.alive = true;
      this.entries = [];
      this.knownIds = new Set();
      this.segments = [];
      this.registry = new Map();
      this.measuredFolded = new Map();
      this.measuredOpen = new Map();
      this.measuredFlat = new WeakMap();
      this.foldedRowMeasures = [];
      this.domBySeg = new Map();
      this.window = null;
      this.offsets = [];
      this.offsetsDirty = true;
      this.totalHeight = 0;
      this.lastScrollTs = -Infinity;
      // Mount intent is bottom-pinned; genuine user scrolls re-derive it and
      // follow-appends re-arm it. It is the pin source for 'resize' projects.
      this.pinnedIntent = true;
      this.rafScheduled = false;
      this.rafHandle = null;
      this.resizeObserver = null;
      this.resizeRafScheduled = false;
      this.resizeRafHandle = null;
      this.idleScheduled = false;
      this.idleHandle = null;
      this.cancelIdle = null;
      this.quietTimer = null;
      this.stats = makeStats();
      this.liveMessageSequence = 0;
      this.onScroll = () => this.handleScroll();
      container.addEventListener('scroll', this.onScroll, {passive: true});
      // Container size is a projection input, so the engine observes it
      // itself: tab visibility flips and window/sidebar resizes all funnel
      // into reproject('resize'), coalesced to one frame per burst on its own
      // scheduling flag (never the scroll flag). Runtimes without
      // ResizeObserver install nothing and keep today's behavior exactly.
      if (typeof ResizeObserver === 'function') {
        this.resizeObserver = new ResizeObserver(() => {
          if (this.resizeRafScheduled || !this.alive) return;
          this.resizeRafScheduled = true;
          const fire = () => {
            this.resizeRafHandle = null;
            this.resizeRafScheduled = false;
            this.handleResize();
          };
          if (typeof requestAnimationFrame === 'function') {
            this.resizeRafHandle = requestAnimationFrame(fire);
          } else {
            this.resizeRafHandle = setTimeout(fire, 0);
          }
        });
        this.resizeObserver.observe(container);
      }
    }

    // ---- registry -----------------------------------------------------------
    state(key) {
      let state = this.registry.get(key);
      if (!state) {
        state = {override: null, nStepsExpanded: false, recapOpen: false};
        this.registry.set(key, state);
      }
      return state;
    }

    newestFinishedTurnKey() {
      for (let i = this.segments.length - 1; i >= 0; i--) {
        const seg = this.segments[i];
        if (seg.kind === 'turn') return seg.key;
      }
      return null;
    }

    effectiveOpen(seg) {
      const state = this.registry.get(seg.key);
      if (state && state.override) return state.override === 'open';
      if (Chat.pageDepth === 'outline') return this.newestFinishedTurnKey() === seg.key;
      return true;
    }

    effectiveNStepsExpanded(seg) {
      if (Chat.pageDepth === 'expanded') return true;
      const state = this.registry.get(seg.key);
      return Boolean(state && state.nStepsExpanded);
    }

    // ---- derivation ---------------------------------------------------------
    // One pass over a message-entry run. A span closes at a stable separator;
    // the tail of a run that never closes stays pending. Settled segments are
    // immutable: their span can no longer change, so pagination only ever
    // re-derives the boundary span.
    deriveSegments(entries) {
      const segments = [];
      let span = [];
      entries.forEach((entry) => {
        span.push(entry);
        if (isStable(entry.msg) && entry.msg.role === 'separator') {
          segments.push(this.closeSpan(span));
          span = [];
        }
      });
      this.stats.derivations++;
      return {segments, tailSpan: span};
    }

    closeSpan(span) {
      const turn = describeSpan(span);
      if (!turn) return {kind: 'flat', entries: span.slice(), pending: false};
      const key = turnKeyOf(turn);
      return {
        kind: 'turn',
        key,
        entries: span.slice(),
        foldEntries: turn.fold.slice(),
        separatorEntry: turn.separator,
        rowSpec: rowSpecOf(turn),
      };
    }

    makeEntry(msg) {
      const entry = {msg, node: null, ready: false};
      this.entries.push(entry);
      if (isStable(msg)) this.knownIds.add(String(msg.id));
      return entry;
    }

    // ---- height model -------------------------------------------------------
    foldedRowEstimate() {
      if (!this.foldedRowMeasures.length) return FOLDED_ROW_DEFAULT_EST;
      const sum = this.foldedRowMeasures.reduce((acc, value) => acc + value, 0);
      return sum / this.foldedRowMeasures.length;
    }

    noteFoldedMeasure(height) {
      this.foldedRowMeasures.push(height);
      if (this.foldedRowMeasures.length > FOLDED_MEASURE_MEMORY) this.foldedRowMeasures.shift();
    }

    openEstimate(seg) {
      let chars = 0;
      seg.entries.forEach((entry) => { chars += contentChars(entry); });
      return OPEN_BASE_EST
        + Math.max(1, Math.ceil(chars / CHARS_PER_LINE_EST)) * LINE_HEIGHT_EST
        + seg.rowSpec.foldCount * 36;
    }

    flatEstimate(seg) {
      let height = 0;
      seg.entries.forEach((entry) => {
        height += OPEN_BASE_EST
          + Math.max(1, Math.ceil(contentChars(entry) / CHARS_PER_LINE_EST)) * LINE_HEIGHT_EST;
      });
      return height;
    }

    heightOf(seg) {
      if (seg.kind === 'flat') {
        const measured = this.measuredFlat.get(seg);
        return measured != null ? measured : this.flatEstimate(seg);
      }
      if (!this.effectiveOpen(seg)) {
        const measured = this.measuredFolded.get(seg.key);
        return measured != null ? measured : this.foldedRowEstimate();
      }
      const sig = this.effectiveNStepsExpanded(seg) ? 'open-x' : 'open-c';
      const measured = this.measuredOpen.get(seg.key + '|' + sig);
      return measured != null ? measured : this.openEstimate(seg);
    }

    ensureOffsets() {
      if (!this.offsetsDirty) return;
      const offsets = new Array(this.segments.length);
      let total = 0;
      this.segments.forEach((seg, i) => {
        offsets[i] = total;
        total += this.heightOf(seg);
      });
      this.offsets = offsets;
      this.totalHeight = total;
      this.offsetsDirty = false;
    }

    // ---- fragments and wraps -------------------------------------------------
    buildNode(entry) {
      if (typeof Chat.buildTurnEngineMessageNode === 'function') {
        return Chat.buildTurnEngineMessageNode(entry.msg, this.sessionId);
      }
      const tmp = document.createElement('div');
      tmp.innerHTML = globalThis.renderMessage(entry.msg, this.sessionId);
      const fragmentRoot = tmp.firstElementChild || tmp;
      fragmentRoot.__turnEngineReadyFragment = true;
      Chat.postProcessRenderedMessages(tmp);
      return fragmentRoot;
    }

    renderAtom(entry, sync) {
      const t0 = performance.now();
      const node = this.buildNode(entry);
      const ms = performance.now() - t0;
      this.stats.prerenderAtoms.push({
        ms,
        id: entry.msg.id != null ? String(entry.msg.id) : null,
        role: entry.msg.role,
        chars: contentChars(entry),
        sync: Boolean(sync),
      });
      if (ms > this.stats.prerenderAtomMaxMs) this.stats.prerenderAtomMaxMs = ms;
      if (ms > FRAME_BUDGET_MS) {
        this.stats.prerenderAtomsOverBudget.push({
          ms,
          id: entry.msg.id != null ? String(entry.msg.id) : null,
          role: entry.msg.role,
          chars: contentChars(entry),
          sync: Boolean(sync),
        });
      }
      entry.node = node;
      entry.ready = true;
    }

    segmentReady(seg) {
      return seg.entries.every((entry) => entry.ready);
    }

    buildTurnWrap(seg, open) {
      const wrap = document.createElement('div');
      wrap.className = 'turn-wrap';
      wrap.dataset.turnKey = seg.key;
      wrap.dataset.turnOpen = String(open);
      wrap.appendChild(Chat.buildTurnRowFromSpec(seg.rowSpec, seg.key));
      if (!open) return wrap;

      seg.entries.forEach((entry) => wrap.appendChild(entry.node));
      Chat.installTurnFold(wrap, seg.foldEntries.map((entry) => entry.node), seg.key);
      Chat.installTurnCollapseControl(seg.separatorEntry.node);
      if (seg.rowSpec.foldCount > 0 && this.effectiveNStepsExpanded(seg)) {
        const bar = wrap.querySelector('.turn-fold-bar');
        if (bar) Chat.setTurnFoldExpanded(bar, true);
      }
      this.restoreRecapPanel(wrap, seg);
      this.stats.segmentMaterializations++;
      return wrap;
    }

    restoreRecapPanel(wrap, seg) {
      const state = this.registry.get(seg.key);
      if (!state || !state.recapOpen) return;
      const eventIndex = seg.separatorEntry.msg.event_index;
      if (eventIndex == null) throw new Error('recap-open turn without separator event_index');
      const btn = seg.separatorEntry.node.querySelector('.recap-toggle');
      if (!btn) throw new Error('recap toggle button missing on separator');
      Chat.toggleRecapPanel(btn, this.sessionId, eventIndex);
    }

    buildPlaceholder(seg) {
      const el = document.createElement('div');
      el.className = 'turn-placeholder';
      el.style.height = Math.round(this.heightOf(seg)) + 'px';
      if (seg.kind === 'turn') el.dataset.turnKey = seg.key;
      el.textContent = '…';
      return el;
    }

    buildFlatNode(seg) {
      const frag = document.createElement('div');
      frag.className = 'turn-flat';
      seg.entries.forEach((entry) => frag.appendChild(entry.node));
      return frag;
    }

    // DOM signature of the materialization a segment currently needs.
    materializationSig(seg) {
      if (seg.kind === 'flat') return 'flat:' + seg.entries.length + ':' + this.segmentReady(seg);
      return [
        this.effectiveOpen(seg) ? 'open' : 'folded',
        seg.rowSpec.foldCount,
        this.effectiveNStepsExpanded(seg),
        this.segmentReady(seg),
      ].join(':');
    }

    // Returns the DOM node for one segment under the current policy — a READY
    // fragment or an estimated-height placeholder. Never builds HTML here.
    materialize(seg) {
      if (seg.kind === 'turn') {
        const open = this.effectiveOpen(seg);
        if (!open) return this.buildTurnWrap(seg, false);
        if (!this.segmentReady(seg)) {
          this.stats.placeholderAppearances++;
          return this.buildPlaceholder(seg);
        }
        return this.buildTurnWrap(seg, true);
      }
      if (!this.segmentReady(seg)) {
        this.stats.placeholderAppearances++;
        return this.buildPlaceholder(seg);
      }
      this.stats.segmentMaterializations++;
      return this.buildFlatNode(seg);
    }

    measureSegment(seg, el) {
      const height = el.getBoundingClientRect().height;
      this.stats.measuredTurnCount++;
      if (el.classList.contains('turn-placeholder')) return height;
      if (seg.kind === 'flat') {
        this.measuredFlat.set(seg, height);
      } else if (!this.effectiveOpen(seg)) {
        this.measuredFolded.set(seg.key, height);
        this.noteFoldedMeasure(height);
      } else {
        const sig = this.effectiveNStepsExpanded(seg) ? 'open-x' : 'open-c';
        this.measuredOpen.set(seg.key + '|' + sig, height);
      }
      return height;
    }

    // ---- pre-render queue ---------------------------------------------------
    queueEntries() {
      this.ensureOffsets();
      const out = [];
      this.segments.forEach((seg, segIndex) => {
        if (seg.kind === 'turn' && !this.effectiveOpen(seg)) return;
        seg.entries.forEach((entry) => {
          if (!entry.ready) out.push({entry, segIndex});
        });
      });
      if (out.length < 2) return out;
      const viewportCenter = this.container.scrollTop + this.container.clientHeight / 2;
      const distance = (item) => {
        const seg = this.segments[item.segIndex];
        const segmentCenter = this.offsets[item.segIndex] + this.heightOf(seg) / 2;
        return Math.abs(segmentCenter - viewportCenter);
      };
      return out.sort((a, b) => distance(a) - distance(b));
    }

    scrollRecentlyActive() {
      return performance.now() - this.lastScrollTs < SCROLL_QUIET_MS;
    }

    scheduleIdle() {
      if (!this.alive || this.idleScheduled || this.quietTimer) return;
      this.idleScheduled = true;
      const arm = () => {
        this.idleHandle = null;
        this.cancelIdle = null;
        this.idleScheduled = false;
        this.runSlice();
      };
      // No forcing timeout: the queue only runs while the browser is idle.
      if (typeof requestIdleCallback === 'function') {
        const handle = requestIdleCallback(arm);
        this.idleHandle = handle;
        if (typeof cancelIdleCallback === 'function') {
          this.cancelIdle = () => cancelIdleCallback(handle);
        }
      } else {
        const handle = setTimeout(arm, 0);
        this.idleHandle = handle;
        this.cancelIdle = () => clearTimeout(handle);
      }
    }

    runSlice() {
      if (!this.alive) return;
      if (this.scrollRecentlyActive()) {
        this.stats.slicesDeferredForScroll++;
        if (!this.quietTimer) {
          this.quietTimer = setTimeout(() => {
            this.quietTimer = null;
            this.runSlice();
          }, SCROLL_QUIET_MS);
        }
        return;
      }
      const queue = this.queueEntries();
      if (!queue.length) return;
      this.renderAtom(queue[0].entry, false);
      if (this.window
          && queue[0].segIndex >= this.window.start
          && queue[0].segIndex <= this.window.end) {
        this.reproject('prerender-ready');
      }
      this.scheduleIdle();
    }

    // ---- window --------------------------------------------------------------
    computeTargetRange() {
      this.ensureOffsets();
      const count = this.segments.length;
      if (count === 0) return {start: 0, end: -1};
      const scrollTop = this.container.scrollTop;
      const viewport = this.container.clientHeight;
      const lowBound = Math.max(0, scrollTop - WINDOW_MARGIN_SCREENS * viewport);
      const highBound = scrollTop + viewport + WINDOW_MARGIN_SCREENS * viewport;

      let first = 0;
      while (first < count
          && this.offsets[first] + this.heightOf(this.segments[first]) < lowBound) first++;
      if (first >= count) first = count - 1;

      let end = first;
      while (end + 1 < count && this.offsets[end + 1] < highBound) end++;

      while (end - first + 1 > MAX_WINDOW_TURNS) {
        const lowSlack = lowBound - (this.offsets[first] + this.heightOf(this.segments[first]));
        const highSlack = this.offsets[end] + this.heightOf(this.segments[end]) - highBound;
        if (lowSlack > highSlack) first++;
        else end--;
      }
      return {start: first, end};
    }

    anchorFor(scrollTop) {
      this.ensureOffsets();
      for (let i = 0; i < this.segments.length; i++) {
        if (this.offsets[i] + this.heightOf(this.segments[i]) > scrollTop) return i;
      }
      return this.segments.length - 1;
    }

    // Bottom-pinned target window: the trailing segments covering the viewport
    // plus the margins, walked back through the height model.
    pinnedRange() {
      this.ensureOffsets();
      const count = this.segments.length;
      if (count === 0) return {start: 0, end: -1};
      const need = (WINDOW_MARGIN_SCREENS + 1) * this.container.clientHeight;
      let total = 0;
      let start = count - 1;
      while (start > 0 && total < need) {
        total += this.heightOf(this.segments[start]);
        start--;
      }
      while (count - start > MAX_WINDOW_TURNS) start++;
      return {start, end: count - 1};
    }

    isPinned() {
      // The browser's real scroll range (spacers + window + sibling margins +
      // the streaming fixture) differs from the engine's height model by the
      // container's space-y-3 margins; pinning to the model would leave the
      // bottom margin worth of content unreachable.
      const el = this.container;
      return el.scrollHeight - el.scrollTop - el.clientHeight < 150;
    }

    // Window bookkeeping + DOM projection. One reproject covers everything:
    // scroll migrations, ingest reshapes, estimate refinements and policy
    // changes. Reads batched first (viewport + anchor offsets), mutations
    // second, one measurement batch after the swap, scroll correction last.
    reproject(reason) {
      if (!this.alive) return;
      // Zero-height invariant: a hidden container cannot size a window, so
      // for every reason there is no projection at all — no nodes, no spacer
      // writes, no scrollTop writes. The first non-zero 'resize' projects
      // from scratch instead of repairing a degenerate window.
      if (this.container.clientHeight === 0) return;
      this.ensureOffsets();
      // Hysteresis: while the viewport stays comfortably inside the current
      // window, a scroll frame is pure bookkeeping — no DOM work at all.
      if (reason === 'scroll' && this.window) {
        this.stats.scrollFramesConsidered++;
        const ws = this.window;
        const vh = this.container.clientHeight;
        const winTop = this.offsets[ws.start] || 0;
        const winBottom = ws.end >= ws.start
          ? this.offsets[ws.end] + this.heightOf(this.segments[ws.end])
          : winTop;
        const viewTop = this.container.scrollTop;
        if (viewTop >= winTop + 0.5 * vh && viewTop + vh <= winBottom - 0.5 * vh) {
          this.stats.scrollFramesSkipped++;
          return;
        }
      }
      // A resize moves the very geometry isPinned() would read (hidden-mount
      // recovery starts from scrollTop 0; a growing viewport drops content
      // into the follow band), so 'resize' projects pin from the user's
      // intent instead. Every other reason keeps live geometry.
      const wasPinned = reason === 'resize' ? this.pinnedIntent : this.isPinned();
      // At the pinned bottom the browser's scrollTop and the height model
      // disagree by the container's sibling margins, which makes a
      // scrollTop-derived range flap ±1 segment in a limit cycle. The pinned
      // range is therefore derived from the model's own offsets — stable by
      // construction.
      const range = wasPinned ? this.pinnedRange() : this.computeTargetRange();
      const scrollTop = this.container.scrollTop;
      const anchor = this.segments.length ? this.anchorFor(scrollTop) : null;
      const anchorOffsetBefore = anchor != null && anchor >= 0 ? this.offsets[anchor] : 0;

      // Classify the work first: evicted segments, changed segments replaced
      // in place (materialization sig moved — policy flip, placeholder
      // resolved, pending tail growth), brand-new window members.
      const evicted = [];
      this.domBySeg.forEach((el, seg) => {
        const index = this.segments.indexOf(seg);
        if (index < range.start || index > range.end) evicted.push(seg);
      });
      const replaced = [];
      const inserted = [];
      for (let i = range.start; i <= range.end; i++) {
        const seg = this.segments[i];
        const sig = this.materializationSig(seg);
        const current = this.domBySeg.get(seg);
        if (current && seg.cachedSig === sig) continue;
        if (current) replaced.push({seg, current, sig});
        else inserted.push({seg, sig, index: i});
      }

      // Phase 1 (reads): measure every outgoing node in one layout pass.
      evicted.forEach((seg) => this.measureSegment(seg, this.domBySeg.get(seg)));
      replaced.forEach(({seg, current}) => this.measureSegment(seg, current));

      // Phase 2 (writes): the whole mutation batch — removals, replacements,
      // insertions — with no reads in between.
      evicted.forEach((seg) => {
        this.domBySeg.get(seg).remove();
        this.domBySeg.delete(seg);
      });
      const freshNodes = [];
      replaced.forEach(({seg, current, sig}) => {
        const node = this.materialize(seg);
        current.replaceWith(node);
        this.domBySeg.set(seg, node);
        seg.cachedSig = sig;
        freshNodes.push({seg, node});
      });
      inserted.forEach(({seg, sig, index}) => {
        const node = this.materialize(seg);
        const next = this.nextInWindow(index + 1, range.end);
        this.container.insertBefore(node, next || this.bottomSpacer);
        this.domBySeg.set(seg, node);
        seg.cachedSig = sig;
        freshNodes.push({seg, node});
      });

      // Phase 3 (reads): measure the fresh nodes in one more layout pass.
      freshNodes.forEach(({seg, node}) => this.measureSegment(seg, node));

      if (evicted.length || replaced.length || inserted.length) {
        this.stats.windowMigrations++;
        if (reason === 'scroll') this.stats.scrollFramesMigrated++;
        this.offsetsDirty = true;
      }
      this.window = range;

      // 3. Heights moved: refresh offsets and rewrite both spacers.
      this.offsetsDirty = true;
      this.ensureOffsets();
      this.topSpacer.style.height = Math.round(range.start < this.offsets.length ? this.offsets[range.start] : 0) + 'px';
      const covered = range.end >= range.start
        ? this.totalHeight - (this.offsets[range.end] + this.heightOf(this.segments[range.end]))
        : this.totalHeight;
      this.bottomSpacer.style.height = Math.round(Math.max(0, covered)) + 'px';

      // 4. Scroll correction: anchor keeps its visual position, or stay pinned.
      // User scroll and quiet-phase pre-rendering are new engine reasons; they
      // must not turn a near-bottom viewport back into a pinned one. The
      // positional snap remains for the legacy-equivalent reasons.
      const snapToBottom = wasPinned && reason !== 'scroll' && reason !== 'prerender-ready';
      if (snapToBottom) {
        this.writeScrollTop(this.container.scrollHeight);
      } else if (anchor != null && anchor >= 0 && this.offsets[anchor] != null) {
        const delta = this.offsets[anchor] - anchorOffsetBefore;
        if (delta !== 0) this.writeScrollTop(scrollTop + delta);
      }
    }

    nextInWindow(fromIndex, endIndex) {
      for (let i = fromIndex; i <= endIndex; i++) {
        const el = this.domBySeg.get(this.segments[i]);
        if (el) return el;
      }
      return null;
    }

    // Browser scroll events arrive asynchronously, so the engine's own
    // corrections mark their value target: an event whose scrollTop equals the
    // last programmatic write is the echo of that write, not user activity —
    // it must not pause the pre-render queue nor kick a redundant reproject.
    writeScrollTop(value) {
      this.container.scrollTop = value;
      this.lastProgrammaticScrollTop = this.container.scrollTop;
    }

    handleScroll() {
      if (this.container.scrollTop === this.lastProgrammaticScrollTop) return;
      this.pinnedIntent = this.isPinned();
      this.lastScrollTs = performance.now();
      this.lastProgrammaticScrollTop = null;
      if (this.rafScheduled || !this.alive) return;
      this.rafScheduled = true;
      const fire = () => {
        this.rafHandle = null;
        this.rafScheduled = false;
        this.reproject('scroll');
      };
      if (typeof requestAnimationFrame === 'function') {
        this.rafHandle = requestAnimationFrame(fire);
      } else {
        this.rafHandle = setTimeout(fire, 0);
      }
    }

    // The ResizeObserver funnel: size recovery (tab shown, window or sidebar
    // drag) re-projects. Zero height is already the reproject invariant's
    // business and the 'resize' pin source is pinnedIntent, so there is no
    // scrollTop pre-write here.
    handleResize() {
      if (!this.alive) return;
      this.reproject('resize');
    }

    // ---- lifecycle -----------------------------------------------------------
    mount(messages) {
      const streamEl = document.getElementById('streaming-msg');
      while (this.container.firstChild) this.container.removeChild(this.container.firstChild);

      messages.forEach((msg) => this.makeEntry(msg));
      const derived = this.deriveSegments(this.entries);
      this.segments = derived.segments;
      if (derived.tailSpan.length) {
        this.segments.push({kind: 'flat', entries: derived.tailSpan.slice(), pending: true});
      }
      this.offsetsDirty = true;

      this.topSpacer = document.createElement('div');
      this.topSpacer.className = 'turn-spacer turn-spacer-top';
      this.bottomSpacer = document.createElement('div');
      this.bottomSpacer.className = 'turn-spacer turn-spacer-bottom';
      this.container.appendChild(this.topSpacer);
      this.container.appendChild(this.bottomSpacer);
      if (streamEl) this.container.appendChild(streamEl);

      this.ensureOffsets();
      // Seed the spacers so the browser exposes its full range, then pin
      // against the real scrollHeight (margins included), then project. A
      // hidden (zero-height) container skips the bottom-spacer seed and both
      // pin writes: reproject guards itself, so a hidden mount projects
      // nothing and the first 'resize' builds the window instead.
      this.topSpacer.style.height = '0px';
      if (this.container.clientHeight !== 0) {
        this.bottomSpacer.style.height = Math.round(this.totalHeight) + 'px';
        this.writeScrollTop(this.container.scrollHeight);
      }
      this.reproject('mount');
      if (this.container.clientHeight !== 0) this.writeScrollTop(this.container.scrollHeight);
      this.scheduleIdle();
      globalThis.__turnEngineStats = this.stats;
    }

    dispose() {
      this.alive = false;
      this.container.removeEventListener('scroll', this.onScroll);
      if (this.cancelIdle) this.cancelIdle();
      this.idleHandle = null;
      this.cancelIdle = null;
      this.idleScheduled = false;
      if (this.quietTimer) clearTimeout(this.quietTimer);
      this.quietTimer = null;
      if (this.rafHandle != null) {
        if (typeof cancelAnimationFrame === 'function') cancelAnimationFrame(this.rafHandle);
        else clearTimeout(this.rafHandle);
      }
      this.rafHandle = null;
      this.rafScheduled = false;
      if (this.resizeObserver) {
        this.resizeObserver.disconnect();
        this.resizeObserver = null;
      }
      if (this.resizeRafHandle != null) {
        if (typeof cancelAnimationFrame === 'function') cancelAnimationFrame(this.resizeRafHandle);
        else clearTimeout(this.resizeRafHandle);
      }
      this.resizeRafHandle = null;
      this.resizeRafScheduled = false;
      this.entries = [];
      this.knownIds.clear();
      this.segments = [];
      this.registry.clear();
      this.measuredFolded.clear();
      this.measuredOpen.clear();
      this.measuredFlat = new WeakMap();
      this.foldedRowMeasures = [];
      this.domBySeg.clear();
      this.offsets = [];
      this.offsetsDirty = false;
      this.totalHeight = 0;
      if (globalThis.__turnEngineStats === this.stats) delete globalThis.__turnEngineStats;
      activeEngines().delete(this.container);
    }

    // ---- ingest: pagination ---------------------------------------------------
    // Incremental per-page derivation: the new page plus, when its trailing
    // span does not close, as many leading settled segments as that span
    // swallows. Everything further down the store is never touched — settled
    // turn wrappers are not re-derived, and the return value is the pixel
    // shift the existing content moved down by.
    prependMessages(messages) {
      const fresh = messages.filter((msg) => {
        if (!isStable(msg)) return true;
        const id = String(msg.id);
        return !this.knownIds.has(id);
      }).map((msg) => this.makeEntry(msg));
      if (!fresh.length) return 0;

      this.ensureOffsets();
      const scrollTopBefore = this.container.scrollTop;
      const anchor = this.anchorFor(scrollTopBefore);
      const anchorEntriesBefore = anchor >= 0 ? this.segments[anchor].entries : null;
      const anchorOffsetBefore = anchor >= 0 ? this.offsets[anchor] : 0;

      const firstPage = this.deriveSegments(fresh);
      // Insert the page's settled prefix first. The page tail must then be
      // merged with the existing leading segment after that prefix; using
      // index 0 here would merge it back into the page's newest turn and lose
      // the tail when an archived page contains both complete and partial
      // spans.
      this.segments.splice(0, 0, ...firstPage.segments);
      let boundaryIndex = firstPage.segments.length;
      let region = firstPage.tailSpan.slice();
      while (region.length) {
        if (boundaryIndex >= this.segments.length) {
          this.segments.splice(boundaryIndex, 0, {
            kind: 'flat', entries: region.slice(), pending: true,
          });
          break;
        }
        const boundary = this.deriveSegments(
          region.concat(this.segments[boundaryIndex].entries));
        this.segments.splice(boundaryIndex, 1, ...boundary.segments);
        this.stats.rederivesOfSettledTurns++;
        if (!boundary.tailSpan.length) break;
        region = boundary.tailSpan.slice();
        boundaryIndex += boundary.segments.length;
      }
      this.domBySeg.forEach((el, seg) => {
        if (!this.segments.includes(seg)) {
          el.remove();
          this.domBySeg.delete(seg);
        }
      });
      this.offsetsDirty = true;
      this.scheduleIdle();
      this.ensureOffsets();

      // Keep the anchored content visually still: the anchor is identified by
      // its own message identity — never by position, which pagination moves.
      let shift = 0;
      if (anchorEntriesBefore && anchorEntriesBefore.length) {
        const anchorIndexAfter = this.segments.findIndex(
          (seg) => seg.entries.includes(anchorEntriesBefore[0]));
        if (anchorIndexAfter >= 0) shift = this.offsets[anchorIndexAfter] - anchorOffsetBefore;
      }
      if (shift) this.writeScrollTop(scrollTopBefore + shift);
      this.reproject('prepend');
      return shift;
    }

    // ---- ingest: live stream --------------------------------------------------
    appendMessage(msg, forceScroll) {
      if (msg.id == null || msg.id === '') {
        msg = Object.assign({}, msg, {
          id: 'client:' + String(this.sessionId) + ':' + (++this.liveMessageSequence),
        });
      }
      const wasPinned = this.isPinned();
      const entry = {msg, node: null, ready: false};
      this.renderAtom(entry, true);
      this.entries.push(entry);
      if (isStable(msg)) this.knownIds.add(String(msg.id));

      const tail = this.segments[this.segments.length - 1];
      if (!tail || !(tail.kind === 'flat' && tail.pending)) {
        this.segments.push({kind: 'flat', entries: [], pending: true});
      }
      const pending = this.segments[this.segments.length - 1];
      pending.entries.push(entry);
      pending.cachedSig = null;

      let closed = null;
      if (isStable(msg) && msg.role === 'separator') {
        closed = this.closeSpan(pending.entries);
        this.segments[this.segments.length - 1] = closed;
        const stale = this.domBySeg.get(pending);
        if (stale) {
          stale.remove();
          this.domBySeg.delete(pending);
        }
        this.stats.derivations++;
      }
      this.offsetsDirty = true;
      this.reproject('append');
      if (forceScroll || wasPinned) {
        // A follow-append re-arms the pin intent; it never clears it here —
        // only a genuine user scroll may do that (handleScroll).
        this.pinnedIntent = true;
        this.writeScrollTop(this.container.scrollHeight);
      } else if (typeof showScrollToBottom === 'function') {
        showScrollToBottom();
      }
      this.scheduleIdle();
      return closed;
    }

    // ---- user-driven state -----------------------------------------------------
    // Manual fold-open jumps the pre-render queue: this single turn renders
    // synchronously, then the projection replaces it in place.
    setOverride(key, open) {
      const index = this.segments.findIndex((seg) => seg.kind === 'turn' && seg.key === key);
      if (index === -1) throw new Error('setOverride for unknown turn ' + key);
      this.state(key).override = open ? 'open' : 'folded';
      const seg = this.segments[index];
      seg.cachedSig = null;
      if (open) {
        seg.entries.forEach((entry) => {
          if (!entry.ready) this.renderAtom(entry, true);
        });
      }
      this.measuredFolded.delete(seg.key);
      this.measuredOpen.delete(seg.key + '|open-c');
      this.measuredOpen.delete(seg.key + '|open-x');
      this.offsetsDirty = true;
      this.reproject('override');
      this.scheduleIdle();
    }

    toggleNSteps(btn) {
      const key = btn.dataset.turnFoldKey;
      if (!key) throw new Error('turn fold bar without a key');
      const content = btn.nextElementSibling;
      const expanded = Boolean(content && content.classList.contains('hidden'));
      this.state(key).nStepsExpanded = expanded;
      Chat.setTurnFoldExpanded(btn, expanded);
      const seg = this.segments.find((s) => s.kind === 'turn' && s.key === key);
      if (seg) {
        seg.cachedSig = null;
        this.measuredOpen.delete(seg.key + '|open-c');
        this.measuredOpen.delete(seg.key + '|open-x');
        this.offsetsDirty = true;
        this.reproject('nsteps');
      }
    }

    noteRecapToggle(btn) {
      if (!this.alive || !btn.closest) return;
      const wrap = btn.closest('.turn-wrap');
      if (!wrap || !wrap.dataset.turnKey) return;
      const sep = btn.closest('.separator-line');
      const open = Boolean(
        sep && sep.nextElementSibling && sep.nextElementSibling.classList.contains('recap-panel'));
      this.state(wrap.dataset.turnKey).recapOpen = open;
    }

    setDepth(depth) {
      if (depth !== 'expanded') {
        this.registry.forEach((state) => { state.nStepsExpanded = false; });
      }
      this.measuredOpen.clear();
      this.segments.forEach((seg) => { seg.cachedSig = null; });
      this.offsetsDirty = true;
      this.reproject('depth');
      this.scheduleIdle();
    }
  }

  // ---------------------------------------------------------------------------
  // Recap toggle wrapping: after the original toggle runs, record the panel's
  // open state in the active engine's registry so it can be restored on
  // re-materialization.
  // ---------------------------------------------------------------------------
  const originalToggleRecapPanel = Chat.toggleRecapPanel;
  function toggleRecapPanelTracked(btn, sessionId, eventIndex) {
    const result = originalToggleRecapPanel.apply(this, arguments);
    const container = document.getElementById('messages');
    const engine = activeEngines().get(container);
    if (engine) engine.noteRecapToggle(btn);
    return result;
  }
  Chat.toggleRecapPanel = toggleRecapPanelTracked;
  globalThis.toggleRecapPanel = toggleRecapPanelTracked;

  function mountIfAvailable(container, messages, sessionId) {
    if (!supportsTurnEngine(container)) return null;
    const prev = activeEngines().get(container);
    if (prev) prev.dispose();
    const engine = new TurnEngine(container, sessionId);
    activeEngines().set(container, engine);
    engine.mount(messages || []);
    return engine;
  }

  function activeFor(container) {
    const engine = activeEngines().get(container);
    return engine && engine.alive ? engine : null;
  }

  Chat.TurnEngine = {
    SCROLL_QUIET_MS,
    MAX_WINDOW_TURNS,
    supportsTurnEngine,
    mountIfAvailable,
    activeFor,
    isHosted(container) {
      return Boolean(activeFor(container));
    },
    debug(container) {
      const engine = activeFor(container);
      if (!engine) return null;
      engine.ensureOffsets();
      return {
        entries: engine.entries.length,
        segments: engine.segments.length,
        window: engine.window,
        totalHeight: engine.totalHeight,
        offsets: engine.offsets.slice(),
        heights: engine.segments.map((seg) => engine.heightOf(seg)),
        keys: engine.segments.map((seg) => (seg.kind === 'turn' ? seg.key : null)),
        pending: engine.segments.map((seg) => Boolean(seg.pending)),
        inDom: engine.domBySeg.size,
        stats: engine.stats,
        queueLength: engine.queueEntries().length,
        lastScrollAgeMs: performance.now() - engine.lastScrollTs,
      };
    },
  };
})();
