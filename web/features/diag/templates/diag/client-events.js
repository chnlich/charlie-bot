// Client timing telemetry covers every kind: a session switch, a message send, a socket disconnect
// episode and the socket's per-subscription catch-ups, a page load, one tab-minute of background
// traffic. Each kind goes to the server's client-event log as one fire-and-forget POST.
// Fire-and-forget by design: never awaited, failures dropped, so the telemetry channel never
// delays a request the user waits on.
(function() {
  const CLIENT_EVENTS_URL = '/api/diag/client-events';

  // keepalive: a post made at page hide must reach the server after the page unloads.
  function postClientEvent(kind, fields) {
    fetch(CLIENT_EVENTS_URL, {
      method: 'POST',
      headers: JSON_HEADERS,
      body: JSON.stringify(Object.assign({kind, client_ts: new Date().toISOString()}, fields)),
      keepalive: true,
    }).catch(() => {});
  }

  // --- session switches ------------------------------------------------------
  // Core announces each switch phase as a page event; the record rides as-is
  // plus the bootstrap's Resource Timing. The started phase has no entry yet
  // (its response has not landed) and reports nulls, as does a cache-hit
  // switch whose bootstrap entry predates the phase lookup window.
  // The bootstrap entry must have started inside the switch's window, which opens at its
  // started phase: an earlier switch's fetch of the same session is not this switch's bootstrap.
  let switchWindowStart = 0;

  document.addEventListener('charliebot:session-switch-phase', (event) => {
    if (event.detail.phase === 'started') switchWindowStart = performance.now();
    postClientEvent('switch', Object.assign({}, event.detail, bootstrapResourceTiming(event.detail.to_session)));
  });

  function bootstrapResourceTiming(sessionId) {
    const suffix = '/api/sessions/' + sessionId + '/bootstrap';
    const entry = performance.getEntriesByType('resource')
      .filter((candidate) => candidate.name.endsWith(suffix) && candidate.startTime >= switchWindowStart)
      .pop();
    return {
      request_start: entry ? Math.round(entry.requestStart) : null,
      response_start: entry ? Math.round(entry.responseStart) : null,
      response_end: entry ? Math.round(entry.responseEnd) : null,
      transfer_size: entry ? Math.round(entry.transferSize) : null,
    };
  }

  // --- message sends ----------------------------------------------------------
  // The composer announces the click, every retry, the POST's outcome and the
  // socket echo of the same request id. One send posts when the echo lands on
  // a response that returned. A refused POST gets no echo, so its send posts
  // at once with the echo time null. A send whose POST succeeded but whose
  // echo never arrives posts nothing today, which reads the live echo p90 low:
  // its send posts at the cutoff, one TTL after the POST returned, with the
  // echo time null. A page that hides first posts its pending sends at the
  // hide — visibilitychange hidden is not a cutoff, because a hidden tab's
  // socket still delivers echoes.
  const SEND_TIMING_TTL_MS = 120000;
  const sendTimings = new Map();

  document.addEventListener('charliebot:client-send-click', (event) => {
    const now = Date.now();
    for (const [requestId, timing] of sendTimings) {
      // A send whose POST returned is owned by its cutoff timer, however late it returned.
      if (!timing.cutoff && now - timing.click > SEND_TIMING_TTL_MS) sendTimings.delete(requestId);
    }
    sendTimings.set(event.detail.request_id, {click: now, retries: 0});
  });

  document.addEventListener('charliebot:client-send-retry', (event) => {
    const timing = sendTimings.get(event.detail.request_id);
    if (timing) timing.retries++;
  });

  document.addEventListener('charliebot:client-send-response', (event) => {
    const timing = sendTimings.get(event.detail.request_id);
    if (!timing) return;
    timing.response = Date.now();
    if (event.detail.ok) {
      timing.cutoff = setTimeout(() => {
        if (sendTimings.get(event.detail.request_id) === timing) finishSend(event.detail.request_id, timing);
      }, SEND_TIMING_TTL_MS);
      if (timing.echo) finishSend(event.detail.request_id, timing);
    } else {
      finishSend(event.detail.request_id, timing);
    }
  });

  document.addEventListener('charliebot:client-send-echo', (event) => {
    const timing = sendTimings.get(event.detail.request_id);
    if (!timing) return;
    timing.echo = Date.now();
    if (timing.response) finishSend(event.detail.request_id, timing);
  });

  function finishSend(requestId, timing) {
    if (timing.cutoff) clearTimeout(timing.cutoff);
    postClientEvent('send', {
      request_id: requestId,
      click_to_response_ms: timing.response - timing.click,
      click_to_echo_ms: timing.echo ? timing.echo - timing.click : null,
      retries: timing.retries,
    });
    sendTimings.delete(requestId);
  }

  window.addEventListener('pagehide', () => {
    for (const [requestId, timing] of sendTimings) {
      if (timing.response) finishSend(requestId, timing);
    }
  });

  // --- socket disconnect episodes ---------------------------------------------
  // websocket.js announces the close (always a network close now: a switch keeps
  // the socket and rides a subscribe frame instead of closing it), the successful
  // reopen (with the reconnect attempt count) and each replay's completion (with
  // the subscription number, the session it served and the replay's bytes).
  // Each replay completion posts its own record at once, so a page whose sockets
  // stay open reports every subscription's catch-up. The reconnect episode also
  // posts as one record at its replay's catchup_complete — the disconnect, the
  // reconnect wait and the replay it took; an episode still open at the next
  // close posts with what it gathered, so a page that never reaches
  // catchup_complete still reports its disconnect.
  let socketEpisode = null;

  document.addEventListener('charliebot:client-socket-close', (event) => {
    if (socketEpisode) postSocket(socketEpisode);
    socketEpisode = {
      close_at: new Date().toISOString(),
      cause: event.detail.cause,
      close_code: event.detail.code == null ? null : event.detail.code,
      reconnect_at: null,
      reconnect_count: null,
      catchup_at: null,
      catchup_bytes: null,
    };
  });

  document.addEventListener('charliebot:client-socket-open', (event) => {
    if (!socketEpisode) return;
    socketEpisode.reconnect_at = new Date().toISOString();
    socketEpisode.reconnect_count = event.detail.attempts;
  });

  document.addEventListener('charliebot:client-socket-catchup', (event) => {
    const detail = event.detail;
    markCatchup();
    postClientEvent('socket', {
      disconnect_at: null,
      cause: null,
      close_code: null,
      reconnect_at: null,
      reconnect_count: null,
      subscription: detail.subscription,
      session_id: detail.session_id,
      catchup_at: new Date().toISOString(),
      catchup_bytes: detail.bytes,
    });
    if (!socketEpisode || socketEpisode.reconnect_at === null || detail.subscription !== 0) return;
    socketEpisode.catchup_at = new Date().toISOString();
    socketEpisode.catchup_bytes = detail.bytes;
    postSocket(socketEpisode);
    socketEpisode = null;
  });

  function postSocket(episode) {
    postClientEvent('socket', {
      disconnect_at: episode.close_at,
      cause: episode.cause,
      close_code: episode.close_code,
      reconnect_at: episode.reconnect_at,
      reconnect_count: episode.reconnect_count,
      catchup_at: episode.catchup_at,
      catchup_bytes: episode.catchup_bytes,
    });
  }

  // --- page loads --------------------------------------------------------------
  // The page reports its own Navigation Timing once, after the load event and the
  // first catch-up. The report carries the document's responseEnd, the DOMContentLoaded
  // mark and the transfer size. It also carries the cause a navigating path announced
  // through a page-load-cause event, the open case, the newest message's time and the
  // catch-up's time (both ms from navigation start), and the page's render-blocking
  // files from another origin beside the same-origin timings of the same load.
  const PAGE_LOAD_CAUSE_KEY = 'charliebot-page-load-cause';
  const PAGE_LOAD_CAUSE_TTL_MS = 60000;
  // The longest the report waits past the load event for the first catch-up.
  const PAGE_LOAD_CATCHUP_WAIT_MS = 60000;
  // The most rows each of the report's two resource lists carries; the server's model allows this many.
  const PAGE_LOAD_ROWS_MAX = 16;

  let pageLoaded = false;
  let pageLoadReported = false;
  let catchupMs = null;
  let newestMessageMs = null;
  // The page's markup-named blocking URLs, read now: the markup is parsed by the time this script runs.
  const blockingUrls = new Set(renderBlockingUrls());
  // The last Resource Timing entry of each blocking URL, kept as the entries arrive: the traffic flush clears
  // the browser's buffer every minute, and a load can finish after the first flush.
  const blockingEntries = new Map();
  watchNewestMessage();

  document.addEventListener('charliebot:page-load-cause', (event) => {
    try {
      sessionStorage.setItem(PAGE_LOAD_CAUSE_KEY, JSON.stringify({cause: event.detail.cause, at: Date.now()}));
    } catch (error) {
      console.warn('client-events: page load cause not recorded', error);
    }
  });

  window.addEventListener('load', () => {
    pageLoaded = true;
    reportIfReady();
    setTimeout(reportPageLoad, PAGE_LOAD_CATCHUP_WAIT_MS);
  });

  // The report waits for the load event and the first catch-up of the page's socket, or for the wait past
  // the load to pass, whichever comes first. A catch-up that never comes reports as null.
  function reportIfReady() {
    if (pageLoaded && catchupMs !== null) reportPageLoad();
  }

  function markCatchup() {
    if (catchupMs === null) catchupMs = performance.now();
    reportIfReady();
  }

  // The newest message of the page's session is the last message of the embedded bootstrap, and its element
  // carries its id. The time the element first reaches the DOM is the newest message on screen.
  function watchNewestMessage() {
    const messages = SESSION_BOOTSTRAP === null ? [] : SESSION_BOOTSTRAP.messages;
    if (messages.length === 0) return;
    const selector = '[data-message-id="' + CSS.escape(String(messages[messages.length - 1].id)) + '"]';
    const observer = new MutationObserver(() => {
      if (document.querySelector(selector) === null) return;
      newestMessageMs = performance.now();
      observer.disconnect();
    });
    observer.observe(document, {childList: true, subtree: true});
  }

  // The URLs the page's markup names as stylesheets and as scripts without async or defer. A script that a
  // page inserts at run time is async unless the page clears that, so it does not count.
  function renderBlockingUrls() {
    const urls = [];
    for (const element of document.querySelectorAll('link[rel~="stylesheet"], script[src]')) {
      if (element.tagName === 'SCRIPT' && (element.async || element.defer)) continue;
      urls.push(element.tagName === 'LINK' ? element.href : element.src);
    }
    return urls;
  }

  function roundOrNull(value) {
    return typeof value === 'number' ? Math.round(value) : null;
  }

  function renderBlockingRows() {
    return renderBlockingUrls()
      .filter((url) => new URL(url).origin !== location.origin)
      .slice(0, PAGE_LOAD_ROWS_MAX)
      .map((url) => {
        const entry = blockingEntries.get(url) || null;
        return {
          url,
          start_ms: entry ? roundOrNull(entry.startTime) : null,
          response_end_ms: entry ? roundOrNull(entry.responseEnd) : null,
          transfer_size: entry ? roundOrNull(entry.transferSize) : null,
          encoded_body_size: entry ? roundOrNull(entry.encodedBodySize) : null,
        };
      });
  }

  // The document first, then the same-origin render-blocking files. The readback takes the round trip from
  // each row's request-to-first-byte time and the throughput from the largest body.
  function sameOriginRows(navigation) {
    const rows = [{
      url: navigation.name,
      request_start_ms: roundOrNull(navigation.requestStart),
      response_start_ms: roundOrNull(navigation.responseStart),
      response_end_ms: roundOrNull(navigation.responseEnd),
      encoded_body_size: roundOrNull(navigation.encodedBodySize),
    }];
    for (const url of blockingUrls) {
      const entry = blockingEntries.get(url);
      if (new URL(url).origin !== location.origin || !entry) continue;
      rows.push({
        url,
        request_start_ms: roundOrNull(entry.requestStart),
        response_start_ms: roundOrNull(entry.responseStart),
        response_end_ms: roundOrNull(entry.responseEnd),
        encoded_body_size: roundOrNull(entry.encodedBodySize),
      });
    }
    return rows.slice(0, PAGE_LOAD_ROWS_MAX);
  }

  function reportPageLoad() {
    if (pageLoadReported) return;
    const navigation = performance.getEntriesByType('navigation')[0];
    if (!navigation) return;
    pageLoadReported = true;
    postClientEvent('page_load', {
      response_end_ms: Math.round(navigation.responseEnd),
      dom_content_loaded_ms: Math.round(navigation.domContentLoadedEventEnd),
      transfer_size: navigation.transferSize == null ? null : Math.round(navigation.transferSize),
      cause: consumePageLoadCause(),
      newest_message_ms: roundOrNull(newestMessageMs),
      current_ms: roundOrNull(catchupMs),
      open_case: OpenedSessions.openCase(),
      // Stored copies (task 2) are the only other source; until then a session page loads from the network.
      content_source: SESSION_ID === null ? null : 'network',
      // The service worker (task 2) makes this true; until then no page has an offline shell.
      offline_shell: false,
      render_blocking: renderBlockingRows(),
      same_origin: sameOriginRows(navigation),
    });
  }

  function consumePageLoadCause() {
    const raw = sessionStorage.getItem(PAGE_LOAD_CAUSE_KEY);
    if (!raw) return 'open';
    sessionStorage.removeItem(PAGE_LOAD_CAUSE_KEY);
    const marked = JSON.parse(raw);
    return marked && Date.now() - marked.at <= PAGE_LOAD_CAUSE_TTL_MS ? marked.cause : 'open';
  }

  // --- background traffic -------------------------------------------------------
  // The page timer registry flushes one event per tab every minute. A hidden tab
  // does no periodic work, so its flush covers the minutes it stayed hidden. Each
  // event carries the Resource Timing entries since the last flush, grouped by
  // endpoint with the session ids folded out of the path. window_s carries the
  // seconds the event's window covered, so the readback divides received bytes by
  // tab-time whatever the tab's visibility did.
  const TRAFFIC_FLUSH_MS = 60000;
  const TRAFFIC_MAX_ENDPOINTS = 40;
  const UUID_PATH = /[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/g;
  const trafficRows = new Map();
  let trafficWindowStart = Date.now();

  if (typeof PerformanceObserver === 'function') {
    new PerformanceObserver((list) => {
      for (const entry of list.getEntries()) {
        recordTrafficEntry(entry);
        if (blockingUrls.has(entry.name)) blockingEntries.set(entry.name, entry);
      }
    }).observe({type: 'resource', buffered: true});
  }

  function recordTrafficEntry(entry) {
    let endpoint;
    try {
      endpoint = new URL(entry.name, location.href).pathname.replace(UUID_PATH, '{id}');
    } catch (error) {
      console.warn('client-events: resource entry without a URL', error);
      return;
    }
    const row = trafficRows.get(endpoint) || {transfer_size: 0, count: 0};
    row.transfer_size += entry.transferSize || 0;
    row.count += 1;
    trafficRows.set(endpoint, row);
  }

  startPageTimer('diag-client-traffic', flushTraffic, TRAFFIC_FLUSH_MS);

  function flushTraffic() {
    const now = Date.now();
    const endpoints = Array.from(trafficRows, ([endpoint, row]) => Object.assign({endpoint}, row))
      .sort((a, b) => b.transfer_size - a.transfer_size)
      .slice(0, TRAFFIC_MAX_ENDPOINTS);
    postClientEvent('traffic', {
      window_s: Math.max(1, Math.round((now - trafficWindowStart) / 1000)),
      endpoints,
      visibility_state: document.visibilityState,
    });
    trafficRows.clear();
    trafficWindowStart = now;
    // The observer has counted every entry above, so the buffer can drop them. The browser's
    // resource timing buffer holds a fixed number of entries, 250 by default, and then drops new
    // ones. Those drops would blind the switch record's bootstrap lookup on a long-lived tab.
    performance.clearResourceTimings();
  }
})();
