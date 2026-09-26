# Perf baseline

Latency-perf baseline for the hourly cron in `prompts/cron/latency_perf.md`: metric definitions
with healthy ranges, seed measurements from the landing day, and the sampling history. Each round
compares its measurements against the healthy ranges here and lands at most one fix. This file
changes only through pull requests: a fix PR appends its sampling-history row, a PR that
introduces a metric with no row here adds that metric's definition row and history row in the same
PR, and a calibration-only round may open a docs-only PR of under 50 lines.

## Metric definitions

| Metric | Source | Unit | Healthy range (provisional) | Seed (2026-08-30) |
| --- | --- | --- | --- | --- |
| M1 host: load + serve CPU | `uptime`; M1 collector below | load 1/5/15; serve count; %CPU total | load < 4 (CPU count); serve CPU total < 300 % | 3.27 / 2.42 / 2.94; 4 serve processes, 237.9 % CPU |
| M2 UI polls | M2 collector below | polls/h; log MB | < 6000 polls/h | 2755 polls/h; 3.1 MB log |
| M3 API latency, 401 path | M3 collector below | seconds per request; the in-server floor sub-reading (the raw-ASGI drive of the same 401 path through the real app stack — middleware chain plus the http_request log line — the served path uvicorn runs after its lifespan installs the lean log renderer) | median < 0.005 s; in-server floor median < 0.000060 s (the line sits at the pre-fix dev-render floor — a regression to it trips; the cron-collision bias the M56 history documents applies) | median 0.002 s, max 0.002 s |
| M4 turns | M4 collector below | seconds per turn; hung sessions (an archived session is never hung — `_session_archived`'s rule; neither is a session whose running threads' own worker logs moved within the hour — a delegation's chat file goes quiet for the delegation's whole run, see the 2026-09-14 history row) | median < 600 s (recalibrated from < 300 s: the median tracks the bot's own cron-delegation workload mix, not code health — see the 2026-09-12 history row); hung = 0 | median 53 s, max 1133 s; 0 hung |
| M5 threads/list latency | M5 collector below | seconds per request, worst session | median < 0.05 s | — (introduced with its first history row) |
| M6 session usage latency | M6 collector below | seconds per request, worst session; the append-round repeat (one appended event before each timed resolution — the 3 s usage poll during a streamed turn — scratch home) | median < 0.05 s; append-round median < 0.005 s | — (introduced with its first history row) |
| M7 token-usage page | M7 collector below | seconds per page load; the changed-round collect (one corpus move since the last collect — the hourly cron's shape, scratch cache doc, live corpus read-only); the restart-cold collect (fresh process, the first page load after a server start — scratch copy of the live document, live corpus read-only; the first round after a deploy measures the one-time document-shape upgrade); the warm-gate changed round (a persistent process's warm row memo and proof gate advancing over one turn's db writes between collects — the in-server shape behind the standing changed-round reading under active turns; the standing collector re-seeds a fresh process per round, so this shape needs its own harness — scratch corpus sized to the live db's row count, live db read-only) | median < 3 s; changed-round median < 0.5 s; restart-cold median < max(0.5 s, (document + sidecar bytes) ÷ 25 MB/s) (recalibrated from < 0.5 s: that line priced the matched-signature restart — the db file+WAL signature unchanged since the document was written, the stored partial serving with the sidecar and db unread, 0.29-0.31 s at the 2026-09-15 landing — while the standing collector's copy of the live document is signature-stale whenever an opencode turn ran since the server's last token-usage collect, so under active turns the reading prices the seeded signature-miss path — sidecar parse plus, before the 2026-09-24 four-field-proof landing, the rows-map per-id key diff (~1.0 s already at the landing-day corpus) and after it the tail fetch of the moved rows; the line tracks that shape's corpus the way M78/M84/M101 track theirs, 25 MB/s ≈ 76-88 % of the measured 28.5-33.0 MB/s end-to-end floor — see the 2026-09-16 history row); warm-gate changed-round median < max(0.050 s, rows × 0.0000013 s) (introduced with the tail-fetch gate at the 221,854-row corpus: the after band reads 0.82-0.96 µs/row and the pre-fix full-key-scan shape reads 1.50-1.60 µs/row, so the line sits 1.35-1.6x over the after band and trips the fallback shape its own price; a fallback round also prints its full-scan count); quiet round (the db signature moved, no row did — the probe skip) median < 0.10 s | — (introduced with its first history row) |
| M8 sidebar search, absent needle | M8 collector below | seconds per request | median < 0.5 s | — (introduced with its first history row) |
| M9 ext-usage codex spend rescan, steady state | M9 collector below | seconds per poll round | median < 0.05 s | — (introduced with its first history row) |
| M10 thread-metadata torn reads | M10 collector below | torn reads per concurrent save stream | 0 torn reads | — (introduced with its first history row) |
| M11 backlog reads on this host | M11 collector below | HTTP status of GET /api/backlog + /api/backlog/history | both 200; 0 backlog 500s in the server log | — (introduced with its first history row) |
| M12 ext-usage codex usage scrape, steady state | M12 collector below | seconds per poll round | median < 0.05 s | — (introduced with its first history row) |
| M13 thread-events read+transform, steady state | M13 collector below | seconds per read, worst on-disk worker log | median < 0.05 s | — (introduced with its first history row) |
| M14 git diff API event-loop lag | M14 collector below | seconds of loop lag per diff/files run, charlie-bot root..HEAD | median < 0.05 s | — (introduced with its first history row) |
| M15 recap-summary cache torn reads | M15 collector below | torn reads per concurrent write stream | 0 torn reads | — (introduced with its first history row) |
| M16 trigger-file torn reads | M16 collector below | torn reads per concurrent save stream | 0 torn reads | — (introduced with its first history row) |
| M17 session fork (clone) latency | M17 collector below | seconds per fork of the heaviest real session, scratch home | median < 2 s | — (introduced with its first history row) |
| M18 hidden-tab periodic poll fetches | M18 collector below | poll fetches per simulated 10 hidden minutes | 0 fetches | — (introduced with its first history row) |
| M19 SSE framing, chunked large-frame stream | M19 collector below | seconds per 16 MB payload (16 KB chunks, ~1 MB frames), the production byte mode | median < 0.010 s | — (introduced with its first history row) |
| M20 recap extract, repeat divider | M20 collector below | seconds per extract at one divider, worst on-disk extract corpus; the cold per-divider repeat (the first extract at each unseen divider a recap scroll-back opens — events cache warm, scratch home) | median < 0.05 s; cold per-divider median < 0.02 s | — (introduced with its first history row) |
| M21 sidebar probe sweep, steady state | M21 collector below | seconds per 10th-poll sweep over all active sessions | median < 0.05 s | — (introduced with its first history row) |
| M22 ext-usage unknown-limit-shape warning stream, steady state | M22 collector below | warnings per 60 steady-state transform rounds | 0 warnings after the first sighting per process | — (introduced with its first history row) |
| M23 archive-range chat-event rescan, steady state | M23 collector below | seconds per 8-page backwards scroll over the biggest archived corpus | median < 0.005 s | — (introduced with its first history row) |
| M24 trigger list, steady state | M24 collector below | seconds per list_triggers call, worst on-disk trigger corpus | median < 0.05 s | — (introduced with its first history row) |
| M25 scheduler config reload, steady state | M25 collector below | seconds of loop lag per steady-state reload, live config corpus | median < 0.005 s | — (introduced with its first history row) |
| M26 message-projection advance per appended event | M26 collector below | seconds per `get_message_projection` advance on one appended event, worst on-disk live-events corpus | median < 0.005 s | — (introduced with its first history row) |
| M27 plans registry tolerant read, steady state | M27 collector below | seconds per `read_plans_tolerant` call, worst on-disk plans corpus | median < 0.005 s | — (introduced with its first history row) |
| M28 ndjson tail+count scan, steady state | M28 collector below | seconds per `parse_ndjson_tail` call, worst on-disk live chat file | median < 0.030 s | — (introduced with its first history row) |
| M29 session-metadata listing preamble, steady state | M29 collector below | seconds per `_load_session_metas(ACTIVE)` call, live session-dir corpus | median < 0.005 s | — (introduced with its first history row) |
| M30 live-half chat-event range rescan, steady state | M30 collector below | seconds per 8-page backwards scroll over the biggest archived session's live file; the append-round repeat (one appended line before each timed round, the page click during a streamed turn) | unchanged median < 0.005 s; append-round median < 0.005 s | — (introduced with its first history row) |
| M31 worker-finalize events-summary read, steady state | M31 collector below | seconds per `read_events_summary` call, worst on-disk worker log | median < 0.02 s | — (introduced with its first history row) |
| M32 memory-store assemble, steady state | M32 collector below | seconds per `assemble_master` call, live memory corpus | median < 0.005 s | — (introduced with its first history row) |
| M33 assistant-stream draft render, full-turn replay | M33 collector below | seconds per replay of the largest on-disk assistant draft, 200 B deltas at 40 ms virtual cadence | median < 0.1 s | — (introduced with its first history row) |
| M34 worker-events poll fetch at rendered count | M34 collector below | seconds + response bytes per events fetch, worst on-disk worker log; the re-open repeat of the full fetch (the panel re-opening an unchanged log — the cold first parse + first deflate of a fresh body is the one-time cost, reported not priced) | after=total median < 0.002 s (recalibrated from < 0.02 s: the old line sat on the TestClient/httpx harness floor the 2026-09-17 repair removed — the served path reads 0.39-0.47 ms, the vacuous-read class the M36/M59/M71 repairs called out; see the 2026-09-17 history row); empty-tail body < 200 B; full fetch repeat median < 0.002 s (recalibrated with the same repair: the served re-open reads 1.11-1.17 ms after the FastJSON+gzip-memo landing, 1.85-2.00 ms before it); full fetch body < 200 KB | — (introduced with its first history row) |
| M35 chat message-page responses, steady state | M35 collector below | seconds per request, worst projection corpus | events page median < 0.004 s (recalibrated from < 0.03 s: the old line sat on the TestClient harness floor and never saw the middleware's deflate — the repaired raw-ASGI drive reads the served path at 1.0-1.4 ms across the landing round's loads 2.4-3.0, the cron-collision bias the M56 history documents) | — (introduced with its first history row) |
| M36 worker list poll payload and handler time, steady state | M36 collector below | seconds per list request + response body bytes, worst thread-metadata corpus; the conditional repeat (?etag=) of an unchanged poll | full median < 0.002 s (recalibrated from < 0.004 s: the 2026-09-15 repair removed the TestClient/httpx harness floor — the served path read 1.3-1.6 ms across that round's loads 3.1-3.7, the cron-collision bias the M56 history documents — and the same day's gzip-memo landing reads 0.62-0.69 ms; see both 2026-09-15 history rows); full decoded body < 200 KB; conditional body 0 B (204) | — (introduced with its first history row) |
| M37 archived-session chat tail page, steady state | M37 collector below | seconds per `parse_ndjson_tail(200)` call, worst on-disk archived live file | median < 0.005 s | — (introduced with its first history row) |
| M38 session stream-broadcast fan-out, worst on-disk turn replay | M38 collector below | stream frames and wire-serialize calls/seconds per turn replay, instant feed, one subscriber | serialize total < 0.02 s; final-frame parity true | — (introduced with its first history row) |
| M39 tui/status busy check, steady state | M39 collector below | seconds of loop lag + wall per per-session busy check, live ~/.claude/projects corpus (loop lag reads the 5 ms ticker floor like M14) | loop-lag median < 0.01 s; wall median < 0.001 s | — (introduced with its first history row) |
| M40 session-list filtered listing + group reduction, steady state | M40 collector below | seconds per `list_sessions(starred=True, …)` call and per group-name reduction, live session corpus | both medians < 0.005 s | — (introduced with its first history row) |
| M41 git diff/files repeat view, steady state | M41 collector below | seconds per repeat `diff_files` call over the charlie-bot root..HEAD range | median < 0.02 s | — (introduced with its first history row) |
| M42 scheduler tick, steady state | M42 collector below | seconds of loop lag per 60 s tick with no task due, live config + session corpus (loop lag reads the 5 ms ticker floor like M14) | median < 0.01 s | — (introduced with its first history row) |
| M43 git diff/file repeat expand, steady state | M43 collector below | seconds per repeat `diff_file` call over the heaviest file of the charlie-bot root..HEAD manifest | median < 0.02 s | — (introduced with its first history row) |
| M44 scheduled-list next-run resolution, steady state | M44 collector below | seconds per `GET /api/sessions/scheduled` request, live session + cron corpus | median < 0.002 s (recalibrated from < 0.004 s: the old line sat on the TestClient/httpx harness floor the 2026-09-15 repair removed — the served path reads 0.83-0.95 ms across the repair round's loads 1.85-1.91, the cron-collision bias the M56 history documents; see that history row) | — (introduced with its first history row) |
| M45 session-WS catchup replay event-loop lag, stale-cursor reconnect | M45 collector below | seconds of loop lag + wall per `_replay_aggregated_catchup` run, worst on-disk live chat corpus, cursor 50 events behind (loop lag reads the 5 ms ticker floor like M14) | loop-lag median < 0.05 s | — (introduced with its first history row) |
| M46 cron tasks list payload and handler time, steady state | M46 collector below | seconds per request + response body bytes, live cron corpus | median < 0.02 s; body < 20 KB | — (introduced with its first history row) |
| M47 claude declared-window warning stream, steady state | M47 collector below | warnings per 60 steady-state declared-window resolutions | 0 warnings after the first sighting per process | — (introduced with its first history row) |
| M48 search content-scan missing-file debug stream, steady state | M48 collector below | debug lines per 60 steady-state content scans of an active session whose live chat file is missing | 0 lines after the first sighting per (session, error) per process | — (introduced with its first history row) |
| M49 opencode part-unhandled debug stream, steady state | M49 collector below | debug lines per 60 steady-state `_translate_part` calls of one unhandled part type | 0 lines after the first sighting per part type per process | — (introduced with its first history row) |
| M50 ext-usage credentials read warning stream, steady state | M50 collector below | warnings per 60 steady-state `_read_credentials` calls of a tokenless file | 0 warnings after the first sighting per (event, path) per broken streak | — (introduced with its first history row) |
| M51 sidebar dirty-session deep probe, post-write | M51 collector below | seconds per post-write deep probe of the worst on-disk threads corpus (scratch copy, one atomic metadata rewrite per round) | median < 0.010 s | — (introduced with its first history row) |
| M52 chat-event append, per event | M52 collector below | seconds per `save_chat_event` append of one probe event, worst on-disk live-events corpus, scratch home | median < 0.005 s (the fdatasync-durable append's flush floor is ~2.8 ms on this host's storage) | — (introduced with its first history row) |
| M53 config reload failure re-fire, broken steady state | M53 collector below | warnings + re-parses per 60 steady-state `get_config` calls of a persistently-broken config corpus | 0 warnings after the first sighting per (event, error) per process; 0 re-parses (one fingerprint stat set per call) | — (introduced with its first history row) |
| M54 stream-draft paint work, code-bearing draft, real highlight.js | M54 collector below | seconds of paint work per full-turn replay of the largest fence-bearing on-disk assistant draft, 200 B deltas at 40 ms virtual cadence, page-pinned marked + hljs 11.9.0 common builds | median < 0.2 s | — (introduced with its first history row) |
| M55 artifact compare-view serve, steady state | M55 collector below | seconds per repeat `?diff=` compare-view request over the worst on-disk artifact pair, plus the request's worst event-loop gap (the 5 ms ticker floor like M14); the cold first compare of a pair (the annotate the repeat memo serves from — the collector's first-view line) | repeat-view median < 0.003 s (recalibrated from < 0.010 s: the old line sat on the TestClient/httpx harness floor the M70/M72 repairs called out — the repaired raw-ASGI drive reads the served path at 1.0-1.3 ms across the landing round's loads 3.4-3.5, and the served path's own per-click deflate the middleware re-ran moves into a served gzip memo; see the 2026-09-16 history row); loop-lag median < 0.010 s; first-view median < 0.25 s | — (introduced with its first history row) |
| M56 sidebar status poll, steady state | M56 collector below | seconds per `GET /api/sessions/status` request over the active-session id set | median < 0.002 s (recalibrated from < 0.004 s: the old line sat on the TestClient/httpx harness floor the 2026-09-15 repair removed — the served path reads 0.48-0.50 ms across the repair round's loads 1.85-1.91; a tripped reading is read as host load first — the cron-collision bias the M56 history documents) | — (introduced with its first history row) |
| M57 plan-registry poll, steady state | M57 collector below | seconds per `GET /api/sessions/{id}/plans` request, worst on-disk plans corpus | median < 0.0020 s (recalibrated from < 0.0030 s: the old line sat on the TestClient/httpx harness floor the 2026-09-15 repair removed — the served path reads 0.8-1.7 ms across the repair round's loads 2.0-2.7, the cron-collision bias the M56 history documents; see that history row) | — (introduced with its first history row) |
| M58 per-request config read, steady state | M58 collector below | seconds per `get_config` call, live config corpus | median < 0.0001 s | — (introduced with its first history row) |
| M59 worker thread-detail poll payload and handler time, steady state | M59 collector below | seconds per request + response body bytes, worst thread-metadata corpus; the attach-mode repeat (`?attach=1`) of the unchanged poll | full-row median < 0.001 s (recalibrated from < 0.003 s: the body-keyed gzip memo removed the middleware's per-request whole-body deflate — the served path reads 0.52-0.57 ms; see the 2026-09-16 history row; the line still trips on the pre-memo 1.3-1.5 ms shape); attach-mode median < 0.001 s (recalibrated from < 0.005 s, same repair — the served attach path reads 0.44-0.45 ms), body < 300 B | — (introduced with its first history row) |
| M60 chat message-body markdown parse, repeat page render | M60 collector below | ms per 40-body page render pass over the worst on-disk live chat file (the cold first render is reported, not the metric — since the deferral PR it splits into a deferred-parse slice and a highlight-flush slice); the repeat is the session re-entry / re-render shape — every session switch rebuilds the turn engine and re-renders the same bodies | repeat median < 0.5 ms; cold first paint < 50 ms deferred-parse + the flush slice carrying the deferred highlight | — (introduced with its first history row) |
| M61 session-metadata read after TTL expiry, idle-cold | M61 collector below | ms per listing/get_session over the live corpus with every cache entry aged past `_METADATA_CACHE_TTL` (archived entries never expire; the idle cost is the non-archived set's revalidation) | bare listing median < 2 ms; single get_session median < 0.1 ms; archived-page median < 0.003 s; all-sessions median < max(0.008 s, cached-metas × 0.000008 s) (the collector's `status=None` probe copies and sorts the whole cached set — the archived share never leaves the cache, so it only grows — the way the M72 walk scales with listed entries and the M78/M84/M101/M107 lines track bytes; the production routes copy only their filtered subsets, so this line prices the full-set probe the collector runs, not a route's cost; calibrated 2026-09-17 on 4.8-5.5 µs per cached meta across the 1075→1241 growth, the margin also carrying the cold sidebar-probe term the fresh-manager shape adds) | — (introduced with its first history row) |
| M62 spawn base-resolution chain, base-less launch | M62 collector below | seconds per base-less base resolution (default branch + start point) against the real origin, quiet-remote steady state | median < 0.5 s | — (introduced with its first history row) |
| M63 session view thread payload, worst on-disk threads corpus | M63 collector below | ms per `get_session_view` handler call + response body bytes, worst thread-metadata corpus | handler median < 0.005 s; body < 300 KB | — (introduced with its first history row) |
| M65 big-page gzip event-loop stall, whole-body JSON response | M65 collector below | seconds of loop lag + wall per 200-message events-page fetch through the real app stack (gzip + auth middleware), worst on-disk live chat corpus (loop lag reads the 5 ms ticker floor like M14; a drive faster than the ticker cadence records no tick and reports its own wall — the M14 never-yields rule) | loop-lag median < 0.010 s; wall median < 0.012 s | — (introduced with its first history row) |
| M66 perfetto merged-trace build wall, worst on-disk trace corpus | M66 collector below | seconds per `merge_traces` build, largest Chrome-JSON trace under the documented trace roots (~/data, ~/scripts) | median < 8 s | — (introduced with its first history row) |
| M67 sidebar deep-probe trigger scan, steady state | M67 collector below | seconds per `pending_trigger_state_sync` call, worst on-disk trigger corpus | median < 0.00005 s | — (introduced with its first history row) |
| M68 worker-list marked changed-poll rebuild | M68 collector below | seconds per body rebuild after one writer mark, worst on-disk thread-metadata corpus; the unchanged poll and its conditional are M36's shapes | median < 0.002 s (recalibrated from < 0.003 s: the row-fragment splice removed the rebuild's whole-body re-dump — the served path reads 1.23-1.27 ms; see the 2026-09-17 history row) | — (introduced with its first history row) |
| M69 opencode SSE unhandled-event debug stream, steady state | M69 collector below | debug lines per 60 steady-state `_translate_sse_event` calls of one unhandled event type | 0 lines after the first sighting per event type per process | — (introduced with its first history row) |
| M70 artifact clean-view serve, steady state | M70 collector below | seconds per repeat credentialed view of the worst on-disk artifact page, scratch home | repeat-view median < 0.003 s (recalibrated from < 0.010 s: the old line sat on the TestClient/httpx harness floor the 2026-09-15 repair removed — the served path reads 0.7-2.7 ms across the repair round's loads 3.6-3.9, the cron-collision bias the M56 history documents; see that history row) | — (introduced with its first history row) |
| M71 sidebar search capped name-match response | M71 collector below | seconds per request, worst capped name-match shape (a one-character query matching the cap), snapshot corpus | median < 0.003 s (recalibrated from < 0.006 s: the old line sat on the TestClient harness floor the 2026-09-17 repair removed — the repaired raw-ASGI+middleware drive reads the served path at 1.66-1.89 ms with the body-keyed gzip memo, 2.99-3.13 ms before it; see the 2026-09-17 history row) | — (introduced with its first history row) |
| M72 file-browser directory listing | M72 collector below | seconds per `GET /absolute_filepath/<dir>` request, worst on-disk listing corpus (the sessions root), the served path (the production gzip middleware mounted, Accept-Encoding: gzip — the browser shape; a bare app without the middleware reads the walk's floor alone, the vacuous-read class the M70 repair called out); the changed-round rebuild (one corpus move since the stored page keyed — a metadata rename into a session dir; the harness drops the page memo per timed round, row memo warm, builder level) | repeat-view median < max(0.008 s, entries × 0.000008 s) (recalibrated from < 0.008 s: the fixed line priced the 2026-09-13 corpus of 1165 entries and the walk scales with the listed entry count — 3.7-4.5 µs/entry measured across the 1165→1296 growth, so the line tracks the corpus the way the M61 all-sessions line does; a tripped reading is still read as host load first — the cron-collision bias the M56 history documents; the 2026-09-15 repair split the TestClient harness floor out of the reading); changed-round median < max(0.007 s, entries × 0.000008 s) (recalibrated from < 0.007 s for the same growth: 4.5-5.5 µs/entry measured from the 2026-09-12 introduction corpus of 1159 entries through this round's 1296, and the fixed line sat at 94 % of a quiet-load reading before the corpus moved again) | — (introduced with its first history row) |
| M73 plan-verb validation event-loop lag | M73 collector below | seconds of loop lag + wall per amend validation (the registration gate: the DOM assertion set plus the headless-Chrome page-height render), scratch home, copied passing plan page (loop lag reads the 5 ms ticker floor like M14) | loop-lag median < 0.010 s; wall median < 0.2 s (warm steady state; the first validation after a process start pays the one-time browser launch) | — (introduced with its first history row) |
| M74 master turn-end raw-log rescan | M74 collector below | seconds of loop lag + wall per fallback-notice projection (whole read+parse+project of the turn's raw log), worst on-disk master-run raw log among the sessions the turn-end gate scans (backend option claude-family — the `_CLAUDE_RESUME_FLAG_BACKEND_TYPES` check the live call site runs), that session's own fresh translate (loop lag reads the 5 ms ticker floor like M14) | loop-lag median < 0.030 s | — (introduced with its first history row) |
| M75 live-aggregator catch-up, first streamed event | M75 collector below | seconds of loop lag + wall per first-`persist_and_broadcast` catch-up (whole read+feed of the live corpus), worst on-disk live chat corpus, scratch home (loop lag reads the 5 ms ticker floor like M14) | loop-lag median < 0.020 s | — (introduced with its first history row) |
| M76 finalize-judgment reads, warm chain | M76 collector below | seconds of loop lag + wall per judgment pair (summary-present then master-woke — the two full-history scans the finalize chain runs per worker/reviewer completion), worst on-disk live-events corpus, scratch home (loop lag reads the 5 ms ticker floor like M14) | loop-lag median < 0.010 s; wall median < 0.0005 s | — (introduced with its first history row) |
| M77 session-switch projection reuse, rotating tabs | M77 collector below | seconds per `get_message_projection` re-entry over a 12-active-session rotation (3 rounds), worst live corpora; the rebuilt count is the eviction shape (a warm re-entry is a dict read + len compare, a rebuild parses the corpus) | re-entry median < 0.5 ms; 0 rebuilt re-entries in a 12-session rotation | — (introduced with its first history row) |
| M78 ndjson event parse, cold whole-file | M78 collector below | seconds per `parse_ndjson_file` call, worst on-disk live chat file and worst on-disk worker log | chat file median < max(0.080 s, bytes ÷ 250 MB/s) (recalibrated from < 0.080 s: the worst on-disk live chat file is the 1051.3 MB runaway-turn capture whose zero-copy mmap parse floor measures 3634-3734 ms — 282-290 MB/s — on this host, so the line tracks the corpus's own floor; the 0.080 s line keeps the small-corpus watch verbatim — see the 2026-09-16 history row); worker log median < 0.050 s (recalibrated from < 0.020 s: the worst on-disk worker log now carries one 9.5 MB tool_result line whose bare orjson parse measures 35-37 ms — the funnel's floor; the 2026-09-08 range was set on the 6.7 MB / 2315-event corpus) | — (introduced with its first history row) |
| M79 git branches list, steady state | M79 collector below | seconds per `GET /api/git/branches` handler call over the charlie-bot checkout | repeat-view median < 0.010 s | — (introduced with its first history row) |
| M80 token-tally changed round under append churn | M80 collector below | seconds per changed-round collect after one 1 MB-class append to each of the two worst copied transcripts (the busy-turn shape: an active master turn appends MBs between /token-usage loads; the 40 h live log sampled 2026-09-09 shows the page's p90 at 219 ms, max 2.35 s, against a 20 ms warm median), scratch corpus + cache | median < 0.30 s (recalibrated from < 0.020 s: the charlie-bot source's 2.3 GB / 20.5k-directory corpus joined every collect's walk in #1352, and the memoized walk's one-stat-per-directory floor on this host is ~0.1 s; the 2026-09-10 landing's 0.0030-0.0048 s readings predate that source) | — (introduced with its first history row) |
| M81 chat math-walk, delimiter gate | M81 collector below | seconds of KaTeX auto-render walk per message-page re-render (the M60 corpus), per streamed math-free draft replay, and per repeat re-render (the same page rebuilt into fresh elements — the session re-entry shape the parse memo's repeat render serves); the walks the gate skips count 0 | page re-render median < 0.020 s + 0.020 s per delimiter-bearing body on the page (recalibrated from < 0.020 s: the 2026-09-09 line was set on the all-math-free corpus the 36.3 MB worst file then was — readings 0.87-1.45 ms over 40 gate-skipped bodies; the worst file is now the 1051.3 MB runaway-turn capture carrying two $-bearing bodies whose walks are the page's own math rendering, 29.98-32.65 ms at 2 walks — see the 2026-09-16 history row; the formula degrades to the old line on a math-free page, where the gate's watch survives verbatim, and the walked count the collector prints is the gate's regression watch); streamed replay walk median < 0.010 s; repeat re-render median < 0.010 s (the born-walked skip's band — the element's innerHTML reads plus the gates; a trip reads the skip falling back to the served swap or the re-walk) | — (introduced with its first history row) |
| M82 worker events-log append, per event | M82 collector below | seconds per append of one probe event to a scratch worker log, the run's held-handle shape | median < 0.0002 s | — (introduced with its first history row) |
| M83 versioned static-asset revalidation, warm page load | M83 collector below | seconds per revalidation request (If-None-Match) per asset over the dashboard's template-referenced asset set; the warm-cache revalidation-request count the page load issues | revalidate median < 0.002 s per asset; 0 revalidation requests per warm page load | — (introduced with its first history row) |
| M84 backend stream-line parse, worst on-disk raw log | M84 collector below | seconds per full replay of the raw-log tail-follow loop and the stdout-stream NDJSON funnel over the worst on-disk raw agent log (scratch copy, live home read-only) | tail-follow median < max(0.060 s, bytes ÷ 250 MB/s) (recalibrated from bytes ÷ 200 MB/s with the 2026-09-18 drain-copy removal: the drain splits its lines from a read-only mapping instead of a whole-backlog readall copy, so both funnels share the orjson+translate parse floor the 250 MB/s figure prices — the after readings sit 344-378 MB/s on the 1051.1 MB corpus — see the 2026-09-18 history row); stdout-stream median < max(0.040 s, bytes ÷ 250 MB/s) (recalibrated from the absolute 0.060/0.040 s lines: the worst on-disk raw log is now a 1050.9 MB / 150-line master-run log whose orjson+translate replay floor measures 2874-3455 ms, 304-366 MB/s, so the line tracks the corpus's own floor; the 0.060/0.040 s max() floors keep the small-corpus watch verbatim — see the 2026-09-16 history row) | — (introduced with its first history row) |
| M85 verify-finalize report read, steady state | M85 collector below | seconds per `read_verify_final_report` call, worst on-disk worker log | median < 0.005 s | — (introduced with its first history row) |
| M86 delegation takeoff-gate scan, delegation-flow shape | M86 collector below | seconds per `check_takeoff_gate` call, worst live chat corpus, one authorized user message appended; the blocked-round repeat (the corpus-as-it-stands shape — nine steady-state calls on an unchanged corpus, the parity witness) | median < 0.001 s; blocked-round median < 0.0005 s | — (introduced with its first history row) |
| M87 opencode abort client round-trip | M87 collector below | seconds per `_abort_session` call against a local stub serve (the per-turn cleanup POST over the shared outbound client — the run-start attempt client keeps its own per-attempt construction; loop lag reads the 5 ms ticker floor like M14) | wall median < 0.005 s | — (introduced with its first history row) |
| M88 perfetto direct-pass build, worst on-disk trace corpus | M88 collector below | seconds per `_build_direct_pass_gzip` build (validation parse + parallel gzip subprocess over the original bytes), largest Chrome-JSON trace under the documented trace roots (~/data, ~/scripts) | median < 3.5 s (recalibrated from < 6 s: the compress now overlaps the parse in a gzip subprocess, landing at 2.79-2.85 s on the 307.3 MB / 1,068,461-event corpus; the validation parse is the floor — 2.72 s measured standalone — and grows with the corpus) | — (introduced with its first history row) |
| M89 backend stderr pump, per chunk | M89 collector below | seconds per 8 KB chunk pumped through the stderr tee (the streamed pump shape: buffer work plus amortized log flushes) | median < 0.00005 s | — (introduced with its first history row) |
| M90 backend stdout pump, per chunk or startup line | M90 collector below | seconds per 8 KB chunk pumped through the opencode stdout pump (the streamed pump shape) and per startup line append (the run-start shape) to the covered backends' stdout.log | chunk median < 0.00003 s; line median < 0.0002 s | — (introduced with its first history row) |
| M91 worker per-event quota-scan head, streamed-turn replay | M91 collector below | seconds per `Worker._process_event` call over a full-corpus replay of the worst on-disk worker events log — per-event median, worst single event, and the replay's total wall (scratch append target, zero-subscriber broadcast) | per-event median < 0.0002 s; worst single event < 0.020 s (recalibrated from < 1.0 ms: the worst on-disk worker log now carries one 9.5 MB tool_result line whose orjson dumps + page-cache write floor measures ~13-14 ms — the funnel's floor; the 2026-09-11 range was set on the 234 KB-era corpus); replay wall median < 0.30 s | — (introduced with its first history row) |
| M92 CLI invocation startup, common-family command | M92 collector below | seconds per `charliebot` invocation's import-and-dispatch floor (`schedule-trigger --help`: fresh process, the shared `src.cli.common` chain, no server round trip); a real common-family command (delegate/plan/improve) pays the same floor plus its request | median < 0.10 s (recalibrated from < 0.40 s: the config-deferral landing's readings sit 0.044-0.047 s, ~8x under the old line the three earlier deferral rows had already been shaving toward) | — (introduced with its first history row) |
| M93 thread-detail 500s, per 24 h server log | M93 collector below | 500 responses per newest server log for `GET /api/threads/{sid}/threads/{tid}` (the workers panel's per-thread detail fetch and its 5 s `?attach=1` poll — a 500 here fails the poll continuously while the panel is open, and each failure ships a ~30-line traceback into the log) | 0 | 9 (the AttributeError 500s the 2026-09-11 cli-binary fix removed; the live server carries the fix from its next deploy on) |
| M94 projection page + stream-delta serialization, giant-tool-output corpus | M94 collector below | tail-40 page body bytes + its json.dumps wall + the projection build wall; streamed replay serialized MB + dumps wall (the live broadcast shape: one json.dumps per emitted delta) | page body median < 1 MB; streamed replay serialized median < 30 MB and dumps wall median < 0.130 s (recalibrated from < 0.060 s: the dumps wall is the same replay's own stdlib re-serialization of the deltas the serialized sub-metric counts — ~237 MB/s measured on both the landing-day and the 2026-09-15 corpora — so the serialized line's 30 MB bound implies ~127 ms, and the old line sat below that floor on the landing day's 6.3 MB corpus; the corpus's largest tool_result grew 1.11 → 13.77 MB since — see the 2026-09-15 history row) | — (introduced with its first history row) |
| M95 worker-log newest-first scans, reviewer completion + failed improve iteration | M95 collector below | seconds per reviewer-completion worker-summary scan (early stop at the first answer); seconds per failed-iteration judgment pair (the quota scan and the summary sharing one newest-first pass), worst on-disk worker log | review median < 0.001 s; judgment-pair median < 0.004 s (recalibrated from < 0.005 s / < 0.012 s: the mapped backward scan removed the walk's window reads — the pair's 4.0 ms BufferedReader.read slice on the 9.5 MB tool_result line the head filter rejects; see the 2026-09-18 history row) | — (introduced with its first history row) |
| M96 switch-bootstrap chat payload, active-session sweep | M96 collector below | body bytes per `GET /api/sessions/{id}/bootstrap` over the active-session set (the SPA switch's fetch — the live `diag_switch` telemetry carries the client-measured switch elapsed it feeds; the sidebar list's projected worker-leaf rows are excluded — a leaf's id is a thread id and no bootstrap fetch exists for it); the after-cap body carries each tool's 500-char input/output previews with their truncation markers, full text on the persisted event | median body < 0.15 MB; max body < 0.60 MB | — (introduced with its first history row) |
| M97 plan-CLI command wall, common-family verb | M97 collector below | seconds per `charliebot plan list --session <sid>` wall (fresh process: the plan chain's import+dispatch plus the live GET; a verb command — present/amend/approve/close — pays the same floor plus its POST, the server side validates) | median < 0.15 s (recalibrated from < 0.40 s: the request path's config model stack — pydantic + yaml, ~150 ms of the old 0.22-0.23 s wall — left the verb process whole; the served wall is the import floor plus the live GET, 0.088-0.091 s at the 2026-09-17 landing; see that history row) | — (introduced with its first history row) |
| M98 memory-CLI invocation wall, read verb | M98 collector below | seconds per `charliebot memory query --topic <t> --index` wall (fresh process: the memory chain's import+dispatch plus the live store read) | median < 0.30 s | — (introduced with its first history row) |
| M99 server import floor, fresh process | M99 collector below | seconds per `import server` wall (fresh process: the module uvicorn imports; the speech stack — numpy via src.agents.transcriber plus the two SIMD scanners — must stay out, loading on the provisioning thread and at the voice use sites) | median < 0.75 s | — (introduced with its first history row) |
| M100 run-start session-adopt signal, worker-log read trip and wire | M100 collector below | broadcast frames per signal; the cold read+transform raw rows and wall over a 51-signal scratch worker log (one signal per production log's head); the chat marker lines the master funnel persists (parity witness — the durable append is the stable-history projection's run-start marker, load-bearing) | 0 broadcast frames per signal; 0 raw rows; read wall median < 0.0005 s; marker persists (1 line per signal, both shapes) | — (introduced with its first history row) |
| M101 raw events download, gzip-accepted | M101 collector below | seconds of loop lag + wall per full download of the worst on-disk live chat file through the real app stack (the events viewer's fetch and its download link, the browser's Accept-Encoding: gzip shape); the first view (the cold read+compress a fresh open pays, scratch home) | loop-lag median < 0.010 s; steady-state wall median < 0.10 s; first-view wall < max(1.0 s, bytes ÷ 200 MB/s) (recalibrated from < 1.0 s: the line was set on the 36.3 MB / 5519-event corpus the 2026-09-14 landing measured — first views 741-811 ms; the worst corpus is now the 1051.3 MB runaway-turn capture whose first view is the memo's one executor hop — read + level-1 gzip of the whole corpus — at 3930-4030 ms, 261-267 MB/s end-to-end, the compress floor the same class the M84 tail-follow line tracks — see the 2026-09-16 history row; a corpus reversion re-tightens the line automatically) | — (introduced with its first history row) |
| M102 artifact-CLI command wall, wrap verb | M102 collector below | seconds per `charliebot artifact wrap <fragment> --genre plan --output <page>` wall, fresh process, scratch fragment/output (the plan page assembly the master's plan delivery runs; local only — no server round trip, no live-home write; the check verb's probe imports its registry stack inside run_probe, so a check run's wall keeps its work) | median < 0.35 s | — (introduced with its first history row) |
| M103 config-dependency resolution, remaining sync sites | M103 collector below | seconds per raw-ASGI drive of the diff viewer, the index page, and the repos listing over a scratch empty corpus (the routes' dependency-solve + render floor); 0 routes resolving config through the sync `Depends(get_config)` — enforced by the route-walk guard test, not the collector | /diff median < 0.0015 s; / median < 0.0030 s; /api/git/repos median < 0.0015 s | — (introduced with its first history row) |
| M104 backend tail-follow cursor checkpoint, per line | M104 collector below | seconds per consumed line of a scripted 2000-line stream checkpointed to a real cursor file (scratch storage, the mount's held-fd shape) | median < 0.00005 s | — (introduced with its first history row) |
| M105 binary-file transport serve, gzip-accepted | M105 collector below | seconds per served request over the worst on-disk artifact `.png` and `.pptx` (the already-compressed media the middleware must skip), plus the worst artifact `.html` page (the keep-compressing witness); the transport header each answer carries | skipped-family median < max(0.005 s, bytes ÷ 250 MB/s), transport identity (no `Content-Encoding`, wire == raw bytes); html witness median < max(0.05 s, bytes ÷ 25 MB/s), transport `Content-Encoding: gzip` | — (introduced with its first history row) |
| M106 switch-during-stream repaint | M106 collector below | ms per synchronous paint of the hide+re-show a session switch performs on a mid-stream pending draft (the largest on-disk assistant draft grown one 200 B delta per switch, the page's marked build, node vm harness — live state read-only); the painted frame's parity against a direct full-draft parse rides every reading | median < 0.005 s | — (introduced with its first history row) |
| M107 multi-trace merged-trace build wall, worst on-disk trace dir | M107 collector below | seconds per `_cached_merge` build over the worst on-disk multi-trace dir (the merged view's dir shape: one merge-pool task per trace, the single gzip run streaming each member's fragment as it completes; scratch cache home, live home read-only) | median < max(8 s, bytes ÷ 200 MB/s) (recalibrated from max(14 s, bytes ÷ 150 MB/s): the 2026-09-17 landing's wave model left the level-1 `gzip` subprocess on the ordered fragment stream — 201 MB/s against the four members' 63 MB/s write — and the stream became the wall after each wave; the isal igzip swap reads 797 MB/s, the wall is the member waves again, and 2.09 GB / 12 traces measures 7.35-8.34 s, 250-284 MB/s effective; the bytes line tracks the corpus the way M78/M84/M101 track theirs) | — (introduced with its first history row) |
| M108 claude-sub launch import+dispatch floor, fresh process | M108 collector below | seconds per `claude-sub --<unsupported-probe-flag>` wall (the subscription-mode worker binary's console script: every cc-claude subscription worker and reviewer launch pays this import floor before the claude CLI starts; the probe flag rejects after argv parse, so no launch work runs — the nonzero exit is the assert; the checkout under test rides PYTHONPATH because the venv's editable finder pins src to the main checkout) | median < 0.15 s (the residual floor is the launch chain's own asyncio plus the backends raw-log machinery — ~36 ms asyncio measured standalone; the pydantic model stacks, the web framework, and the config model stack stay out — the ban-set contract test pins it) | — (introduced with its first history row) |
| M110 remote ssh probe, warm-master steady state | M110 collector below | seconds per `ssh <host> "sacct …"` probe through `ssh_cmd` against the standing watches' SLURM login host, quiet-cluster steady state; the re-master round (the first probe after the persist window expired or the master died — the shape a create-time verify pays when the previous watch's last probe is older than the window) | warm median < 0.3 s; re-master median < 1.5 s | — (introduced with its first history row) |
| M111 review-context chat-log scan, worker completion | M111 collector below | seconds per `_first_delegation_description` scan, worst active live chat corpus carrying a delegation, deepest needle (the newest thread's completion — its match sits at the file's tail) and absent needle (a thread id no event names — the whole-corpus proof) | median < max(0.005 s, bytes ÷ 2000 MB/s) both shapes | — (introduced with its first history row) |
| M112 backup archive build, whole-home corpus | M112 collector below | seconds per `create_backup` build over the scratch synthetic-home corpus (the builder block below; fresh random ids, no `cc_session_id`), with the archive's wire bytes and ratio riding the reading; the wire sits ~16 % over the level-9 stream it replaced (36.9× vs 42.8× on this corpus — the level-1 isal trade the landing priced, not a regression) | median < max(2.0 s, corpus bytes ÷ 1200 MB/s) | — (introduced with its first history row) |
| M113 voice transcription wall, worst on-disk recording | M113 collector below | seconds per offline decode of the largest on-disk voice recording (quiet, and contended by 8 spinner processes at the turn tree's nice — the production contention shape), + decode determinism across two fresh decodes; the production walls this prices live in the server log's `http_request` duration for `POST /api/voice/*` | quiet median < audio seconds × 0.4; contended median < 2× the same round's quiet median; determinism true | — (introduced with its first history row) |
| M114 backend-launch spawn loop stall, big-heap shape | M114 collector below | seconds of event-loop stall per backend spawn through the checkout's spawn seam, both production shapes (the fork's page-table copy scales with the forking process's resident set — the collector inflates a 3.5 GB heap to the server's standing RSS class first; raw-log shape preexec-free, piped shape through the pdeathsig spawn seam's clone(CLONE_VM|CLONE_VFORK) path) | raw-log shape loop-lag median < 0.020 s (the M75 loop-lag line); piped shape < 0.020 s (re-tightened from < 0.150 s: the child-side-prctl follow-up landed — the vfork seam's clone(CLONE_VM|CLONE_VFORK) spawn skips the page-table copy the preexec fork pays, the after reading sits at the 5 ms ticker floor like the raw-log line; a regression to the thread-fork's ~110 ms shape trips it 20×) | — (introduced with its first history row) |
| M115 cold config+credentials resolution, fresh process | M115 collector below | seconds per fresh-process shared import + `get_config()` + `get_credentials()` wall (the shape a server start, a config-cache-miss verb, and every config/credentials change round pay; a cache-hit CLI verb reads only the credentials half) | median < 0.25 s | 0.148-0.155 s (branch arm, 2026-09-24 landing; main arm read 0.155-0.167 s the same round) |
| M116 ndjson whole-file parse, worst live chat file by event count | M116 collector below | seconds per `parse_ndjson_file` over the live chat file carrying the most events (the per-line plumbing's own corpus: the by-bytes worst file the M78 collector reads carries its wall in orjson's huge-line work, where a per-line cut is invisible — 507 lines across 1051 MB vs every regular session's thousands of small lines) | median < max(0.010 s, events × 0.0000060 s) (2.1x over the post-fix 2.7-2.9 µs/event measured per line, the same headroom convention the M72 walk line set — the line watches for a per-line cost class returning, not for the fix's own 8 % band) | — (introduced with its first history row) |
| M118 raw-log tail-follow grown-line round cost | M118 collector below | seconds of drain wall + worst event-loop tick gap while a backend appends to one never-closing raw-log line (the runaway-write window — the on-disk worst raw log's 2.1 GB single line is the observed instance; the writer paces slower than the drain's poll interval, so each round sees one append) | wall median < 4.0 s (the collector's own 2.0 s write pacing plus the after band's ~0.9 s drain-and-exit work; the pre-fix copy-per-round shape reads 6.2-7.8 s and trips); max tick gap median < 0.15 s (the after band's 79-90 ms is the harness's own 128 MB page-cache write; the pre-fix ~1.0 s per-round copy+rescan trips) | — (introduced with its first history row) |
| M119 sidebar root session-list serve | M119 collector below | seconds per request, worst projected-list corpus (the sidebar "All" pill fetch, `GET /api/sessions/`: every active session plus one projected worker-leaf row per legacy thread, the M71 snapshot corpus); the served shape is the identity-keyed whole-body memo (the search route's `_search_whole_body` mechanism) over the pre-dumped render (the M34 events-fetch repair's shape) with the body-keyed gzip memo — a regression to the response_model jsonable_encoder pass over every row, to a per-request re-render of an unchanged corpus, or to the middleware's whole-body deflate trips (the cron-collision bias the M56 history documents applies) | median < max(0.002 s, rows × 0.0000080 s) (the after band reads 4.8-5.8 µs/row over the projection walk plus the memo-serve render; the line sits ~1.4-1.7x over it, the same headroom convention the M72 walk line set) | — (introduced with its first history row) |
| M120 task-tree page serve, invalidated index | M120 collector below | seconds per `GET /api/sessions/tree` roots page over the live-corpus scratch copy with the tree index dropped before each timed call (any metadata write between clicks does that — the production shape the server log's per-request durations show), warm shared metadata cache, warm events caches | median < max(0.010 s, metas × 0.0000060 s) (the after band reads 2.4 µs/meta over the scandir, the shared-snapshot consult, and the facts revision; the pre-fix whole-file re-read+re-parse shape reads 41 µs/meta and trips 7x) | — (introduced with its first history row) |
| M121 task-tree index rebuild burst, invalidated | M121 collector below | seconds per 6-reader concurrent `_get_index` burst after one invalidation, warm shared metadata cache (the delegation-burst shape: one structural write invalidates, and the sidebar poll, the tree page, and the delegate's own read all arrive together — scratch home, live home read-only); the solo invalidated rebuild rides as the sub-reading (the build's own cost must not move — the parity witness), and the builds-per-burst count is the mechanism witness | burst median < max(0.010 s, solo median × 2) (the burst sits at one build's cost; the pre-fix one-build-per-reader shape reads 10.7x the solo wall at 6 readers and 64x at 12 — the amplification is the thread-pool queue plus the shared-cache lock pileup, the shape the 2026-09-25 17:10 delegation burst logged at 766-1339 ms per request); solo median < max(0.010 s, metas × 0.0000100 s) | — (introduced with its first history row) |
| M122 backend stream event discovery delay | M122 collector below | seconds from one complete NDJSON line's append to the follow loop yielding its translated event, over a live-followed scratch raw log (the per-event freshness of every streamed cc-family turn — assistant message, tool call, and result each wait one poll wake; the pipe-shaped funnels — opencode, codex stdout — have no such wait); the idle-round CPU cost rides the same reading as the trade witness | median < 0.015 s, max < 0.030 s (the after band reads 8.2-10.2 ms median, 11.2-19.3 ms max — interval/2 and interval plus parse; the pre-fix 0.15 s poll reads 73.6-78.6 ms median and trips 5x) | — (introduced with its first history row) |
| M123 hook-helper import floor, per hook event | M123 collector below | seconds per fresh-process registered hook-command wall (the collector runs the exact argv `claude_sub._write_hook_plugin` writes for the PreToolUse gate event, stdin `{}`, absent socket, `--gate` so the fail path returns rc 2 without signalling the parent group; the wall every Claude Code hook event pays before the bridge round-trip — the gate events UserPromptSubmit/PreToolUse/PermissionRequest sit on the turn's critical path) | median < 0.030 s, max < 0.030 s (the after band reads 23.2-23.5 ms median, 23.6-24.2 ms max — interpreter base, the json/re import chain, and the transport modules; the pre-fix shape pays site's editable finder for pathlib/glob/re the helper never imports plus argparse for a three-flag argv and reads 34.6-36.4 ms median with maxima to 50.4, tripping both lines) | — (introduced with its first history row) |
| M124 config-verb dispatch wall, help path | M124 collector below | seconds per fresh-process `config --help` wall (the import+dispatch floor every config-verb invocation pays before argparse prints — the M92 protocol) and per `config get <key>` round (the verb's own reading; the model stack it needs either way is why its wall is not this line's subject) | help median < 0.10 s (the M92 line's shape: the deferred-module verbs' help floors read 26-57 ms, and the pre-fix config verb reads 151-173 ms — the eager `src.core.config` import pricing the pydantic model build into discovery; the other config-importing verb modules' floors sit higher on their core modules' own eager chains — see the history row); get median < 0.25 s (the M115 line's shape — the get round is the cold config resolution plus argparse) | — (introduced with its first history row) |
| M125 sibling-verb dispatch floor, help path | M125 collector below | seconds per fresh-process `<verb> --help` wall (the M92 protocol's import+dispatch floor) for the five sibling verbs the M124 round left pinned by their core modules' eager chains (improve, publish, storage, gc-trash, remote-launch) | median < 0.10 s per verb (the M92 line's shape — the after band reads 29-45 ms across the five, the src.cli.config deferral shape's deferred-module band; the pre-fix shape reads 0.157-0.382 s, the publish/storage/trash/sequence/config chains' own eager cost) | — (introduced with its first history row) |
Note — every healthy range is provisional: a single-sample calibration from the 2026-08-30 seed
measurements against the design intent (load below the CPU count, serve CPU total well under
machine capacity, API median in the low tens of milliseconds, zero hung sessions). The serve CPU
ceiling sits at 75 % of the 4-CPU capacity because every round's reading includes its own worker
process, a constant bias. The M2 seed depends on how many browser tabs hold the dashboard open.
Any PR's Evidence section may recalibrate a range; the new value lands with that PR.

## Collector commands

The exact commands behind the seed values, run verbatim by the cron; the cron prompt does not
duplicate them — this file is their single home. The in-process commands import the code under
test from the repo's local main checkout, so every reading assumes that checkout sits at
`origin/main`; the preflight below pins it there first, and a round where the restore fails loud
reports the in-process metrics unmeasured rather than measuring a stale tree (a sibling cron can
leave the checkout on its own branch after its pull request merges — the 2026-09-17 M108 history
row is the ghost reading that produces).

Preflight — pin the default checkout at `origin/main` before the first in-process collector:

```bash
git -C /home/chaoli/workspace/charlie-bot fetch origin main || { echo "preflight: fetch failed"; exit 1; }
test -z "$(git -C /home/chaoli/workspace/charlie-bot status --porcelain)" || { echo "preflight: default checkout dirty, refusing to restore:"; git -C /home/chaoli/workspace/charlie-bot status --porcelain; exit 1; }
test "$(git -C /home/chaoli/workspace/charlie-bot rev-parse HEAD)" = "$(git -C /home/chaoli/workspace/charlie-bot rev-parse origin/main)" && echo "default checkout at origin/main" || { git -C /home/chaoli/workspace/charlie-bot switch main && git -C /home/chaoli/workspace/charlie-bot merge --ff-only origin/main; }
```

Every failure is loud and stops the round's sweep: a failed fetch exits before the compare (a
stale `origin/main` would make the compare pass while the tree sits behind), a dirty tree —
tracked or untracked, which `switch` would otherwise carry silently — exits before any mutation,
and a diverged checkout fails the `--ff-only` merge. The restore runs only on a clean tree, where
it cannot discard a sibling's work; the branch keeps its commits.

The whole-corpus scratch copies (the M35/M55/M70/M71 pair consumers and the M66/M84 builders) are removed by the block that finishes with them, on every exit path: the hourly cadence turns a skipped removal into one leaked copy per round, tmpfiles reaps /tmp only past 30 days, and the leak compounds on the root fs that holds every collector's corpus.

M1 — host load and serve CPU. The grep covers both process shapes the serving path runs: the
server's own launcher chain (`scripts/start-server.sh` → `uv run python3 server.py` wrapper →
`python3 server.py` child — the two lines the `server.py` pattern matches; the `tee` and `bash`
wrappers carry neither pattern), plus the opencode agent daemons the opencode backend family
spawns (`opencode serve`, the only shape the pre-repair grep counted — the server itself never
matched it, so a round with no live opencode session read a structurally silent zero):

```bash
uptime
ps -eo pcpu,args | grep -E '[o]pencode serve|[s]erver\.py' | awk '{n++; s+=$1} END {printf "%d serve processes, %.1f%% cpu total\n", n, s}'
```

M2 — UI poll rate and server-log size, from the newest server log (its filename carries the server
start time; polls per hour is the status-request count divided by hours since that start):

```bash
LOG=$(ls -1t /tmp/charliebot-logs/server_*.log | head -1); python3 -c '
import os, re, sys, time
log = sys.argv[1]
start = time.mktime(time.strptime(re.search(r"server_(\d{8}_\d{6})", log).group(1), "%Y%m%d_%H%M%S"))
hours = (time.time() - start) / 3600
with open(log, errors="replace") as f:
    n = sum(1 for line in f if "sessions/status" in line or "tui/status" in line)
print(f"{n} status requests over {hours:.2f} h = {n / hours:.0f} polls/h; {os.path.getsize(log) / 1e6:.1f} MB")
' "$LOG"
```

M3 — API latency: the status check, then five credential-free timed requests (the 401 path — no
auth header; the timing reads the middleware-and-framework floor of the server path):

```bash
curl -s -o /dev/null -w 'http_code=%{http_code} time_total=%{time_total}s\n' http://127.0.0.1:18498/api/sessions/status
for i in 1 2 3 4 5; do curl -s -o /dev/null -w '%{time_total}\n' http://127.0.0.1:18498/api/sessions/status; done | sort -n | awk '{a[NR]=$1} END {printf "median %.3f s, max %.3f s over %d requests\n", a[int((NR+1)/2)], a[NR], NR}'
```

M3 in-server floor — the same 401 path driven raw-ASGI through the real app stack (the middleware
chain plus the http_request log line, the served path uvicorn runs after its lifespan installs the
lean log renderer — the curl reading above is dominated by client overhead and cannot see a
server-side cut of this size). One cold pass, then the median of 1000 drives with stdout captured
so the render cost stays in the reading:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, contextlib, io, os, sys, time
sys.path.insert(0, os.environ["CHECKOUT"])
import server as srv
from src.core.log_once import ensure_lean_renderer

ensure_lean_renderer()  # the lifespan's first startup statement

def scope():
    return {"type": "http", "asgi": {"version": "3.0", "spec_version": "2.3"},
            "http_version": "1.1", "method": "GET", "scheme": "http",
            "path": "/api/sessions/status", "raw_path": b"/api/sessions/status",
            "query_string": b"", "root_path": "",
            "headers": [(b"host", b"test")],
            "client": ("t", 1), "server": ("t", 80)}

async def drive():
    out = {"status": 0}
    async def receive():
        return {"type": "http.request", "body": b"", "more_body": False}
    async def send(msg):
        if msg["type"] == "http.response.start":
            out["status"] = msg["status"]
    await srv.app(scope(), receive, send)
    return out["status"]

sink = io.StringIO()
async def main():
    for _ in range(100):
        with contextlib.redirect_stdout(sink):
            await drive()
    ts = []
    with contextlib.redirect_stdout(sink):
        for _ in range(1000):
            t0 = time.perf_counter()
            await drive()
            ts.append(time.perf_counter() - t0)
    ts.sort()
    print(f"in-server 401 floor median {ts[500] * 1e6:.2f} us, p10 {ts[100] * 1e6:.2f} us, "
          f"p90 {ts[900] * 1e6:.2f} us over 1000")

asyncio.run(main())
EOF
```

M4 — turn durations and hung sessions. The projection reads only `type` and `timestamp` from chat
events and `status` from thread and session metadata; session content is never read. A session whose
own metadata says `archived` is never hung — archiving is the user's statement that the session is
finished, the same rule the boot recovery applies (`_session_archived`), so the stale `running`
thread markers an archived session keeps do not count; the session metadata is read only when a
thread claims `running`, so the common scan pays nothing extra:

```bash
python3 - <<'EOF'
import json
import statistics
from datetime import datetime, timedelta, timezone
from pathlib import Path

def event_ts(raw):
    if not raw:
        return None
    ts = datetime.fromisoformat(raw)
    return ts if ts.tzinfo else ts.replace(tzinfo=timezone.utc)

now = datetime.now(timezone.utc)
day_ago = now - timedelta(hours=24)
hour_ago = now - timedelta(hours=1)
root = Path.home() / ".charliebot" / "sessions"
durations = []
hung = 0
malformed = 0
for session_dir in root.iterdir():
    running = False
    running_threads = []
    threads_dir = session_dir / "threads"
    if threads_dir.is_dir():
        for thread_meta in threads_dir.glob("*/metadata.json"):
            if json.loads(thread_meta.read_text()).get("status") == "running":
                running = True
                running_threads.append(thread_meta.parent)
    if running:
        # An archived session's threads are not work to resume: archiving is the
        # user's statement that the session is finished, the rule the boot
        # recovery applies (_session_archived), so its stale "running" thread
        # markers are not a hung session. Read only when a thread claims running.
        meta_path = session_dir / "metadata.json"
        if meta_path.is_file():
            try:
                if json.loads(meta_path.read_text()).get("status") == "archived":
                    running = False
            except (OSError, ValueError):
                pass
    events_path = session_dir / "data" / "chat_events.jsonl"
    if not events_path.is_file():
        continue
    recent = events_path.stat().st_mtime >= day_ago.timestamp()
    if not (running or recent):
        continue
    turn_start = None
    last_event = None
    with events_path.open(encoding="utf-8", errors="replace") as stream:
        for line in stream:
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                malformed += 1
                continue
            ts = event_ts(event.get("timestamp"))
            if ts is None:
                continue
            last_event = ts
            kind = event.get("type")
            if kind == "user":
                if turn_start is None:
                    turn_start = ts
            elif kind == "master_done" and turn_start is not None:
                if turn_start >= day_ago:
                    durations.append((ts - turn_start).total_seconds())
                turn_start = None
    if running and (last_event is None or last_event < hour_ago):
        # A delegation's chat file goes quiet for the delegation's whole run — the
        # worker appends only its own events log and the summary lands at
        # completion — so the chat mtime alone cannot separate an in-flight
        # delegation from a dead session. A running thread whose own worker log
        # moved within the window is live work, not a hang; a worker log stale
        # with the chat file is the stuck shape the tripwire exists for.
        worker_live = False
        for thread_dir in running_threads:
            worker_log = thread_dir / "data" / "events.jsonl"
            if worker_log.is_file() and worker_log.stat().st_mtime >= hour_ago.timestamp():
                worker_live = True
                break
        if not worker_live:
            hung += 1

median = statistics.median(durations) if durations else 0.0
peak = max(durations) if durations else 0.0
line = (f"{len(durations)} user->master_done turns in last 24h: "
        f"median {median:.0f}s, max {peak:.0f}s; {hung} running sessions with last event older than 1h")
if malformed:
    line += f"; {malformed} malformed event lines"
print(line)
EOF
```

M5 — threads/list latency for the session with the most thread metadata files on disk (the worst
case the 3 s workers-panel poll can hit; the key is read read-only from the host credentials):

```bash
KEY=$(awk '/^charliebot:/{f=1;next} f&&/^  access_key:/{print $2;exit}' ~/.charliebot/credentials.yaml); read SID N <<<"$(python3 -c '
from pathlib import Path
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    t = d / "threads"
    if t.is_dir():
        n = sum(1 for p in t.iterdir() if (p / "metadata.json").is_file())
        if n > best_n:
            best, best_n = d, n
print(best.name, best_n)
')"; echo "session $SID, $N threads"; for i in 1 2 3 4 5; do curl -s -o /dev/null -w '%{time_total}\n' -H "Authorization: Bearer $KEY" "http://127.0.0.1:18498/api/threads/$SID/list"; done | sort -n | awk '{a[NR]=$1} END {printf "median %.3f s, max %.3f s over %d requests\n", a[int((NR+1)/2)], a[NR], NR}'
```

M6 — session usage latency for the session with the most lines in its live `chat_events.jsonl` (the
worst case the 3 s active-session-view poll can hit while that session is thinking; only the live
file feeds resolution, archived events do not):

```bash
KEY=$(awk '/^charliebot:/{f=1;next} f&&/^  access_key:/{print $2;exit}' ~/.charliebot/credentials.yaml); read SID N <<<"$(python3 -c '
from pathlib import Path
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    p = d / "data" / "chat_events.jsonl"
    if p.is_file():
        with open(p, errors="replace") as f:
            n = sum(1 for _ in f)
        if n > best_n:
            best, best_n = d, n
print(best.name, best_n)
')"; echo "session $SID, $N chat events"; for i in 1 2 3 4 5; do curl -s -o /dev/null -w '%{time_total}\n' -H "Authorization: Bearer $KEY" "http://127.0.0.1:18498/api/sessions/$SID/usage"; done | sort -n | awk '{a[NR]=$1} END {printf "median %.3f s, max %.3f s over %d requests\n", a[int((NR+1)/2)], a[NR], NR}'
```

M6 append-round — usage resolution during a streamed turn. The standing collector above reads
the live instance, where the unchanged-list memo serves every poll; the streamed-turn shape
appends one chat event per delta, so a poll's resolution must fold only the appended suffix
into the carried fold state instead of re-scanning the whole history. A round writes state,
so the collector copies the session whose live chat file carries the most events into a
scratch `CHARLIEBOT_HOME` under /tmp (metadata.json and data/ only; live home read once for
the copy, never written), warms the fold as the view's first resolution does, then appends
one probe event before each timed resolution — the poll-during-a-streamed-turn shape —
asserting the resolved usage equals a fresh full-scan reference:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, os, shutil, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
import structlog  # the served process imports structlog at startup; without this the first timed round prices the lazy logger import, not the fold advance
from src.core import session_usage
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager

# Worst usage corpus: the session whose LIVE chat file carries the most events;
# a streamed turn appends one event per delta to exactly this file.
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    p = d / "data" / "chat_events.jsonl"
    if p.is_file():
        with open(p, errors="replace") as f:
            n = sum(1 for _ in f)
        if n > best_n:
            best, best_n = d, n
SID = best.name

# Isolation: scratch CHARLIEBOT_HOME under /tmp holding only a copy of that
# session's metadata.json and data/; live home read once for the copy, never written.
home = Path(tempfile.mkdtemp(prefix="m6-append-home-", dir="/tmp"))
dst = home / "sessions" / SID
dst.mkdir(parents=True)
shutil.copy2(best / "metadata.json", dst / "metadata.json")
shutil.copytree(best / "data", dst / "data")

cfg = CharlieBotConfig(charliebot_home=home)
mgr = SessionManager(cfg)

async def main():
    meta = await mgr.get_session(SID)
    await asyncio.to_thread(mgr._session_usage._load_and_scan, SID)  # warm the fold, as the view's first resolution does
    times = []
    parity = True
    for i in range(9):
        await mgr.save_chat_event(SID, {"id": f"m6-append-probe-{i}", "type": "assistant",
                                        "message": {"content": [{"type": "text", "text": "probe"}]},
                                        "timestamp": "2026-09-05T00:00:00Z"})
        t0 = time.perf_counter()
        usage = await mgr.resolve_session_usage(SID, meta)
        times.append(time.perf_counter() - t0)
        events = mgr.load_chat_events_sync(SID)
        fold = session_usage._UsageFold()
        fold.feed(events)
        facts = fold.facts()
        reference = (session_usage._resolve_claude_tier(facts) or session_usage._resolve_snapshot_tier(facts)
                     or (None if not events else session_usage._resolve_no_source_tier(facts)))
        parity = parity and usage == reference
    times.sort()
    print(f"{best_n}-event corpus; append-round usage resolution median {times[4] * 1000:.2f} ms, "
          f"max {times[-1] * 1000:.2f} ms over 9; parity {parity}")
    shutil.rmtree(home)

asyncio.run(main())
EOF
```

M7 — token-usage page load: five timed requests of the rendered page, each asserted 200 —
a 500 arrives as fast as a warm 200 (the collect finishes, the render raises), so a
status-blind timing reads a down page as healthy latency (the 2026-09-11 21:43 deploy-skew
outage read 5x500/hour for 8+ hours of rounds before a 500-blind sweep exposed it). The page
builds a persisted tally cache covering all three sources: per-file entries for the gigabyte-scale
Claude logs and the hundred-megabyte Codex rollouts, and one entry for the opencode db's
whole contribution signatured on the main file plus its `-wal` sidecar (a WAL-mode write
leaves the main file untouched, so the main file's stat alone can never see it). The first
load after a server start (or after bulk log churn) is a full scan, later loads re-scan
only the sources that changed, so the five-request median reads the warm steady state
(~2 s at the seed host's 2.2 GB + 190 MB log volume, measured before the Codex logs joined
the cache — hence the provisional 3 s ceiling). Evidence while the live server runs older
code is a scratch-instance
A/B: live-before against this instance, scratch-after against a scratch server on the changed
code with a scratch `CHARLIEBOT_HOME` (its own empty cache directory — cold-then-warm measures
both paths):

```bash
KEY=$(awk '/^charliebot:/{f=1;next} f&&/^  access_key:/{print $2;exit}' ~/.charliebot/credentials.yaml); for i in 1 2 3 4 5; do curl -s -o /dev/null -w '%{http_code} %{time_total}\n' -H "Authorization: Bearer $KEY" http://127.0.0.1:18498/token-usage; done | sort -k2 -n | awk '{c[$1]++; a[NR]=$2} END {if (c[200] != NR) {printf "M7 FAILED, non-200 statuses:"; for (s in c) printf " %dx%s", c[s], s; print ""; exit 1} printf "median %.3f s, max %.3f s over %d requests\n", a[int((NR+1)/2)], a[NR], NR}'
```

M7 changed-round — the collect behind a page load whose corpus moved since the last one (any
grown log moves the walk signature): the persisted document re-parses, unchanged files serve
from it, and the db row memo re-proves its rows. The standing collector's five back-to-back
requests never cross a corpus move, so the changed round needs its own timing: the harness
drops the in-process memos per round and restores a scratch copy of the live document (live
home read once for the copy, never written), keeping the row memos warm as the running
server's are:

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import shutil, sys, time
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from pathlib import Path
import src.core.token_tally as tt

CACHE = Path.home() / ".charliebot" / "cache" / "token_tally.json"
SCRATCH = Path("/tmp/opencode/m7-changed-round.json")

def changed_round():
    # The changed-round shape: the walk ran, the aggregate memo is gone, the document's
    # per-file signatures are stale for exactly the files that moved; the row memos stay
    # warm, as the long-running server's are.
    tt._aggregate_memo = None
    tt._tally_memo = None
    shutil.copy2(CACHE, SCRATCH)
    t0 = time.perf_counter()
    tally = tt.collect_token_usage(cache_path=SCRATCH)
    return time.perf_counter() - t0, tally

changed_round()  # cold pass, as at the first changed round after a server start; not timed
times = []
for _ in range(5):
    dt, tally = changed_round()
    times.append(dt)
times.sort()
print(f"changed-round collect median {times[2]:.3f} s, max {times[-1]:.3f} s over 5, "
      f"{len(tally.rows)} rows, {tally.scanned_bytes / 1e6:.1f} MB re-read")
EOF
```

M7 warm-gate changed round — the in-server shape the standing changed-round collector
cannot see: that collector re-seeds a fresh process per round, so its gate miss takes the
full key scan, while the server's row memo and proof gate are warm and the hourly page load
advances them over one turn's db writes. The harness: a scratch message-table corpus sized
to the live db's row count (read once, never written; turn rows and cache dropped on
reuse), one warm-up collect, per timed round ten in-place step-finish upserts plus twenty
appended rows each bumping time_updated above the table's max like drizzle's `$onUpdate`,
the full key scan counted (a fallback round prints its count and trips the line through its
own price), then the quiet round (the signature touched, no row moved — the probe skip) and
a cold replay carrying the rows digest as the parity witness:

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import hashlib, json, os, sqlite3, sys, time
from pathlib import Path
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
import src.core.token_tally as tt

DB = Path("/home/chaoli/.local/share/opencode/opencode.db")
SCRATCH = Path("/tmp/opencode/m7-warmgate")
CORPUS, CACHE = SCRATCH / "db.sqlite", SCRATCH / "cache.json"
live_rows = sqlite3.connect(f"file:{DB}?mode=ro", uri=True).execute(
    "select count(*) from message").fetchone()[0]

SCRATCH.mkdir(parents=True, exist_ok=True)
if not CORPUS.is_file():
    con = sqlite3.connect(CORPUS)
    con.execute("create table message (id text primary key, session_id text not null, "
                "time_created integer not null, time_updated integer not null, data text not null)")
    rows = [(f"seed-{i}", "sess", 1700000000000 + i, 1700000000000 + i,
             json.dumps({"role": "assistant" if i % 2 == 0 else "user", "modelID": "oc-m",
                         "providerID": "prov", "time": {"created": 1700000001000},
                         "pad": "y" * 120,
                         **({"tokens": {"input": 100, "output": 20, "cache": {"read": 4, "write": 2}}}
                            if i % 2 == 0 else {})}))
            for i in range(live_rows)]
    con.executemany("insert into message (id, session_id, time_created, time_updated, data) "
                    "values (?, ?, ?, ?, ?)", rows)
    con.commit()
    con.close()
con = sqlite3.connect(CORPUS)  # a reused corpus drops the last round's turn rows
con.execute("delete from message where id like 'turn%'")
con.commit()
con.close()
CACHE.unlink(missing_ok=True)
for stale in SCRATCH.glob("*.opencode_rows.json"):
    stale.unlink()

KW = dict(opencode_db=CORPUS, cache_path=CACHE, claude_homes={}, codex_homes={},
          sessions_dir=SCRATCH / "sessions")
TURN = json.dumps({"role": "assistant", "modelID": "oc-m", "providerID": "prov",
                   "time": {"created": 1700000001000},
                   "tokens": {"input": 500, "output": 50, "cache": {"read": 10, "write": 5}}})


def synthesize_turn(tag: str) -> None:
    # Ten in-place step-finish upserts plus twenty appends, every write bumping
    # time_updated above the table's max like drizzle's $onUpdate.
    con = sqlite3.connect(CORPUS)
    tu = con.execute("select max(time_updated) + 1 from message").fetchone()[0]
    for i, (mid,) in enumerate(con.execute("select id from message limit 10").fetchall()):
        con.execute("update message set data = ?, time_updated = ? where id = ?", (TURN, tu + i, mid))
    for i in range(20):
        con.execute("insert into message (id, session_id, time_created, time_updated, data) "
                    "values (?, 'sess', ?, ?, ?)", (f"{tag}-{i}", tu + 10 + i, tu + 10 + i, TURN))
    con.commit()
    con.close()


def rows_digest(tally: tt.TokenTally) -> str:
    rows = [[r.source, r.model, r.calls, r.in_fresh, r.cache_write, r.cache_read, r.output]
            for r in tally.rows]
    return hashlib.sha256(json.dumps(rows, sort_keys=True).encode()).hexdigest()[:12]


tt.collect_token_usage(**KW)  # cold pass, as at a process start; not timed

full_scans, orig_scan = 0, tt._scan_opencode_rows


def counting_scan(con, memo):
    global full_scans
    full_scans += 1
    return orig_scan(con, memo)


tt._scan_opencode_rows = counting_scan
changed, tally = [], None
for r in range(5):
    synthesize_turn(f"turn{r}")
    tt._aggregate_memo = None
    tt._tally_memo = None
    t0 = time.perf_counter()
    tally = tt.collect_token_usage(**KW)
    changed.append(time.perf_counter() - t0)
tt._scan_opencode_rows = orig_scan
changed.sort()
digest = rows_digest(tally)

quiet = []
for _ in range(5):
    os.utime(CORPUS)  # the WAL-noise shape: the signature moves, no row did
    tt._aggregate_memo = None
    tt._tally_memo = None
    t0 = time.perf_counter()
    tt.collect_token_usage(**KW)
    quiet.append(time.perf_counter() - t0)
quiet.sort()

tt._reset_aggregate_memo()
replay_digest = rows_digest(tt.collect_token_usage(**KW))
print(f"{live_rows} corpus rows; warm-gate changed round median "
      f"{changed[2] * 1000:.1f} ms, max {changed[-1] * 1000:.1f} ms over 5; quiet round "
      f"(probe skip) median {quiet[2] * 1000:.1f} ms; full key scans {full_scans}; rows "
      f"digest {digest}, replay digest {replay_digest}, parity {digest == replay_digest}")
EOF
```

M7 restart-cold — the collect behind the first page load after a server start: a fresh
process re-parses the persisted document, rebuilds the row memo from its stored rows map,
and gates the key diff on the entry's stored proof aggregates — a matching proof tuple, the
same proof the warm memo's gate takes, skips the key pass entirely, and a miss tail-fetches
only the rows written after the stored max (a legacy two-field document — a build before the
maxes were persisted, what the live server writes until its next deploy — seeds without a
max and its first miss takes the full key diff) (live home read
once for the copy, never written):

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import hashlib, json, shutil, sys, time
from pathlib import Path
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
import src.core.token_tally as tt

CACHE = Path.home() / ".charliebot" / "cache" / "token_tally.json"
SCRATCH = Path("/tmp/opencode/m7-restart-cold.json")
shutil.copy2(CACHE, SCRATCH)  # live home read once for the copy, never written
SIDECAR = CACHE.parent / tt._rows_sidecar_name(tt.DEFAULT_OPENCODE_DB)
corpus_mb = (CACHE.stat().st_size + SIDECAR.stat().st_size) / 1e6
t0 = time.perf_counter()
tally = tt.collect_token_usage(cache_path=SCRATCH)
wall = time.perf_counter() - t0
rows = [[r.source, r.model, r.calls, r.in_fresh, r.cache_write, r.cache_read, r.output] for r in tally.rows]
digest = hashlib.sha256(json.dumps(rows, sort_keys=True).encode()).hexdigest()[:12]
print(f"restart-cold collect wall {wall:.3f} s, {len(tally.rows)} rows, "
      f"scanned {tally.scanned_bytes / 1e6:.1f} MB, document+sidecar {corpus_mb:.1f} MB, rows digest {digest}")
EOF
```

M8 — sidebar search latency, worst case: five timed requests for a needle absent from every
active session's live chat file, so each request scans the whole searchable corpus (active
sessions' `chat_events.jsonl`) without an early hit. Evidence while the live server runs older
code is a scratch-instance A/B on the search corpus (all session metadata plus active sessions'
live chat files), the same shape as the M7 protocol.

```bash
KEY=$(awk '/^charliebot:/{f=1;next} f&&/^  access_key:/{print $2;exit}' ~/.charliebot/credentials.yaml); for i in 1 2 3 4 5; do curl -s -o /dev/null -w '%{time_total}\n' -H "Authorization: Bearer $KEY" "http://127.0.0.1:18498/api/sessions/search?q=zzq9xneverpresentneedle77"; done | sort -n | awk '{a[NR]=$1} END {printf "median %.3f s, max %.3f s over %d requests\n", a[int((NR+1)/2)], a[NR], NR}'
```

M9 — ext-usage poller codex spend rescan, steady state. The poller's cost is background work invisible to HTTP probes, so the collector times the function the poller calls every round: the provider's spend computation over the live corpus (read-only), from the main repo checkout. The collector reports the no-churn steady state; a round where a rollout file changed re-parses just that file, and one cold full-corpus pass runs per server process start:

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import sys, time
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from pathlib import Path
from src.api.ext_usage import CodexUsageProvider, _list_rollout_files

prov = CodexUsageProvider("main", str(Path.home() / ".codex"))
rollouts = _list_rollout_files(prov.sessions_dir)
prov._compute_spend(rollouts)  # cold pass, as at a server restart; not timed
times = []
for _ in range(5):
    t0 = time.perf_counter()
    prov._compute_spend(rollouts)
    times.append(time.perf_counter() - t0)
times.sort()
print(f"{len(rollouts)} rollout files; steady-state spend rescan median {times[2]:.4f} s, max {times[-1]:.4f} s")
EOF
```

M10 — thread-metadata torn reads: the invariant behind the threads/list endpoint's writes.
`list_threads` validates every thread's `metadata.json` from an executor thread with no
coordination against `save_metadata`'s rewrite, so a save that publishes the file
truncated lets a concurrent poll observe a half-written file; that read fails
`ThreadMetadata` validation and 500s the whole list response (the same failure the
3 s workers-panel poll hits). The collector cannot drive that race through HTTP on the
live instance without writing to its state, so it reproduces the race against the main
checkout's code with scratch state: 3000 `save_metadata` calls on one thread's file
while four reader threads validate every read. A torn read under this stream is a read
the fixed write path cannot produce; the count is the metric.

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, sys, tempfile, threading
from pathlib import Path
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest, ThreadMetadata
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager

WRITES = 3000

async def main():
    work = Path(tempfile.mkdtemp(prefix="m10-torn-read-"))
    # A session's default backend resolves from backends.options (empty by
    # default since the sectioned config), so the scratch config carries one.
    cfg = CharlieBotConfig(charliebot_home=work / "home",
                           backends={"options": [{"id": "m10", "label": "M10", "type": "cc-claude",
                                                  "model": "claude-opus-4-6"}]})
    sessions = SessionManager(cfg)
    threads = ThreadManager(cfg)
    session = await sessions.create_session(CreateSessionRequest(name="M10"))
    meta = await threads.create_thread(session, "m10")
    path = work / "home" / "sessions" / session.id / "threads" / meta.id / "metadata.json"
    torn = 0
    reads = 0
    stop = False

    async def writer():
        for _ in range(WRITES):
            await threads.save_metadata(meta)

    def reader():
        nonlocal torn, reads
        while not stop:
            raw = path.read_text(encoding="utf-8")
            reads += 1
            try:
                ThreadMetadata.model_validate_json(raw)
            except ValueError:
                torn += 1

    running = [threading.Thread(target=reader) for _ in range(4)]
    for t in running:
        t.start()
    await writer()
    stop = True
    for t in running:
        t.join()
    print(f"{WRITES} save_metadata calls, {reads} concurrent reads; torn reads observed: {torn}")

asyncio.run(main())
EOF
```

M11 — backlog read endpoints' status codes. This host configures no
`ui.backlog_repos`, so both GETs read the empty-state path, and every 500 arrives
with a ~30-line ASGI traceback in the server log beside its structured
`http_request … status=500` line (the line the 5xx count below matches — the
server switched from uvicorn's `"GET … HTTP/1.1" 500` access-log shape to these
structured lines, which the count must follow). The count appends `|| [ $? -eq 1 ]`
because `grep -c` exits 1 on the healthy zero count: a real grep failure (exit 2)
stays loud, a zero count exits clean. Evidence while the live server runs older
code is a scratch-instance
A/B (scratch `CHARLIEBOT_HOME`, TestClient against before and after code), the
same shape as the M7 protocol.

```bash
KEY=$(awk '/^charliebot:/{f=1;next} f&&/^  access_key:/{print $2;exit}' ~/.charliebot/credentials.yaml); for p in /api/backlog /api/backlog/history; do curl -s -o /dev/null -w "$p %{http_code}\n" -H "Authorization: Bearer $KEY" "http://127.0.0.1:18498$p"; done
LOG=$(ls -1t /tmp/charliebot-logs/server_*.log | head -1); grep -c "path=/api/backlog status=500" "$LOG" || [ $? -eq 1 ]
```

M12 — ext-usage poller codex usage scrape, steady state. The scrape reads each account's
newest rollout every round for the latest token_count event; the collector times the function
the poller calls each round over the live corpus (read-only), from the main repo checkout. An
unchanged file is the steady state; a changed file re-reads only its tail window (the
full-file read remains for a token_count farther back than the window):

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import sys, time
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from pathlib import Path
from src.api.ext_usage import CodexUsageProvider, _list_rollout_files

prov = CodexUsageProvider("main", str(Path.home() / ".codex"))
rollouts = _list_rollout_files(prov.sessions_dir)
prov._fetch_usage(rollouts)  # cold pass, as at a server restart; not timed
times = []
for _ in range(5):
    t0 = time.perf_counter()
    prov._fetch_usage(rollouts)
    times.append(time.perf_counter() - t0)
times.sort()
print(f"{len(rollouts)} rollout files; steady-state usage scrape median {times[2]:.4f} s, max {times[-1]:.4f} s")
EOF
```

M13 — thread-events read+transform, steady state. The workers panel polls
`GET /api/threads/{sid}/threads/{tid}/events` every 5 s for each expanded
running worker, and the endpoint projects the worker's whole events log on
every call. The cost is invisible to HTTP probes of the standing metrics, so
the collector times the function the handler calls over the largest on-disk
worker log (read-only), from the main repo checkout. An unchanged log is the
steady state (one stat); a log appended between calls re-parses only its new
tail; a shrunk log restarts its entry from scratch. Evidence while the live
server runs older code is a scratch-instance A/B, the same shape as the M7
protocol:

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import sys, time
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from pathlib import Path
from src.api.threads import read_thread_worker_events

root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for p in root.glob("*/threads/*/data/events.jsonl"):
    n = p.stat().st_size
    if n > best_n:
        best, best_n = p, n
read_thread_worker_events(best)  # cold pass, as at first panel open; not timed
times = []
for _ in range(5):
    t0 = time.perf_counter()
    read_thread_worker_events(best)
    times.append(time.perf_counter() - t0)
times.sort()
print(f"{best_n / 1e6:.1f} MB thread log; steady-state read+transform median {times[2]:.4f} s, max {times[-1]:.4f} s")
EOF
```

M14 — git diff API event-loop lag. The diff endpoints run `git` subprocesses whose duration
is user data (up to SUBPROCESS_GIT_DIFF_TIMEOUT per call, several calls per request); run
inline in the async handler, that time is an event-loop freeze for every concurrent request
and WebSocket, invisible to the standing probes above. The collector drives the `diff_files`
handler over the charlie-bot checkout's full history (root commit .. HEAD — the largest
range this host's workspace carries) with a concurrent 5 ms ticker, and reports the worst
ticker gap per run; when the handler never yields, every gap is the handler's whole wall
time. Evidence while the live server runs older code points the same collector at the
branch checkout, the same shape as the M7 protocol (both runs are read-only against live
state: the scratch CHARLIEBOT_HOME is a tempfile):

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, subprocess, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from src.core.config import CharlieBotConfig
from src.api.git import diff_files

REPO = Path("/home/chaoli/workspace/charlie-bot")
BASE = subprocess.run(["git", "rev-list", "--max-parents=0", "HEAD"],
                      cwd=REPO, capture_output=True, text=True, check=True).stdout.splitlines()[0]
cfg = CharlieBotConfig(charliebot_home=Path(tempfile.mkdtemp(prefix="m14-home-")),
                       paths={"workspace_dirs": ["/home/chaoli/workspace"]})

async def run_once():
    gaps = []
    stop = False
    async def ticker():
        prev = time.perf_counter()
        while not stop:
            await asyncio.sleep(0.005)
            now = time.perf_counter()
            gaps.append(now - prev)
            prev = now
    t = asyncio.create_task(ticker())
    t0 = time.perf_counter()
    result = await diff_files(repo=str(REPO), base=BASE, head="HEAD", mode="three-dot", cfg=cfg)
    wall = time.perf_counter() - t0
    stop = True
    await t
    return (max(gaps) if gaps else wall), result["total_files"]

async def main():
    await run_once()  # cold pass, as at first diff after a server start; not timed
    worst = []
    total = 0
    for _ in range(5):
        gap, total = await run_once()
        worst.append(gap)
    worst.sort()
    print(f"{total} files in root..HEAD diff; loop-lag median {worst[2]:.4f} s, max {worst[-1]:.4f} s")

asyncio.run(main())
EOF
```

M15 — recap-summary cache torn reads: the invariant behind the recap view's
cache writes. `get_session_recap` reads `recap_summaries.json` from an executor
thread with no coordination against `_write_cache_entry`'s rewrite from the
summarize handler, so a write that publishes the file truncated lets a
concurrent read observe a half-written file; that read fails json parsing and
500s the recap response. The collector reproduces the race against the main
checkout's code with scratch state: 3000 `_write_cache_entry` calls on one
session's cache file while four reader threads run the handler's lookup. A torn
read under this stream is a read the fixed write path cannot produce; the count
is the metric. The reader loop yields every 8 reads: a memo-hit lookup is
shorter than the interpreter's 5 ms GIL switch request, so a yield-free reader
starves the writer of the GIL indefinitely and the stream never finishes. The
yield keeps the write stream moving with the readers still running throughout.
Evidence while the live server runs older code points the same
collector at the branch checkout (`sys.path.insert` at the worktree root).

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, sys, tempfile, threading, time
from pathlib import Path
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest
from src.core.sessions import SessionManager
from src.core import recap

WRITES = 3000

async def setup():
    work = Path(tempfile.mkdtemp(prefix="m15-torn-read-"))
    # A session's default backend resolves from backends.options (empty by
    # default since the sectioned config), so the scratch config carries one.
    cfg = CharlieBotConfig(charliebot_home=work / "home",
                           backends={"options": [{"id": "m15", "label": "M15", "type": "cc-claude",
                                                  "model": "claude-opus-4-6"}]})
    sessions = SessionManager(cfg)
    session = await sessions.create_session(CreateSessionRequest(name="M15"))
    return work, sessions, session

work, sessions, session = asyncio.run(setup())
recap._write_cache_entry(sessions, session.id, 0, "seed")

state = {"torn": 0, "reads": 0, "stop": False, "errors": []}

def writer():
    for i in range(WRITES):
        recap._write_cache_entry(sessions, session.id, i % 100, f"summary {i}")

def reader():
    while not state["stop"]:
        state["reads"] += 1
        try:
            recap.lookup_cached_summary(sessions, session.id, 0)
        except ValueError:
            state["torn"] += 1
        except BaseException as e:
            state["errors"].append(repr(e))
            return
        # The memo-hit lookup releases the GIL for only a few microseconds, so
        # the switch request never fires and the writer starves without a yield.
        if state["reads"] % 8 == 0:
            time.sleep(0)

running = [threading.Thread(target=reader) for _ in range(4)]
for t in running:
    t.start()
writer()
state["stop"] = True
for t in running:
    t.join()
if state["errors"]:
    raise SystemExit(f"unexpected reader errors: {state['errors'][:3]}")
print(f"{WRITES} _write_cache_entry calls, {state['reads']} concurrent reads; torn reads observed: {state['torn']}")
EOF
```

M16 — trigger-file torn reads: the invariant behind the trigger files the polls read.
`_save_trigger` rewrites a trigger's JSON on schedule/cancel/fire while
`list_triggers` (the 3 s workers-panel poll, the session view) and the sidebar
probe (`pending_trigger_state_sync`) read it from executor threads with no
coordination; a save that publishes the file truncated lets a concurrent read
observe a half-written file, fail JSON parsing, and drop the trigger from that
poll's list and pending count. The collector reproduces the race against the
checkout's code with scratch state: 3000 `_save_trigger` calls on one trigger's
file while four reader threads run the read path (`read_text` +
`_migrate_legacy_watch_pids`). A torn read under this stream is a read the
fixed write path cannot produce; the count is the metric. Evidence while the
live server runs older code points the same collector at the branch checkout
(`sys.path.insert` at the worktree root).

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, sys, tempfile, threading
from datetime import UTC, datetime, timedelta
from pathlib import Path
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from src.core.config import CharlieBotConfig
from src.core.models import PendingTrigger
from src.core.sessions import SessionManager
from src.core.triggers import TriggerManager, _migrate_legacy_watch_pids

WRITES = 3000

work = Path(tempfile.mkdtemp(prefix="m16-torn-read-"))
cfg = CharlieBotConfig(charliebot_home=work / "home")
sessions = SessionManager(cfg)
triggers = TriggerManager(cfg, sessions)
trigger = PendingTrigger(
    session_id="m16",
    fire_at=datetime.now(UTC) + timedelta(hours=1),
    message="m16 torn-read probe",
    watch_targets=[],
)
path = cfg.sessions_dir / "m16" / "triggers" / f"{trigger.id}.json"

state = {"torn": 0, "reads": 0, "stop": False, "errors": []}

async def writer():
    for _ in range(WRITES):
        await triggers._save_trigger(trigger)

def reader():
    while not state["stop"]:
        raw = path.read_text(encoding="utf-8")
        state["reads"] += 1
        try:
            _migrate_legacy_watch_pids(raw)
        except ValueError:
            state["torn"] += 1
        except BaseException as e:
            state["errors"].append(repr(e))
            return

async def main():
    await triggers._save_trigger(trigger)
    running = [threading.Thread(target=reader) for _ in range(4)]
    for t in running:
        t.start()
    await writer()
    state["stop"] = True
    for t in running:
        t.join()

asyncio.run(main())
if state["errors"]:
    raise SystemExit(f"unexpected reader errors: {state['errors'][:3]}")
print(f"{WRITES} _save_trigger calls, {state['reads']} concurrent reads; torn reads observed: {state['torn']}")
EOF
```

M17 — session fork (clone) latency: the endpoint behind `POST /api/sessions/{id}/fork`,
`SessionManager.fork_session`, parses the parent's archived and live chat events and
re-serializes them into the child's `data/parent_reference.jsonl`, appends the clone event,
and copies plan artifacts file by file, so the wall time scales with the parent's chat-event
bytes while the `threads/` payload — often the bulk of a session's directory size — is never
read. A fork writes state, so the live instance cannot be probed read-only; the collector
resolves the session with the heaviest fork corpus (live chat events plus archives), copies
only that session into a scratch `CHARLIEBOT_HOME` under /tmp, and times `fork_session` from
the main checkout, one cold pass then five timed forks, each child removed as soon as its
wall is taken (a resident child carries a corpus-sized ``parent_reference.jsonl`` whose
pending writeback throttles the next fork's reference write into the kernel's dirty-page
path — the reading then tracks host IO state, not the fork). Evidence while the live server runs
older code points the same collector at the branch checkout (`sys.path.insert` at the
worktree root), the same shape as the M15 protocol.

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, shutil, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager

# Heaviest fork corpus: live chat events plus archives, the bytes fork_session parses
# and re-serializes into the child's parent_reference.jsonl.
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    data = d / "data"
    corpus = [data / "chat_events.jsonl", *sorted((data / "archives").glob("chat_events.*.jsonl"))]
    n = sum(p.stat().st_size for p in corpus if p.is_file())
    if n > best_n:
        best, best_n = d, n
SID = best.name
print(f"heaviest fork corpus: session {SID}, {best_n / 1e6:.1f} MB chat events")

# Isolation: scratch CHARLIEBOT_HOME under /tmp holding only a copy of that session; live home read once for the copy, never written.
home = Path(tempfile.mkdtemp(prefix="m17-fork-home-", dir="/tmp"))
dst = home / "sessions" / SID
dst.parent.mkdir(parents=True)
shutil.copytree(best, dst)

cfg = CharlieBotConfig(charliebot_home=home)
sessions = SessionManager(cfg)

async def main():
    # Each fork leaves a corpus-sized parent_reference.jsonl; the child is freed
    # as soon as its wall is taken because accumulated gigabytes of dirty page
    # cache throttle the next fork's reference write (the same ramp five plain
    # 1 GB writes into one directory ride) — without the free, the five
    # back-to-back forks of the 1051.3 MB corpus read 6.3-10.1 s where the
    # freed shape reads 0.5-0.7 s on an idle disk.
    child = await sessions.fork_session(SID)  # cold pass, as at first fork after a server start; not timed
    shutil.rmtree(home / "sessions" / child.id)
    times = []
    for _ in range(5):
        t0 = time.perf_counter()
        child = await sessions.fork_session(SID)
        times.append(time.perf_counter() - t0)
        shutil.rmtree(home / "sessions" / child.id)
    times.sort()
    n = sessions.get_chat_event_count_sync(SID)
    print(f"{n} parent events; fork median {times[2]:.4f} s, max {times[-1]:.4f} s over 5 runs")

asyncio.run(main())
shutil.rmtree(home)
EOF
```

M18 — hidden-tab periodic poll fetches: the invariant behind page-timers.js
("a hidden tab does no periodic work at all"). A poll routined around the
registry keeps fetching while the tab is hidden (browser throttling slows but
never stops a raw interval). The collector loads the checkout's real
page-timers.js, usage.js, sidebar/namespace.js, sidebar/session-view.js and
ext_usage.js — the module chain the page loads them in — in a node vm with a
stub document,
holds the page hidden, starts one worker transcript's poll (`setWorkerTranscriptMode`;
the worker transcript lives in the main chat column since the worker-card panel's
removal) plus the ext-usage strip's DOMContentLoaded init, and fires every
registered interval the number of times its cadence fits a simulated 10 minutes.
Fetches issued after bootstrap settle are the metric; healthy is 0. The closing
10 s visible re-check must fetch again (a poll that never resumes is a finding,
not a pass). Evidence while the live server runs older code points the same
collector at the branch checkout (`CHECKOUT` at the worktree root):

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} node - <<'EOF'
'use strict';
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const CHECKOUT = process.env.CHECKOUT || '/home/chaoli/workspace/charlie-bot';
const read = (name) => fs.readFileSync(path.join(CHECKOUT, 'web/static/js', name), 'utf8');

const listeners = new Map();
const elements = new Map([
  // hideStreaming() resolves both ids unconditionally on every transcript poll
  // whose response carries no pending draft.
  ['streaming-msg', {classList: {contains: () => false, add() {}, remove() {}, toggle() {}}}],
  ['streaming-content', {innerHTML: '', classList: {contains: () => false, add() {}, remove() {}, toggle() {}}}],
]);
const documentStub = {
  hidden: true,
  addEventListener(type, fn) {
    if (!listeners.has(type)) listeners.set(type, []);
    listeners.get(type).push(fn);
  },
  removeEventListener() {},
  dispatch(type) {
    (listeners.get(type) || []).forEach((fn) => fn());
  },
  getElementById: (id) => elements.get(id) || null,
  querySelector: () => null,
  querySelectorAll: () => [],
  createElement: () => ({style: {}, classList: {add() {}, remove() {}, toggle() {}}, setAttribute() {}}),
  body: {appendChild() {}, style: {}},
};

const intervals = new Map();
let nextId = 1;
let fetches = 0;
const context = {
  document: documentStub,
  SESSION_ID: 'session-a',
  URLSearchParams,
  setTimeout,
  clearTimeout,
  setInterval(fn, ms) {
    const id = nextId++;
    intervals.set(id, {id, fn, ms});
    return id;
  },
  clearInterval(id) {
    intervals.delete(id);
  },
  console: {error() {}, warn() {}, log() {}},
  fetch: async (url) => {
    fetches += 1;
    // The transcript poll's applyTranscriptUpdate advances its counters and
    // revision from this shape; the 'messages' container id resolves null in
    // the stub document, so no render path is reachable whatever the body.
    return {ok: true, json: async () => ({messages: [], total: 0, revision: '', active_run_id: null})};
  },
  localStorage: {getItem: () => null, setItem() {}, removeItem() {}},
};
vm.createContext(context);
for (const f of ['page-timers.js', 'usage.js', 'sidebar/namespace.js', 'sidebar/session-view.js', 'ext_usage.js']) {
  vm.runInContext(read(f), context, {filename: f});
}

async function tickWindow(seconds) {
  for (const entry of Array.from(intervals.values())) {
    const n = Math.floor((seconds * 1000) / entry.ms);
    for (let i = 0; i < n; i++) await entry.fn();
  }
}

(async () => {
  documentStub.dispatch('DOMContentLoaded');
  await new Promise((r) => setImmediate(r));
  context.setWorkerTranscriptMode({sessionId: 'session-a'});
  await new Promise((r) => setImmediate(r));
  const bootstrapFetches = fetches;

  fetches = 0;
  await tickWindow(600);
  const hiddenFetches = fetches;

  documentStub.hidden = false;
  documentStub.dispatch('visibilitychange');
  fetches = 0;
  await tickWindow(10);
  const visibleFetches = fetches;

  console.log(
    `checkout ${CHECKOUT}: ${hiddenFetches} poll fetches per simulated 10 hidden min ` +
    `(${bootstrapFetches} bootstrap fetch excluded); ${visibleFetches} fetches in the 10 s visible re-check`
  );
})().catch((err) => {
  console.error(err);
  process.exit(1);
});
EOF
```

M19 — SSE line framing of a chunked large-frame stream. `iter_sse_lines` frames
both opencode master/worker event streams and the anthropic-proxy upstream, and
its terminator-search cost is invisible to the HTTP probes above: a frame spans
as many byte chunks as the network delivers, so when the search re-scans the
accumulated remainder on every chunk, framing costs O(bytes × chunks) — seconds
of blocked event-loop time per multi-MB tool payload at network chunk sizes —
while a resumable search stays O(bytes). The production consumers read the
byte mode (`lines_as_bytes=True`), whose lines skip the chunk decode the
pre-byte framer paid for their JSON parsers (orjson parses the wire's UTF-8
bytes natively); the collector streams a
16 MB payload of ~1 MB frames (terminators at frame ends only) through the
adapter at a fixed 16 KB chunking against the checkout's code, synthetic and
read-only, in the byte mode the serve path runs. Evidence points the same
collector at the before and after checkouts (`sys.path.insert` at each root),
the same shape as the M7 protocol:

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, sys, time
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from src.core.sse import iter_sse_lines

class _Stub:
  def __init__(self, chunks):
    self._chunks = chunks
  async def aiter_bytes(self):
    for c in self._chunks:
      yield c

frame = b"data: " + b"x" * 1000000 + b"\n\n"
payload = frame * 16
chunks = [payload[i:i + 16 * 1024] for i in range(0, len(payload), 16 * 1024)]

async def run_once():
  lines = 0
  t0 = time.perf_counter()
  async for _ in iter_sse_lines(_Stub(chunks), lines_as_bytes=True):
    lines += 1
  return time.perf_counter() - t0, lines

async def main():
  await run_once()  # cold pass, as at a first big frame after a server start; not timed
  times = []
  lines = 0
  for _ in range(5):
    dt, lines = await run_once()
    times.append(dt)
  times.sort()
  print(f"{len(payload) / 1e6:.1f} MB payload in 16 KB chunks, ~1 MB frames, {lines} lines; "
        f"framing median {times[2]:.4f} s, max {times[-1]:.4f} s")

asyncio.run(main())
EOF
```

M20 — recap extract repeats at one divider. `GET /api/sessions/{id}/recap?upto=…`
runs `extract_recap`, which parses and projects every event below the divider; the
chat UI re-requests an open recap panel on every re-materialization (virtualized
scrolling, turn re-renders), so each repeat pays a full corpus scan for an
unchanged result and the cost is invisible to the standing HTTP probes. The
collector copies the worst on-disk extract corpus (the session whose live chat
file plus archives carry the most bytes — the corpus extraction parses;
metadata.json plus data/ only) into a scratch `CHARLIEBOT_HOME` under /tmp and
times `extract_recap` from the checkout, one cold pass then five timed repeats at
the same divider — the re-materialization pattern, identical results which the
memo serves without a re-scan. Evidence while the live server runs older code
points the same collector at the branch checkout (`sys.path.insert` at the
worktree root), the same shape as the M15 protocol:

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, hashlib, json, shutil, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager
from src.core import recap

# Worst extract corpus: the session whose chat events (live file plus archives)
# carry the most bytes; extract_recap parses every event below the divider.
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    data = d / "data"
    corpus = [data / "chat_events.jsonl", *sorted((data / "archives").glob("chat_events.*.jsonl"))]
    n = sum(p.stat().st_size for p in corpus if p.is_file())
    if n > best_n:
        best, best_n = d, n
SID = best.name
print(f"worst extract corpus: session {SID}, {best_n / 1e6:.1f} MB chat events")

# Isolation: scratch CHARLIEBOT_HOME under /tmp holding only a copy of that
# session's metadata.json and data/; live home read once for the copy, never written.
home = Path(tempfile.mkdtemp(prefix="m20-recap-home-", dir="/tmp"))
dst = home / "sessions" / SID
dst.mkdir(parents=True)
shutil.copy2(best / "metadata.json", dst / "metadata.json")
shutil.copytree(best / "data", dst / "data")

cfg = CharlieBotConfig(charliebot_home=home)
mgr = SessionManager(cfg)
count = mgr.get_chat_event_count_sync(SID)
upto = count - 1

recap.extract_recap(mgr, SID, upto)  # cold pass, as at first divider open; not timed
times = []
result = None
for _ in range(5):
    t0 = time.perf_counter()
    result = recap.extract_recap(mgr, SID, upto)
    times.append(time.perf_counter() - t0)
times.sort()
digest = hashlib.sha256(json.dumps(result, sort_keys=True).encode()).hexdigest()[:12]
print(f"{count} events, {len(result['asks'])} asks, digest {digest}; "
      f"repeat-divider extract median {times[2]:.4f} s, max {times[-1]:.4f} s")
shutil.rmtree(home)
EOF
```

M20 cold per-divider — the first extract at each unseen divider, the shape a recap
scroll-back produces. The standing collector's five back-to-back repeats never leave
one divider, so the per-divider cold cost needs its own timing: the harness copies the
worst extract corpus into a scratch `CHARLIEBOT_HOME` under /tmp (live home read once
for the copy, never written), warms the events cache as the viewed session's polls do,
then times the first extract at six unseen dividers (0.70-0.95 of the corpus) — each
divider a distinct memo key, so every timed round is a genuine cold extract:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import os, shutil, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager
from src.core import recap

# Worst extract corpus: the session whose chat events (live file plus archives)
# carry the most bytes; the recap's per-divider extract projects events[0:end].
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    data = d / "data"
    corpus = [data / "chat_events.jsonl", *sorted((data / "archives").glob("chat_events.*.jsonl"))]
    n = sum(p.stat().st_size for p in corpus if p.is_file())
    if n > best_n:
        best, best_n = d, n
SID = best.name

# Isolation: scratch CHARLIEBOT_HOME under /tmp holding only a copy of that
# session's metadata.json and data/; live home read once for the copy, never written.
home = Path(tempfile.mkdtemp(prefix="m20cold-home-", dir="/tmp"))
dst = home / "sessions" / SID
dst.mkdir(parents=True)
shutil.copy2(best / "metadata.json", dst / "metadata.json")
shutil.copytree(best / "data", dst / "data")

cfg = CharlieBotConfig(charliebot_home=home)
mgr = SessionManager(cfg)
count = mgr.get_chat_event_count_sync(SID)
mgr.load_chat_events_sync(SID)  # warm the events cache, as the viewed session's polls do

DIVIDERS = [int(count * f) for f in (0.95, 0.9, 0.85, 0.8, 0.75, 0.7)]
times = []
for end in DIVIDERS:
    t0 = time.perf_counter()
    result = recap.extract_recap(mgr, SID, end)
    times.append(time.perf_counter() - t0)
times.sort()
print(f"checkout {os.environ['CHECKOUT'].rsplit('/', 1)[-1]}: {count} events, 6 unseen dividers "
      f"(0.70-0.95 of corpus), events cache warm; cold per-divider extract median {times[3]*1000:.1f} ms, "
      f"max {times[-1]*1000:.1f} ms, asks {len(result['asks'])}")
shutil.rmtree(home)
EOF
```

M21 — sidebar 10th-poll probe sweep, steady state. Every 10th `/api/sessions/status`
poll re-selects every active session for a sidebar re-probe (the self-heal window for
a writer that forgot its dirty mark). The pre-fix sweep deep-probed all of them — a
scandir+stat plus a read+parse of every 30-day-window thread metadata, every trigger
file, and every plans.json — where the fixed sweep pays one stat-only signature pass
per session and deep-probes only sessions whose probe inputs changed (the escape
hatch `/status?force=1` still deep-probes everything). The every-10th sweep
runs detached from the poll's request (single-flight; its results land for the
polls that follow it), so the cost is background work invisible to HTTP
probes, and the collector times the sweep function the poll schedules
(read-only over the live state), with the poll's on-loop signature storage
replayed between sweeps. The steady state is no probe input changed between sweeps; the cold
pass, as at a server start with no signatures stored, is a full deep probe and is not
timed. The pre-fix number used in the landing PR's evidence is the same sweep-shaped
command against `probe_sidebar_state_sync` unconditionally (the poll's pre-fix call).

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, sys, time
from pathlib import Path
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from src.core import sidebar_state
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager, selective_probe_sidebar_state

async def main():
    cfg = CharlieBotConfig(charliebot_home=Path.home() / ".charliebot")
    mgr = SessionManager(cfg)
    metas = await asyncio.to_thread(mgr.list_active_session_metas)
    specs = [
        (m.id, mgr._threads_dir(m.id), mgr._session_dir(m.id) / "triggers", mgr._session_dir(m.id) / "plans.json")
        for m in metas
    ]
    sidebar_state.reset_for_tests()

    def sweep():
        entries, sigs = selective_probe_sidebar_state(specs, deep=False)
        for sid, sig in sigs.items():
            sidebar_state.store_probe_signature(sid, sig)  # the poll's on-loop storage half
        return entries

    sweep()  # cold pass, as at a server start with no signatures stored; not timed
    times = []
    for _ in range(5):
        t0 = time.perf_counter()
        sweep()
        times.append(time.perf_counter() - t0)
    times.sort()
    print(f"{len(specs)} active sessions; steady-state sidebar sweep median {times[2]:.4f} s, max {times[-1]:.4f} s")

asyncio.run(main())
EOF
```

M22 — ext-usage unknown-limit-shape warning stream, steady state. Every account
fetch re-runs the response transform (`_transform_response` with
`_scoped_windows`, `_transform_codex_response` with `_codex_windows`), and each
unrecognized utilization-bearing field or unmappable scoped entry logs
`ext_usage_unknown_limit_shape`; a field the upstream keeps sending re-fires the
same warning every fetch round forever — 1536 lines in the 20.47 h live server
log sampled 2026-09-01 (~75/h, 20 % of the log's structured lines) — while one
sighting per process carries the whole signal. The cost is background log volume
invisible to HTTP probes, so the collector drives the pure transform directly
over a synthetic response shaped like the observed one (two unknown top-level
utilization fields plus one scoped entry missing its percent): one first-sighting
call, as at a process start, then 60 steady-state repeat rounds, counting the
event. A warning in the repeat window is a re-fired alarm; the count is the
metric. Evidence while the live server runs older code points the same collector
at the branch checkout (`sys.path.insert` at the worktree root), the same shape
as the M7 protocol:

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import sys
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from src.api import ext_usage as ext_usage_mod

raw = {
    "fiveHour": {"utilization": 11.0, "resetsAt": "2026-08-04T12:00:00+00:00"},
    "sevenDay": {"utilization": 22.0, "resetsAt": "2026-08-04T19:00:00+00:00"},
    "nimbus_quill": {"utilization": 3.0, "resetsAt": "2026-08-04T20:00:00+00:00"},
    "extra_usage": {"utilization": 5.0, "resets_at": "2026-08-05T00:00:00+00:00"},
    "limits": [
        {"kind": "weekly_scoped", "group": "weekly", "resets_at": "",
         "scope": {"model": {"display_name": "Nimbus"}}},
    ],
}

warns = []
orig = ext_usage_mod.log.warning
ext_usage_mod.log.warning = lambda event, **kw: warns.append({"event": event, **kw})
try:
    ext_usage_mod._transform_response(raw, account="main")  # first sighting, as at a process start; not counted
    warns.clear()
    for _ in range(60):  # steady-state repeat rounds of the poll's re-transform
        ext_usage_mod._transform_response(raw, account="main")
finally:
    ext_usage_mod.log.warning = orig
n = sum(1 for w in warns if w["event"] == "ext_usage_unknown_limit_shape")
print(f"60 steady-state transform rounds; ext_usage_unknown_limit_shape warnings: {n}")
EOF
```

M23 — archive-range chat-event rescan, steady state. Sessions with
`archive_offset > 0` paginate backwards through `load_chat_events_range`,
whose archive half re-read and re-scanned every archive file below the page
end on every page click (and on every cold recap extract touching archives).
The fixed reader memoizes each archive file's parsed events on
(mtime_ns, size): a repeat range read over unchanged archives pays one stat
per file and zero corpus bytes, mirroring the live events cache that
unarchived sessions already serve from. Archive files are append-only within
their week and frozen after, so the key is sound; a same-week recycle append
re-parses only that file. The cost is invisible to HTTP probes of archived
sessions (deep page turns only), so the collector copies the session whose
`data/archives` carries the most bytes into a scratch `CHARLIEBOT_HOME`
under /tmp (metadata.json and data/ only; live home read once for the copy,
never written), warms the metadata cache as the live server's polls do, and
times 8-page backwards scrolls of 200 events below the archive end — one
cold pass, as at first scroll after a server start with an empty memo, then
five timed repeats. Evidence while the live server runs older code points
the same collector at the branch checkout (`sys.path.insert` at the worktree
root), the same shape as the M15 protocol:

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, shutil, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager

# Worst archived corpus: the session whose data/archives carries the most bytes.
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    arch = d / "data" / "archives"
    if arch.is_dir():
        n = sum(p.stat().st_size for p in arch.glob("chat_events.*.jsonl"))
        if n > best_n:
            best, best_n = d, n
SID = best.name
print(f"worst archived corpus: session {SID}, {best_n / 1e6:.1f} MB in archives")

# Isolation: scratch CHARLIEBOT_HOME under /tmp holding only a copy of that
# session's metadata.json and data/; live home read once for the copy, never written.
home = Path(tempfile.mkdtemp(prefix="m23-arch-home-", dir="/tmp"))
dst = home / "sessions" / SID
dst.mkdir(parents=True)
shutil.copy2(best / "metadata.json", dst / "metadata.json")
shutil.copytree(best / "data", dst / "data")

cfg = CharlieBotConfig(charliebot_home=home)
mgr = SessionManager(cfg)
asyncio.run(mgr.get_session(SID))  # warm the metadata cache, as the live server's polls do
offset = mgr._chat_events.read_archive_offset_sync(SID)

def scroll():
    before = offset
    for _ in range(8):
        if before <= 0:
            break
        mgr.load_chat_events_range(SID, max(0, before - 200), before)
        before -= 200

scroll()  # cold pass, as at first scroll after a server start; not timed
times = []
for _ in range(5):
    t0 = time.perf_counter()
    scroll()
    times.append(time.perf_counter() - t0)
times.sort()
print(f"archive_offset {offset}; 8-page scroll steady-state median {times[2]:.4f} s, max {times[-1]:.4f} s")
shutil.rmtree(home)
EOF
```

M24 — trigger list, steady state. The 3 s workers-panel poll
(`GET /api/threads/{sid}/list`) and the session view render call
`TriggerManager.list_triggers`, whose pre-fix form read and parsed every
trigger file of the session on every call. The cost is invisible to the
standing HTTP probes (panels poll the sessions they have open, not the worst
corpus), so the collector resolves the session whose triggers directory
carries the most files and times the manager function the poll awaits
(read-only over the live state), from the main repo checkout: one cold pass,
as at a server start with an empty memo, then five timed calls. The fixed
reader memoizes each trigger file's parsed record on (mtime_ns, size): the
steady state is one scandir and zero corpus bytes, and a `_save_trigger`
rewrite (schedule/cancel/fire) re-reads only that file. Trigger files change
only through that atomic rewrite, so the key is sound. Evidence while the
live server runs older code points the same collector at the branch checkout
(`sys.path.insert` at the worktree root), the same shape as the M7 protocol:

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, sys, time
from pathlib import Path
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager
from src.core.triggers import TriggerManager

# Worst trigger corpus: the session whose triggers directory carries the most files.
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.glob("*/triggers"):
    n = sum(1 for p in d.glob("*.json") if p.is_file())
    if n > best_n:
        best, best_n = d, n
SID = best.parent.name

async def main():
    cfg = CharlieBotConfig(charliebot_home=Path.home() / ".charliebot")
    triggers = TriggerManager(cfg, SessionManager(cfg))
    await triggers.list_triggers(SID)  # cold pass, as at a server start; not timed
    times = []
    result = None
    for _ in range(5):
        t0 = time.perf_counter()
        result = await triggers.list_triggers(SID)
        times.append(time.perf_counter() - t0)
    times.sort()
    print(f"session {SID}, {len(result)} triggers (worst corpus {best_n} files); "
          f"steady-state list_triggers median {times[2]:.4f} s, max {times[-1]:.4f} s")

asyncio.run(main())
EOF
```

M25 — scheduler config reload, steady state. Every 60 s scheduler tick re-reads the
config so edited cron tasks take effect without a restart; the pre-fix form ran a full
YAML parse and model validation inline on the event loop on every tick, while the fixed
form routes through the process-wide fingerprint-cached `get_config` — one stat-key
comparison when nothing changed, a real reload only on a fingerprint change (which still
lands within one tick, the same freshness the per-tick read guaranteed). Ticks are
background work invisible to HTTP probes, so the collector drives
`Scheduler._reload_config` over the live config corpus (read-only: a parse, never a
write) with a concurrent 5 ms ticker, from the checkout under test: one cold pass, as
at a server start, then five timed steady-state reloads (a reload faster than the
ticker cadence reports its own wall time). Evidence while the live server runs older
code points the same collector at the branch checkout (`CHECKOUT` at the worktree
root), the same shape as the M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, os, sys, time
from unittest.mock import AsyncMock
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.config import get_config
from src.core.scheduler import Scheduler

async def main():
    sched = Scheduler(get_config(), AsyncMock())
    sched._reload_config()  # cold pass, as at a server start; not timed
    worst = []
    for _ in range(5):
        gaps = []
        stop = False
        async def ticker():
            prev = time.perf_counter()
            while not stop:
                await asyncio.sleep(0.005)
                now = time.perf_counter()
                gaps.append(now - prev)
                prev = now
        t = asyncio.create_task(ticker())
        t0 = time.perf_counter()
        sched._reload_config()
        wall = time.perf_counter() - t0
        stop = True
        await t
        worst.append(max(gaps) if gaps else wall)
    worst.sort()
    print(f"steady-state scheduler config reload loop-lag median {worst[2]:.4f} s, max {worst[-1]:.4f} s")

asyncio.run(main())
EOF
```

M26 — message-projection advance per appended event. The chat bootstrap
(`GET /api/sessions/{id}/bootstrap`, session view, SPA switches) and the
events pagination endpoint serve from the per-session message projection;
every one of those reads on a session whose live chat file grew since the
last read re-derived the projection from scratch — a full re-aggregation of
every event, ~59 ms on the 20534-event worst live corpus, per SPA switch or
page click on an appending session. The fixed projection is append-
incremental: `stable_closed_prefix_len` splits the raw stream at the last
point no open OpenCode run interval crosses, the closed prefix feeds the
aggregator exactly once, and the still-open tail is re-evaluated per call
through a cloned aggregator, so an advance costs O(open tail) instead of
O(history) while the served view still equals `events_to_view(all_events)`.
The split rule is sound because the stable-history projection defers queued
users only within one completed run interval; a shrinking live file (rewind)
still pays a full rebuild. The cost is a per-interaction latency no standing
probe sees, so the collector copies the session with the most live chat
events into a scratch `CHARLIEBOT_HOME` under /tmp (metadata.json and data/
only; live home read once for the copy, never written), builds the
projection once (cold pass, as at first view after a server start; not
timed), then appends one event at a time and times the per-append advance,
checking the served view against the whole-list reference digest. Evidence
while the live server runs older code points the same collector at the
branch checkout (`CHECKOUT` at the worktree root), the same shape as the
M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, hashlib, json, os, shutil, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager
from src.api.message_utils import events_to_messages

# Worst projection corpus: the session whose LIVE chat file carries the most
# events (archive_offset > 0 sessions take the legacy path, never the projection).
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    p = d / "data" / "chat_events.jsonl"
    if p.is_file():
        with open(p, errors="replace") as f:
            n = sum(1 for _ in f)
        if n > best_n:
            best, best_n = d, n
SID = best.name
print(f"worst projection corpus: session {SID}, {best_n} live chat events")

# Isolation: scratch CHARLIEBOT_HOME under /tmp holding only a copy of that
# session's metadata.json and data/; live home read once for the copy, never written.
home = Path(tempfile.mkdtemp(prefix="m26-proj-home-", dir="/tmp"))
dst = home / "sessions" / SID
dst.mkdir(parents=True)
shutil.copy2(best / "metadata.json", dst / "metadata.json")
shutil.copytree(best / "data", dst / "data")

cfg = CharlieBotConfig(charliebot_home=home)
mgr = SessionManager(cfg)

async def main():
    mgr.get_message_projection(SID)  # cold pass, as at first view after a server start; not timed
    times = []
    for i in range(8):
        await mgr.save_chat_event(SID, {
            "id": f"m26-probe-{i}", "type": "assistant",
            "message": {"content": [{"type": "text", "text": f"probe chunk {i}"}]},
            "timestamp": f"2026-09-01T19:00:{i:02d}Z",
        })
        t0 = time.perf_counter()
        projection = mgr.get_message_projection(SID)
        times.append(time.perf_counter() - t0)
    times.sort()
    all_events = mgr.load_chat_events_sync(SID)
    def digest(msgs):
        ident = [(m.get("id"), m.get("role"), len(m.get("content", "") or ""), m.get("event_index")) for m in msgs]
        return hashlib.sha256(json.dumps(ident).encode()).hexdigest()[:12]
    ref = events_to_messages(all_events)
    match = digest(projection.history) == digest(ref)
    print(f"8 single-event appends on {best_n}-event corpus; projection advance "
          f"median {times[4] * 1000:.2f} ms, max {times[-1] * 1000:.2f} ms; parity {match} (digest {digest(projection.history)})")

asyncio.run(main())
shutil.rmtree(home)
EOF
```

M27 — plans registry tolerant read, steady state. The plan panel's poll
endpoint (`GET /api/sessions/{id}/plans`) and the sidebar deep probe both run
`read_plans_tolerant`, whose pre-fix form read and parsed the session's
plans.json and re-derived every plan's state on every call. The cost is
invisible to the standing HTTP probes (the panel polls the session it has
open, not the worst corpus), so the collector times the function over the
session whose plans.json carries the most bytes (read-only over the live
state), from the checkout under test: one cold pass, as at a server start
with an empty memo, then five timed calls. The fixed reader memoizes each
plans.json's projected result on (mtime_ns, size): the steady state is one
exists+stat pair and zero registry bytes, and a verb's rewrite re-reads only
that file. Registry writes go through `write_json_atomically` and the derived
state is a pure function of the file content, so the key is sound. Evidence
while the live server runs older code points the same collector at the branch
checkout (`CHECKOUT` at the worktree root), the same shape as the M7
protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import os, sys, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.plans import read_plans_tolerant

# Worst plans corpus: the session whose plans.json carries the most bytes.
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    p = d / "plans.json"
    if p.is_file():
        n = p.stat().st_size
        if n > best_n:
            best, best_n = p, n
print(f"worst plans corpus: session {best.parent.name}, {best_n / 1e3:.1f} KB plans.json")

result = read_plans_tolerant(best, best.parent.name)  # cold pass, as at a server start with an empty memo; not timed
times = []
for _ in range(5):
    t0 = time.perf_counter()
    result = read_plans_tolerant(best, best.parent.name)
    times.append(time.perf_counter() - t0)
times.sort()
print(f"{len(result['plans'])} plans, {len(result['errors'])} errors; steady-state tolerant read "
      f"median {times[2] * 1e6:.1f} us, max {times[-1] * 1e6:.1f} us")
EOF
```

M28 — ndjson tail+count scan, steady state. The chat paging paths for sessions
without a message projection (``archive_offset > 0`` — bootstrap, view, and events
pages) call ``parse_ndjson_tail`` on every request, and its count half must scan
the whole live file to report ``total_line_count`` (``count_ndjson_lines`` is the
same scan for the recap default divider and the session GET). The pre-fix form
walked the file line by line in Python — and even ``bytes.count`` measures only
~0.7 GB/s on this host — while the fixed form counts newlines per 1 MiB chunk
through a numpy SIMD compare (~3.4 GB/s measured), keeping the file-iteration
count contract (an unterminated final line counts) that the tail result's global
ordinal math depends on. The cost is per page view, invisible to the standing
HTTP probes, so the collector times both functions over the largest live chat
file on disk (read-only), from the checkout under test: one cold pass, as at a
first page view after a server start, then seven timed calls. Evidence while the
live server runs older code points the same collector at the branch checkout
(``CHECKOUT`` at the worktree root), the same shape as the M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import os, sys, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.ndjson import parse_ndjson_tail, count_ndjson_lines

# Worst tail+count corpus: the session whose LIVE chat file carries the most bytes.
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    p = d / "data" / "chat_events.jsonl"
    if p.is_file():
        n = p.stat().st_size
        if n > best_n:
            best, best_n = p, n

parse_ndjson_tail(best, 200)  # cold pass, as at a first page view after a server start; not timed
times = []
for _ in range(7):
    t0 = time.perf_counter()
    events, total, has_more = parse_ndjson_tail(best, 200)
    times.append(time.perf_counter() - t0)
times.sort()
ctimes = []
for _ in range(7):
    t0 = time.perf_counter()
    n = count_ndjson_lines(best)
    ctimes.append(time.perf_counter() - t0)
ctimes.sort()
print(f"{best_n / 1e6:.1f} MB file, {total} lines, tail events {len(events)}; "
      f"parse_ndjson_tail median {times[3] * 1000:.2f} ms, max {times[-1] * 1000:.2f} ms; "
      f"count_ndjson_lines median {ctimes[3] * 1000:.2f} ms")
EOF
```

M29 — session-metadata listing preamble, steady state. The status poll
(`GET /api/sessions/status`), the sessions list, the archived pages, and search
all route through `_load_session_metas`, whose first step lists the session
directories; this host's sessions root holds ~1000 dirs, so the pre-fix
`Path.iterdir()` + `is_dir()` form rebuilt a Path per entry and paid one stat()
each (~6 ms measured), while the fixed `os.scandir` pass answers `is_dir()` from
the directory record itself (~1 ms), itself memoized on the root's own
(mtime_ns, size) — the mtime moves exactly when a session entry is created or
removed — so a repeat listing with an unchanged root pays one stat (~5 µs,
signature taken before the scan so a racing create/delete re-scans next call).
The cost is a slice of every listing call
and stays invisible to the standing HTTP probes (a poll's total keeps its own
budget), so the collector times the manager function the listings await
(read-only over the live state), from the checkout under test: one cold pass,
as at a server start with an empty metadata cache, then nine timed calls. The
steady state is every metadata cache entry fresh (TTL-fresh or archived), so a
call pays the dir scan plus cache lookups and zero file reads. Evidence while
the live server runs older code points the same collector at the branch
checkout (`CHECKOUT` at the worktree root), the same shape as the M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, os, sys, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.config import CharlieBotConfig
from src.core.models import SessionStatus
from src.core.sessions import SessionManager

async def main():
    cfg = CharlieBotConfig(charliebot_home=Path.home() / ".charliebot")
    mgr = SessionManager(cfg)
    await mgr._load_session_metas()  # cold pass, as at a server start with an empty metadata cache; not timed
    times = []
    n = 0
    for _ in range(9):
        t0 = time.perf_counter()
        metas = await mgr._load_session_metas(SessionStatus.ACTIVE)
        times.append(time.perf_counter() - t0)
        n = len(metas)
    times.sort()
    print(f"{n} active sessions in listing; steady-state _load_session_metas "
          f"median {times[4]*1000:.2f} ms, max {times[-1]*1000:.2f} ms")

asyncio.run(main())
EOF
```

M30 — live-half chat-event range rescan, steady state. Archived sessions
(``archive_offset > 0``) paginate backwards through ``load_chat_events_range``,
whose live half re-read the live file from byte 0 on every page click (and on
every cold recap extract touching the live tail) while the archive half already
served repeats from the M23 memo. The fixed reader memoizes the live file's
per-physical-line parsed events on (mtime_ns, size) for archived sessions —
the range index domain is physical lines (blank and malformed lines consume an
index, mirroring ``parse_ndjson_range``) — so a repeat scroll over an unchanged
live file pays one stat per page and zero corpus bytes; an append re-parses
once. The memo is gated to archived sessions: unarchived ones paginate through
the M26 message projection, so their range callers never reuse a moving live
corpus. The cost is invisible to HTTP probes of archived sessions (deep page
turns only), so the collector copies the archived session whose live
``chat_events.jsonl`` carries the most bytes into a scratch
``CHARLIEBOT_HOME`` under /tmp (metadata.json and data/ only; live home read
once for the copy, never written), warms the metadata cache as the live
server's polls do, and times 8-page backwards scrolls of 200 events over the
live half — one cold pass, as at first scroll after a server start with an
empty memo, then five timed repeats. Evidence while the live server runs older
code points the same collector at the branch checkout (``CHECKOUT`` at the
worktree root), the same shape as the M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, json, os, shutil, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager

# Worst live-half range corpus: the archived session whose LIVE chat file
# carries the most bytes; its live-half range reads re-parse per page click.
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    meta_p = d / "metadata.json"
    if not meta_p.is_file():
        continue
    try:
        off = json.loads(meta_p.read_text()).get("archive_offset", 0)
    except Exception:
        continue
    live = d / "data" / "chat_events.jsonl"
    if off and off > 0 and live.is_file():
        n = live.stat().st_size
        if n > best_n:
            best, best_n = d, n
SID = best.name
print(f"worst live-half range corpus: session {SID}, live {best_n / 1e6:.1f} MB")

# Isolation: scratch CHARLIEBOT_HOME under /tmp holding only a copy of that
# session's metadata.json and data/; live home read once for the copy, never written.
home = Path(tempfile.mkdtemp(prefix="m30-live-half-home-", dir="/tmp"))
dst = home / "sessions" / SID
dst.mkdir(parents=True)
shutil.copy2(best / "metadata.json", dst / "metadata.json")
shutil.copytree(best / "data", dst / "data")

cfg = CharlieBotConfig(charliebot_home=home)
mgr = SessionManager(cfg)
asyncio.run(mgr.get_session(SID))  # warm the metadata cache, as the live server's polls do
offset = mgr._chat_events.read_archive_offset_sync(SID)
total = mgr.get_chat_event_count_sync(SID)
live_path = home / "sessions" / SID / "data" / "chat_events.jsonl"

def scroll():
    before = total
    for _ in range(8):
        if before <= offset:
            break
        mgr.load_chat_events_range(SID, max(offset, before - 200), before)
        before -= 200

scroll()  # cold pass, as at first scroll after a server start with an empty memo; not timed
times = []
for _ in range(5):
    t0 = time.perf_counter()
    scroll()
    times.append(time.perf_counter() - t0)
times.sort()
scroll()
atimes = []
for i in range(8):
    with open(live_path, "a", encoding="utf-8") as f:  # the scratch copy, never the live home
        f.write(json.dumps({"id": f"m30-append-probe-{i}", "type": "assistant",
                            "message": {"content": [{"type": "text", "text": "probe"}]},
                            "timestamp": "2026-09-04T19:00:00Z"}) + "\n")
    t0 = time.perf_counter()
    scroll()
    atimes.append(time.perf_counter() - t0)
atimes.sort()
print(f"archive_offset {offset}, total {total}; 8-page live-half scroll steady-state "
      f"median {times[2]:.4f} s, max {times[-1]:.4f} s; append-round median {atimes[3]:.4f} s, "
      f"max {atimes[-1]:.4f} s over 8")
shutil.rmtree(home)
EOF
```

M31 — worker-finalize events-summary read, steady state. Every worker completion
runs `read_events_summary` on the finalize path (and again on the
reviewer-completion path for the original worker's log), quoting the log's last
parseable events into the worker_summary bubble. The pre-fix reader full-parsed
the whole events.jsonl (~41 ms measured on the 6.7 MB worst on-disk log) and
sliced the last 80; the fixed reader walks 512 KiB segments from the end
collecting the last 80 parseable events — identical output, blank and malformed
lines never counting toward the budget in either form. The cost is thread-pool
time invisible to HTTP probes, so the collector times the function the finalize
path awaits over the largest on-disk worker log (read-only), from the checkout
under test: one cold pass, as at first finalize after a server start, then five
timed calls. Evidence while the live server runs older code points the same
collector at the branch checkout (`sys.path.insert` at the worktree root), the
same shape as the M7 protocol. The sibling review-context scan (the reviewer
prompt's delegation lookup over the session chat log) moved from a full parse
to stream-until-first-match in the same change; its position-dependent numbers
ride along in the PR's Evidence section instead of carrying a standing row.

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, sys, time
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from pathlib import Path
from src.core.config import CharlieBotConfig
from src.core.threads import ThreadManager
from src.core.spawner_events import read_events_summary

root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for p in root.glob("*/threads/*/data/events.jsonl"):
    n = p.stat().st_size
    if n > best_n:
        best, best_n = p, n
SID, TID = best.parts[-5], best.parts[-3]

async def main():
    cfg = CharlieBotConfig(charliebot_home=Path.home() / ".charliebot")
    thread_mgr = ThreadManager(cfg)
    result = await read_events_summary(SID, TID, thread_mgr)  # cold pass, as at first finalize after a server start; not timed
    times = []
    for _ in range(5):
        t0 = time.perf_counter()
        result = await read_events_summary(SID, TID, thread_mgr)
        times.append(time.perf_counter() - t0)
    times.sort()
    print(f"{best_n / 1e6:.1f} MB worker log, session {SID} thread {TID}; "
          f"steady-state events-summary read median {times[2]:.4f} s, max {times[-1]:.4f} s")

asyncio.run(main())
EOF
```

M32 — memory-store assemble, steady state. The master run builds the
instruction block on every user message (``master_cc_run`` via
``_build_instructions_content``), and every worker spawn calls
``assemble_worker``; both full-parse the whole memory store (~68 entry files
plus the topics vocabulary) through ``load_store`` on every call. The cost is
to_thread time invisible to HTTP probes, so the collector times
``assemble_master`` over the live memory corpus (read-only), from the
checkout under test: one cold pass, as at a server start, then nine timed
calls. The fixed loader memoizes the parsed Store on a stat-only signature
((relative path, mtime_ns, size) of every parsed file): the steady state is
~69 stats and zero store bytes, and any rewrite, append, new entry, or
deletion re-parses. Only successful loads memoize — a malformed store keeps
raising on every call. Evidence while the live server runs older code points
the same collector at the branch checkout (``CHECKOUT`` at the worktree
root), the same shape as the M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import os, sys, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.memory import assemble_master

memory_dir = Path.home() / ".charliebot" / "memory"
assemble_master(memory_dir)  # cold pass, as at a server start; not timed
times = []
for _ in range(9):
    t0 = time.perf_counter()
    assemble_master(memory_dir)
    times.append(time.perf_counter() - t0)
times.sort()
n_files = sum(1 for p in (memory_dir / "entries").rglob("*.md"))
print(f"{n_files} entry files; steady-state assemble_master "
      f"median {times[4] * 1000:.2f} ms, max {times[-1] * 1000:.2f} ms")
EOF
```

M33 — assistant-stream draft render, full-turn replay. Every `stream` WebSocket
delta carries the whole accumulated draft, and the pre-fix `showStreaming`
(usage.js) painted it on every delta — `marked.parse(fixNestedFences(whole
draft))` plus a KaTeX re-walk and a bubble DOM swap — so a turn with N deltas
over an S-byte draft costs O(N × S) of main-thread markdown work, the chat UI's
jank source during long streamed turns; the fixed form coalesces paints to a
200 ms leading+trailing cadence, and `hideStreaming` cancels the pending paint
(every terminal path — committed bubble, error, session swap — hides first).
The cost is client-side, invisible to every HTTP probe, so the collector
(tests/stream_render_collector.js + the shared stream_render_harness.js) replays
a full turn in a node vm against the checkout's real usage.js/markdown-renderer.js
and the page-pinned marked build: the largest single assistant text block across
live chat files (a block cannot exceed its file's size, so smaller files skip),
fed as 200 B deltas at a 40 ms virtual cadence against a stub DOM/clock, one
cold pass then five timed replays wall-clocking only render work; KaTeX's
per-paint re-walk is stubbed identically in both arms, understating rather than
overstating the win (parity: the last frame equals a direct full-draft render).
Evidence points the collector at the branch checkout (`CHECKOUT` at the
worktree root, live state read-only), the same shape as the M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} node /home/chaoli/workspace/charlie-bot/tests/stream_render_collector.js
```

M34 — worker-events poll fetch at the client's rendered count. The 5 s
workers-panel events poll served the whole projected history every round —
860706 B and 2177 events for the worst on-disk log; the projection itself
is memoized (M13), but serialization, transfer, and the client's full
innerHTML rebuild stayed O(history) per poll. The endpoint's ``after=N``
returns only the events past the client's rendered raw count (a sound
prefix cut: ``_append_worker_events`` never rewrites an emitted row) and
answers ``reset`` + the full payload when the count runs ahead; the client
appends tails through a scratch paint plus ``insertAdjacentHTML``. The
steady state (the metric) is a poll with nothing new. Both fetch shapes
ride pre-dumped rows through FastJsonResponse (a Response skips
response_model's jsonable_encoder pass, ~6x ``model_dump`` on mapped
returns), and the full fetch's gzip form rides the body-keyed memo — one
off-loop deflate per distinct projection, ``Content-Encoding`` set upstream
so the middleware skips (the M59/M71 mechanism); the panel's re-open of an
unchanged log is the repeat shape the full-fetch line prices, the cold
first parse + first deflate of a fresh body is the panel's one-time cost
and is reported, not priced. The collector copies
the worst on-disk worker log into a scratch ``CHARLIEBOT_HOME`` (live home
read once, never written), and drives the endpoint raw-ASGI behind the
production gzip middleware — the served path the middleware and route
actually run (a TestClient drive adds ~1.5-2 ms of httpx harness per
request and skips the middleware whose deflate the browser's fetch always
pays) — one cold pass per shape, as at first panel open after a server
start, then seven timed full fetches (the re-open shape) and seven timed
``after=total`` fetches, asserting within-round body identity and the
envelope-at-0 rows equaling the full fetch's parsed rows; the pytest suite
pins prefix+tail parity. Evidence points the same collector at the branch
checkout (``CHECKOUT`` at the worktree root), the same shape as the M7
protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, gzip, hashlib, json, os, shutil, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from fastapi import FastAPI
from server import _CharlieBotGZipMiddleware
from src.api.deps import get_thread_manager
from src.api.threads import router as threads_router
from src.core.config import CharlieBotConfig
from src.core.threads import ThreadManager

# Worst worker-events corpus: the largest events.jsonl on disk; live home
# read once for the copy, never written; the endpoint reads only the copy.
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for p in root.glob("*/threads/*/data/events.jsonl"):
    n = p.stat().st_size
    if n > best_n:
        best, best_n = p, n
SID, TID = best.parts[-5], best.parts[-3]
home = Path(tempfile.mkdtemp(prefix="m34-events-home-", dir="/tmp"))
dst = home / "sessions" / SID / "threads" / TID / "data" / "events.jsonl"
dst.parent.mkdir(parents=True)
shutil.copy2(best, dst)

app = FastAPI()
app.include_router(threads_router, prefix="/api/threads")
cfg = CharlieBotConfig(charliebot_home=home)
app.dependency_overrides[get_thread_manager] = lambda: ThreadManager(cfg)
# The production middleware chain: the browser's panel fetch always sends
# Accept-Encoding: gzip, so the full fetch's deflate is part of the served
# shape — a TestClient drive adds ~1.5-2 ms of httpx harness per request and
# skips the middleware whose deflate the browser's fetch always pays (the
# vacuous-read class the M36/M59/M71 repairs called out).
app.add_middleware(_CharlieBotGZipMiddleware, minimum_size=1000, compresslevel=1)
url = f"/api/threads/{SID}/threads/{TID}/events"


def scope(url, query=b""):
    return {
        "type": "http", "asgi": {"version": "3.0", "spec_version": "2.3"},
        "http_version": "1.1", "method": "GET", "scheme": "http",
        "path": url, "raw_path": (url + ("?" + query.decode() if query else "")).encode(),
        "query_string": query, "root_path": "",
        "headers": [(b"host", b"test"), (b"accept-encoding", b"gzip")],
        "client": ("test", 123), "server": ("test", 80),
    }


async def drive(url, query=b""):
    body = b""
    out = {"status": 0, "encoding": b""}

    async def receive():
        return {"type": "http.request", "body": b"", "more_body": False}

    async def send(msg):
        nonlocal body
        if msg["type"] == "http.response.start":
            out["status"] = msg["status"]
            out["encoding"] = dict(msg.get("headers", [])).get(b"content-encoding", b"")
        elif msg["type"] == "http.response.body":
            body += msg.get("body", b"")

    t0 = time.perf_counter()
    await app(scope(url, query), receive, send)
    return time.perf_counter() - t0, body, out


def decoded(body, encoding):
    return gzip.decompress(body) if encoding == b"gzip" else body


def digest(decoded):
    return hashlib.sha256(json.dumps(json.loads(decoded), sort_keys=True).encode()).hexdigest()[:12]


async def timed(url, query=b""):
    cold_dt, cold_body, cold_out = await drive(url, query)  # cold pass, as at first panel open; not timed
    cold_dec = decoded(cold_body, cold_out["encoding"])
    cold = (cold_dt, len(cold_body), len(cold_dec), digest(cold_dec))
    times, wire, dec, digests = [], 0, 0, set()
    for _ in range(7):
        dt, body, out = await drive(url, query)
        assert out["status"] == 200, (url, out["status"])
        d = decoded(body, out["encoding"])
        times.append(dt)
        wire = len(body)
        dec = len(d)
        digests.add(digest(d))
    times.sort()
    assert len(digests) == 1, f"repeat bodies differ: {digests}"
    return cold, times[3], times[-1], wire, dec, digests.pop()


async def main():
    (f_cold, _f_cw, _f_cd, f_dig), fm, fx, fwire, fdec, fdig = await timed(url)
    assert f_dig == fdig
    _env_dt, env_body, env_out = await drive(url, b"after=0")
    env = json.loads(decoded(env_body, env_out["encoding"]))
    total = env["total"]
    assert env["reset"] is False
    _full_dt, full_body, full_out = await drive(url)
    assert env["events"] == json.loads(decoded(full_body, full_out["encoding"]))
    (a_cold, _a_cw, _a_cd, a_dig), am, ax, awire, _adec, adig = await timed(url, f"after={total}".encode())
    assert a_dig == adig
    print(f"checkout {os.environ['CHECKOUT'].rsplit('/', 1)[-1]}: {best_n / 1e6:.1f} MB log, {total} events; "
          f"full fetch cold (first parse + first deflate) {f_cold * 1000:.2f} ms, "
          f"repeat median {fm * 1000:.2f} ms, max {fx * 1000:.2f} ms, wire {fwire} B, decoded {fdec} B, digest {fdig}; "
          f"after=total cold {a_cold * 1000:.2f} ms, median {am * 1000:.2f} ms, max {ax * 1000:.2f} ms, "
          f"body {awire} B, digest {adig}")

asyncio.run(main())
shutil.rmtree(home)
EOF
```

M35 — chat message-page responses (events/view/bootstrap), steady state. The chat
pagination endpoint (``GET /api/sessions/{id}/events``), the SPA-switch session view,
and the bootstrap payload return their message pages through FastJSON renders whose
bodies ride the production gzip middleware — the browser's page fetch always sends
``Accept-Encoding: gzip``, so each response's deflate is part of the served shape.
The events page serves its rendered body from the projection's own cache (M26); the
fixed form memoizes the page's gzip form beside the plain one and ships it with
``Content-Encoding`` set upstream, which is what makes the middleware skip its own
per-request pass (the M72 listing-serve mechanism) — one level-1 deflate per page
per projection generation, in the executor, instead of one per click. The cost
is per page click / SPA switch, invisible to the standing HTTP probes, so the
collector snapshots the worst projection corpus (the session with the most live
chat events) into one shared scratch ``CHARLIEBOT_HOME`` (live home read once for
the copy, never written) and drives the three endpoints raw-ASGI behind the
production gzip middleware in each checkout's process — the served path the
middleware and route actually run; a TestClient drive adds ~1.5-2 ms of httpx
harness per request and skips the middleware whose deflate the browser's fetch
always pays, the vacuous-read class the M36/M59 repairs called out: one cold pass
per endpoint, as at first view after a server start, then five timed requests, with
digests read off the decoded last timed response. All three endpoints are
side-effect-free reads — the view/bootstrap mark_read write-once moved off the
fetch path to the client's post-render ``POST /read`` — so no write side effect
survives to skew the cross-checkout comparison. Evidence points the same
collector at the before and after checkouts (``CHECKOUT`` at each root, shared
``M35_HOME`` snapshot), asserting identical decoded bodies, the same shape as the
M7 protocol. Snapshot once:

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import shutil, tempfile
from pathlib import Path

root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    p = d / "data" / "chat_events.jsonl"
    if p.is_file():
        with open(p, errors="replace") as f:
            n = sum(1 for _ in f)
        if n > best_n:
            best, best_n = d, n
home = Path(tempfile.mkdtemp(prefix="m35-msg-page-home-", dir="/tmp"))
shutil.copytree(best, home / "sessions" / best.name)
print(f"export M35_HOME={home} M35_SID={best.name} M35_N={best_n}")
EOF
```

Then run per checkout (``eval`` the snapshot export first):

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, shutil, gzip, hashlib, json, os, sys, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from fastapi import FastAPI
from server import _CharlieBotGZipMiddleware
import src.api.deps as deps
from src.api.deps import get_config, get_session_manager, get_thread_manager
from src.api.sessions import router as sessions_router
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager
from src.core.triggers import TriggerManager

home = Path(os.environ["M35_HOME"])
SID = os.environ["M35_SID"]
BEFORE_N = int(os.environ["M35_N"])

# Scratch wiring: managers and config resolve to the snapshot; the view handler's
# direct get_trigger_manager() call is seeded with the scratch manager too.
cfg = CharlieBotConfig(charliebot_home=home)
mgr = SessionManager(cfg)
deps._trigger_manager = TriggerManager(cfg, mgr)
app = FastAPI()
app.include_router(sessions_router, prefix="/api/sessions")
app.dependency_overrides[get_session_manager] = lambda: mgr
app.dependency_overrides[get_thread_manager] = lambda: ThreadManager(cfg)
app.dependency_overrides[get_config] = lambda: cfg
# The production middleware chain: the browser's page fetch always sends
# Accept-Encoding: gzip, so the body's deflate is part of the served shape —
# a bare app reads the render floor alone.
app.add_middleware(_CharlieBotGZipMiddleware, minimum_size=1000, compresslevel=1)


def scope(url, query=b""):
    return {
        "type": "http", "asgi": {"version": "3.0", "spec_version": "2.3"},
        "http_version": "1.1", "method": "GET", "scheme": "http",
        "path": url, "raw_path": (url + ("?" + query.decode() if query else "")).encode(),
        "query_string": query, "root_path": "",
        "headers": [(b"host", b"test"), (b"accept-encoding", b"gzip")],
        "client": ("test", 123), "server": ("test", 80),
    }


async def drive(url, query=b""):
    body = b""
    out = {"status": 0, "encoding": b""}

    async def receive():
        return {"type": "http.request", "body": b"", "more_body": False}

    async def send(msg):
        nonlocal body
        if msg["type"] == "http.response.start":
            out["status"] = msg["status"]
            out["encoding"] = dict(msg.get("headers", [])).get(b"content-encoding", b"")
        elif msg["type"] == "http.response.body":
            body += msg.get("body", b"")

    t0 = time.perf_counter()
    await app(scope(url, query), receive, send)
    return time.perf_counter() - t0, body, out


def digest(decoded):
    return hashlib.sha256(json.dumps(json.loads(decoded), sort_keys=True).encode()).hexdigest()[:12]


async def timed(url, query=b""):
    _, _, out = await drive(url, query)  # cold pass, as at first view after a server start; not timed
    encoding = out["encoding"]
    times, wire, decoded_size, digests = [], 0, 0, set()
    for _ in range(5):
        dt, body, out = await drive(url, query)
        assert out["status"] == 200, (url, out["status"])
        decoded = gzip.decompress(body) if out["encoding"] == b"gzip" else body
        times.append(dt)
        wire = len(body)
        decoded_size = len(decoded)
        digests.add(digest(decoded))
    times.sort()
    assert len(digests) == 1, f"repeat bodies differ: {digests}"
    return times, wire, decoded_size, digests.pop(), encoding


async def main():
    ev_t, ev_wire, ev_dec, ev_d, ev_enc = await timed(
        f"/api/sessions/{SID}/events", f"before={BEFORE_N}&limit=200".encode())
    vw_t, vw_wire, vw_dec, vw_d, vw_enc = await timed(f"/api/sessions/{SID}/view")
    bt_t, bt_wire, bt_dec, bt_d, bt_enc = await timed(f"/api/sessions/{SID}/bootstrap")
    print(f"checkout {os.path.basename(os.environ['CHECKOUT'])}: events median {ev_t[2]*1000:.2f} ms, "
          f"max {ev_t[-1]*1000:.2f} ms (wire {ev_wire} B, decoded {ev_dec} B, enc {ev_enc.decode() or 'none'}, digest {ev_d}); "
          f"view median {vw_t[2]*1000:.2f} ms, max {vw_t[-1]*1000:.2f} ms (wire {vw_wire} B, decoded {vw_dec} B, digest {vw_d}); "
          f"bootstrap median {bt_t[2]*1000:.2f} ms, max {bt_t[-1]*1000:.2f} ms (wire {bt_wire} B, decoded {bt_dec} B, digest {bt_d})")


try:
    asyncio.run(main())
finally:
    shutil.rmtree(home)  # every exit path removes the scratch copy: the hourly cadence leaks one copy per skipped removal
EOF
```

M36 — worker list poll payload and handler time, steady state. The 3 s
workers-panel poll (``GET /api/threads/{sid}/list``) serves an unchanged
session from the whole-body memo; a poll repeating the ETag it rendered via
``?etag=`` gets a bodyless 204 instead of the full rows, so the steady state
transfers zero body bytes and the client skips its JSON.parse. The conditional
rides a query param rather than If-None-Match because the browser's HTTP cache
fulfils a revalidation itself and fetch never surfaces the 304; no-store on
every answer keeps each poll a real request. The collector drives the endpoint
raw-ASGI — the served path the middleware and route actually run; a TestClient
drive adds ~1.5 ms of httpx harness per request and skips the gzip middleware
whose deflate the browser's poll always pays (the vacuous-read class the
M57/M70/M72 repairs called out) — over the session whose threads directory
carries the most metadata bytes (live state read-only), with the thread and
trigger managers built once as the server's dependency singletons are —
per-request manager instances would rebuild the M5/M24 memos on every call and
drown the measured path in a memo-cold scan the live server never pays. One
cold pass, as at first panel paint after a server start, then seven timed full
requests (first paint / changed rows; byte-identical code path before and
after) and seven timed conditional repeats. Evidence while the live server runs
older code points the same collector at the branch checkout (``CHECKOUT`` at
the worktree root), the same shape as the M7 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, gzip, hashlib, json, os, sys, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from fastapi import FastAPI
from server import _CharlieBotGZipMiddleware
from src.api.deps import get_thread_manager, get_trigger_manager
from src.api.threads import router as threads_router
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager
from src.core.triggers import TriggerManager

# Worst worker-list corpus: the session whose threads carry the most metadata
# bytes; the endpoint reads live state read-only. Managers are built once, as
# the server's dependency singletons are.
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    t = d / "threads"
    if t.is_dir():
        n = sum((p / "metadata.json").stat().st_size for p in t.iterdir() if (p / "metadata.json").is_file())
        if n > best_n:
            best, best_n = d, n
SID = best.name

cfg = CharlieBotConfig(charliebot_home=Path.home() / ".charliebot")
thread_mgr = ThreadManager(cfg)
trigger_mgr = TriggerManager(cfg, SessionManager(cfg))
app = FastAPI()
app.include_router(threads_router, prefix="/api/threads")
app.dependency_overrides[get_thread_manager] = lambda: thread_mgr
app.dependency_overrides[get_trigger_manager] = lambda: trigger_mgr
# The production middleware chain: the browser's poll always sends
# Accept-Encoding: gzip, so the full poll's deflate is part of the served
# shape — a bare app reads the handler floor alone.
app.add_middleware(_CharlieBotGZipMiddleware, minimum_size=1000, compresslevel=1)

def scope(url, query=b""):
    return {
        "type": "http", "asgi": {"version": "3.0", "spec_version": "2.3"},
        "http_version": "1.1", "method": "GET", "scheme": "http",
        "path": url, "raw_path": url.encode(), "query_string": query, "root_path": "",
        "headers": [(b"host", b"test"), (b"accept-encoding", b"gzip")],
        "client": ("test", 123), "server": ("test", 80),
    }


async def drive(url, query=b""):
    body = b""
    out = {"status": 0, "encoding": b"", "headers": {}}

    async def receive():
        return {"type": "http.request", "body": b"", "more_body": False}

    async def send(msg):
        nonlocal body
        if msg["type"] == "http.response.start":
            out["status"] = msg["status"]
            out["headers"] = dict(msg.get("headers", []))
            out["encoding"] = out["headers"].get(b"content-encoding", b"")
        elif msg["type"] == "http.response.body":
            body += msg.get("body", b"")

    t0 = time.perf_counter()
    await app(scope(url, query), receive, send)
    return time.perf_counter() - t0, body, out


def digest(decoded):
    return hashlib.sha256(json.dumps(json.loads(decoded), sort_keys=True).encode()).hexdigest()[:12]


async def main():
    url = f"/api/threads/{SID}/list"
    _, cold_body, cold_out = await drive(url)  # cold pass, as at first panel paint after a server start; not timed
    decoded = gzip.decompress(cold_body) if cold_out["encoding"] == b"gzip" else cold_body
    rows = json.loads(decoded)
    etag = cold_out["headers"].get(b"etag", b"").decode()
    full_times, full_decoded, full_wire, digests = [], 0, 0, set()
    for _ in range(7):
        dt, body, out = await drive(url)
        d = gzip.decompress(body) if out["encoding"] == b"gzip" else body
        full_times.append(dt)
        full_decoded = len(d)
        full_wire = len(body)
        digests.add(digest(d))
    full_times.sort()
    assert len(digests) == 1, f"full poll bodies differ: {digests}"
    trunc = sum(1 for row in rows if "description_full_len" in row)
    cond_query = f"etag={etag}".encode() if etag else b""
    cond_times, cond_status, cond_wire = [], 0, -1
    for _ in range(7):
        dt, body, out = await drive(url, cond_query)
        cond_times.append(dt)
        cond_status = out["status"]
        cond_wire = len(body)
    cond_times.sort()
    assert cond_status == 204 and cond_wire == 0, (cond_status, cond_wire)
    print(f"checkout {os.environ['CHECKOUT'].rsplit('/', 1)[-1]}: {best_n / 1e3:.0f} KB thread metadata over {len(rows)} rows "
          f"in session {SID} ({trunc} truncated); full poll median {full_times[3] * 1000:.2f} ms, "
          f"max {full_times[-1] * 1000:.2f} ms, decoded {full_decoded} B wire {full_wire} B, digest {digests.pop()}; "
          f"conditional poll median {cond_times[3] * 1000:.2f} ms, max {cond_times[-1] * 1000:.2f} ms, body {cond_wire} B (204)")

asyncio.run(main())
EOF
```

M37 — archived-session chat tail page, steady state. Sessions with
``archive_offset > 0`` serve the chat view and bootstrap from
``load_chat_events_tail`` → ``parse_ndjson_tail`` (unarchived sessions use the
M26 message projection), and the pre-fix reader paid a full-file line count
plus the 512 KiB tail-window parse on every SPA switch — repeat work against
an unchanged file. The fixed reader memoizes the whole page and,
independently, the line count on (mtime_ns, size): a repeat over an
unchanged file pays one stat and zero file bytes; an append re-reads the
count and window. Each signature is taken before its read, so an entry
recorded during a concurrent append keys the older signature and can never
be served for the newer bytes (chat files only append; archive rewrites
replace the whole file). The line-count memo is shared with
``count_ndjson_lines``, the ``get_chat_event_count_sync`` path behind the
recap default divider. The cost is a per-view latency no standing HTTP
probe isolates (the biggest live files belong to unarchived sessions, which
route through the projection — the heaviest tail-page corpus on disk is the
7.4 MB archived-session live file), so the collector times the function the
view awaits over the worst on-disk archived live file (read-only), from the
checkout under test: one cold pass, as at first view of the session, then
nine timed repeats, asserting the served page is repeat-call identical.
Evidence while the live server runs older code points the same collector at
the branch checkout (``CHECKOUT`` at the worktree root), the same shape as
the M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import json, os, sys, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.ndjson import parse_ndjson_tail

# Worst tail-page corpus: the archived session whose LIVE chat file carries
# the most bytes; view/bootstrap tail pages count and parse it per call.
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    meta_p = d / "metadata.json"
    if not meta_p.is_file():
        continue
    try:
        off = json.loads(meta_p.read_text()).get("archive_offset", 0)
    except Exception:
        continue
    live = d / "data" / "chat_events.jsonl"
    if off and off > 0 and live.is_file():
        n = live.stat().st_size
        if n > best_n:
            best, best_n = live, n
print(f"worst archived live tail corpus: session {best.parts[-3]}, {best_n / 1e6:.1f} MB")

first = parse_ndjson_tail(best, 200)  # cold pass, as at first view of the session; not timed
times = []
result = None
for _ in range(9):
    t0 = time.perf_counter()
    result = parse_ndjson_tail(best, 200)
    times.append(time.perf_counter() - t0)
times.sort()
assert result == first, "served tail page differs between repeat calls"
events, total, has_more = result
print(f"{total} lines, tail events {len(events)}; steady-state parse_ndjson_tail(200) "
      f"median {times[4] * 1000:.2f} ms, max {times[-1] * 1000:.2f} ms")
EOF
```

M38 — session stream-broadcast fan-out, worst on-disk turn replay. Every stream part
feeds ``persist_and_broadcast``, whose aggregator emits a ``stream`` preview carrying the
whole accumulated draft; the pre-fix fan-out serialized that draft per delta per
subscriber (O(deltas × draft) of event-loop json.dumps: 78 ms / 174 frames on the worst
on-disk turn), and the fixed StreamingManager coalesces previews per channel (leading +
200 ms trailing windows at the client's showStreaming paint cadence, pending dropped only
on the preview-hiding types) and serializes once per fan-out. The collector resolves the
worst stream turn on disk (events between bare user events whose stream deltas carry the
most preview bytes; only files ≥ 2 MB contend), replays it instant-feed through the real
``MessageAggregator`` and ``StreamingManager`` into a stub socket mirroring starlette's
send_json (both ``json.dumps`` and ``orjson.dumps`` wrapped process-wide, so the counted
serialize cost is the wire render whichever renderer the checkout uses), stopping before
``master_done`` so both arms must deliver the final preview. Evidence points the
collector at the before and after checkouts (``CHECKOUT`` at each root):

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, json, os, sys, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.message_aggregator import MessageAggregator
from src.core.streaming import StreamingManager

class Probe:
  def __init__(self):
    self.frames = []
  async def send_json(self, data):
    self.frames.append(data)
    json.dumps(data, separators=(",", ":"), ensure_ascii=False)  # starlette send_json's cost
  async def send_text(self, text):
    self.frames.append(json.loads(text))

stats = {"calls": 0, "time": 0.0}
real_dumps = json.dumps
def timed_dumps(*a, **kw):
  t0 = time.perf_counter()
  out = real_dumps(*a, **kw)
  stats["calls"] += 1
  stats["time"] += time.perf_counter() - t0
  return out

import orjson
real_odumps = orjson.dumps
def timed_odumps(*a, **kw):
  t0 = time.perf_counter()
  out = real_odumps(*a, **kw)
  stats["calls"] += 1
  stats["time"] += time.perf_counter() - t0
  return out

def worst_stream_turn():
  # Turn: events between bare user events; score: summed stream-preview bytes
  # (the serialization driver); only files >= 2 MB can hold a contender.
  best = (-1, None, None)
  for p in Path.home().glob(".charliebot/sessions/*/data/chat_events.jsonl"):
    if p.stat().st_size < 2e6:
      continue
    events = []
    with open(p, errors="replace") as f:
      for line in f:
        try: events.append(json.loads(line))
        except json.JSONDecodeError: pass
    turns, cur = [], []
    for ev in events:
      if ev.get("type") == "user" and "message" not in ev:
        if cur:
          turns.append(cur)
        cur = [ev]
      else:
        cur.append(ev)
    if cur:
      turns.append(cur)
    for turn in turns:
      agg, score = MessageAggregator(), 0
      for i, ev in enumerate(turn):
        for d in agg.feed_indexed([(i, ev)]):
          if d["type"] == "stream":
            score += len(d["message"].get("content", "")) + len(d["message"].get("thinking", ""))
      if score > best[0]:
        best = (score, turn, p.parts[-3])
  return best[1], best[2]

async def main():
  turn, sid = worst_stream_turn()
  probe = Probe()
  manager = StreamingManager()
  await manager.subscribe("m38", probe)
  agg = MessageAggregator()
  final = None
  json.dumps = timed_dumps
  orjson.dumps = timed_odumps
  t0 = time.perf_counter()
  for i, ev in enumerate(turn):
    if ev.get("type") == "master_done":
      break  # stop before the commit; both arms must deliver the final preview first
    for d in agg.feed_indexed([(i, ev)]):
      if d["type"] == "stream":
        final = d
      await manager.broadcast("m38", d)
    if ev.get("type") not in ("assistant", "user", "scheduled_trigger"):
      await manager.broadcast("m38", ev)
  wall = time.perf_counter() - t0
  await asyncio.sleep(0.5)  # settle: lets the fixed arm's trailing flush land
  json.dumps = real_dumps
  orjson.dumps = real_odumps
  streams = [f for f in probe.frames if f.get("type") == "stream"]
  parity = bool(streams) and final is not None and streams[-1] == final
  print(f"session {sid}; turn replay {wall:.2f} s, {len(streams)} stream frames, "
        f"{stats['calls']} serialize calls {stats['time'] * 1000:.0f} ms; final-frame parity {parity}")

asyncio.run(main())
EOF
```

M39 — tui/status busy check, steady state. The 3 s tui-status poll
(`fetchTuiStatus`, per visible tab) runs `_claude_jsonl_busy` per running tui
session, whose pre-fix form globbed all of `~/.claude/projects` inline on the
event loop on every call — ~15 ms of loop stall per check at this host's
942-dir projects corpus, delaying every concurrent request and WebSocket. The
fixed form memoizes the transcript path per session id (a hit is stable for
the session's life; a miss re-globs at a 30 s TTL) and awaits the check in a
thread. The collector drives the endpoint's fixed call shape
(`asyncio.to_thread` of `_claude_jsonl_busy`) with a concurrent 5 ms ticker
over a synthetic never-present session id — the glob cost is corpus-shaped,
identical for a real hit — from the checkout under test: one cold pass, as at
a server start with an empty memo, then nine timed steady-state checks (all
inside the miss TTL, so a glob would fire on every call were the memo absent).
The pre-fix number is the same command with `_claude_jsonl_busy(SID)` called
inline (the endpoint's pre-fix call shape) against an unmemoized import.

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, os, sys, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.agents.backends.tui import _claude_jsonl_busy

# Synthetic never-present id; the glob cost is corpus-shaped (~/.claude/projects
# dir count), identical for a real hit and this miss-shaped stand-in.
SID = "00000000-0000-0000-0000-000000000000"

async def run_once():
    gaps = []
    stop = False
    async def ticker():
        prev = time.perf_counter()
        while not stop:
            await asyncio.sleep(0.005)
            now = time.perf_counter()
            gaps.append(now - prev)
            prev = now
    t = asyncio.create_task(ticker())
    t0 = time.perf_counter()
    busy = await asyncio.to_thread(_claude_jsonl_busy, SID)  # the endpoint's call shape
    wall = time.perf_counter() - t0
    stop = True
    await t
    return busy, (max(gaps) if gaps else wall), wall

async def main():
    await run_once()  # cold pass, as at a server start with an empty memo; not timed
    results = []
    for _ in range(9):
        results.append(await run_once())
    lags = sorted(r[1] for r in results)
    walls = sorted(r[2] for r in results)
    n_dirs = sum(1 for _ in (Path.home() / ".claude" / "projects").iterdir())
    print(f"{n_dirs} project dirs; busy={results[0][0]}; "
          f"loop-lag median {lags[4]*1000:.2f} ms, max {lags[-1]*1000:.2f} ms; "
          f"busy-check wall median {walls[4]*1000:.2f} ms, max {walls[-1]*1000:.2f} ms")

asyncio.run(main())
EOF
```

M40 — session-list filtered listing and group-name reduction, steady state. The
sessions page's load fetches hit `GET /api/sessions/starred` and
`GET /api/sessions/groups`; the pre-fix `list_sessions` copied and
thinking-stamped every cached meta (~1012 rows at measurement) before the
starred/scheduled filters ran (the starred list keeps ~10), and the groups
handler plus the autonamer's group list paid the same full copy pass to reduce
the corpus to ~25 names — several ms of on-loop pydantic work per request.
The fixed `list_sessions` runs the starred/scheduled filters against the shared
cached metas (read-only) so only surviving rows pay the model_copy and thinking
stamp, and the group-name readers go through `list_group_names`, a read-only
reduction of the cached metas with no copies at all. The per-request enrich and
enrichment-free paths behind the standing status poll are unchanged. The cost
is a page-load latency no standing row covers, so the collector times the two
handler-level manager calls over the live session corpus (read-only), from the
checkout under test: one cold pass, as at a server start with an empty metadata
cache, then nine timed pairs. The pre-fix numbers used in the landing PR's
evidence are the same verbatim command with the pre-fix groups handler body
(`sorted({s.group for s in await mgr.list_sessions() if s.group})`) in place of
the `list_group_names()` call, the same shape as the M21 protocol. Evidence
while the live server runs older code points the same collector at the branch
checkout (`CHECKOUT` at the worktree root), the same shape as the M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, os, sys, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager

async def main():
    cfg = CharlieBotConfig(charliebot_home=Path.home() / ".charliebot")
    mgr = SessionManager(cfg)
    await mgr.list_group_names()  # cold pass, as at a server start; not timed
    stimes, gtimes, n_star, n_groups = [], [], 0, 0
    for _ in range(9):
        t0 = time.perf_counter()
        starred = await mgr.list_sessions(starred=True, include_running_status=True, include_pending_trigger_status=True)
        stimes.append(time.perf_counter() - t0)
        t0 = time.perf_counter()
        groups = await mgr.list_group_names()
        gtimes.append(time.perf_counter() - t0)
        n_star, n_groups = len(starred), len(groups)
    stimes.sort()
    gtimes.sort()
    print(f"starred list: {n_star} rows, median {stimes[4]*1000:.2f} ms, max {stimes[-1]*1000:.2f} ms; "
          f"group-name reduction: {n_groups} groups, median {gtimes[4]*1000:.2f} ms, max {gtimes[-1]*1000:.2f} ms")

asyncio.run(main())
EOF
```

M41 — git diff/files repeat view, steady state. The /diff viewer fetches
``GET /api/git/diff/files`` on every page open and on every refresh of a review
loop's pinned /diff link; each fetch ran one ``rev-parse`` and two sequential
``git diff`` subprocesses over the range (~0.18 s on the root..HEAD worst
corpus) to recompute an immutable result: a diff between two commits never
changes (SHAs are content-addressed). The fixed handler resolves both refs
through a ref-resolution memo keyed on a stat-only ref-state signature (the
git dir's top-level pseudo-refs, packed-refs, and the loose refs tree — the
full ``rev-parse`` read set), runs the two manifest diffs concurrently on a
miss, and serves a repeat view of the same (repo, base_sha, head_sha, mode,
.gitattributes signature) from the manifest memo with zero subprocesses — the
attributes signature keys git's worktree diff drivers, which feed
``--numstat``'s counts. The cost is per page view, invisible to the standing
HTTP probes, so the collector drives ``diff_files`` over the charlie-bot
checkout's full history (root commit .. HEAD — the largest range this host's
workspace carries; read-only, with a scratch ``CHARLIEBOT_HOME``), from the
checkout under test: the first view, as at page open with a cold memo, then
seven timed repeats. Evidence while the live server runs older code points the
same collector at the branch checkout (``CHECKOUT`` at the worktree root), the
same shape as the M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, os, subprocess, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.config import CharlieBotConfig
from src.api.git import diff_files

REPO = Path("/home/chaoli/workspace/charlie-bot")
BASE = subprocess.run(["git", "rev-list", "--max-parents=0", "HEAD"],
                      cwd=REPO, capture_output=True, text=True, check=True).stdout.splitlines()[0]
cfg = CharlieBotConfig(charliebot_home=Path(tempfile.mkdtemp(prefix="m41-home-")),
                       paths={"workspace_dirs": ["/home/chaoli/workspace"]})

async def main():
    t0 = time.perf_counter()
    result = await diff_files(repo=str(REPO), base=BASE, head="HEAD", mode="three-dot", cfg=cfg)  # first view, as at page open; not a repeat
    cold = time.perf_counter() - t0
    times = []
    for _ in range(7):
        t0 = time.perf_counter()
        result = await diff_files(repo=str(REPO), base=BASE, head="HEAD", mode="three-dot", cfg=cfg)
        times.append(time.perf_counter() - t0)
    times.sort()
    print(f"{result['total_files']} files in root..HEAD diff; first view {cold:.4f} s; "
          f"repeat-view median {times[3]:.4f} s, max {times[-1]:.4f} s")

asyncio.run(main())
EOF
```

M42 — scheduler tick session scan, steady state. Every 60 s scheduler tick rebuilds the
per-task session cache from the session corpus; the pre-fix form copied and thinking-stamped
every cached meta (~1012 rows of on-loop pydantic work at measurement) only to read each
row's `scheduled_task` field, while the fixed form passes the M40 `scheduled=True` pre-copy
filter and copies just the surviving rows. The tick is background work invisible to HTTP
probes, so the collector drives `Scheduler._tick` over the live config and session
corpora with a concurrent 5 ms ticker, from the checkout under test: one cold pass, as at
a server start with an empty metadata cache, then five timed ticks. The steady state is
read-only: every enabled task keeps a matching-backend session and an unchanged cron (a
due fire is stubbed so no task spawns and no scheduler bookkeeping is written). Evidence
while the live server runs older code points the same collector at the branch checkout
(`CHECKOUT` at the worktree root), the same shape as the M25 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, os, sys, time
from unittest.mock import AsyncMock
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.config import get_config, get_scheduled_tasks
from src.core.sessions import SessionManager
from src.core.scheduler import Scheduler

async def main():
    cfg = get_config()
    sched = Scheduler(cfg, SessionManager(cfg))
    sched._execute_task = AsyncMock()  # fire stub: a due fire records without spawning
    await sched._tick()  # cold pass, as at a server start; not timed
    worst = []
    for _ in range(5):
        gaps = []
        stop = False
        async def ticker():
            prev = time.perf_counter()
            while not stop:
                await asyncio.sleep(0.005)
                now = time.perf_counter()
                gaps.append(now - prev)
                prev = now
        t = asyncio.create_task(ticker())
        await sched._tick()
        stop = True
        await t
        worst.append(max(gaps) if gaps else 0.0)
    worst.sort()
    n_enabled = sum(1 for t in get_scheduled_tasks() if t.enabled)
    print(f"{n_enabled} enabled tasks; steady-state scheduler tick loop-lag median {worst[2]:.4f} s, "
          f"max {worst[-1]:.4f} s; fire stub awaited {sched._execute_task.await_count}x over 6 ticks")

asyncio.run(main())
EOF
```

M43 — git diff/file repeat expand, steady state. The /diff viewer fetches
``GET /api/git/diff/file`` on every file expand, and a collapse drops the
rendered body, so every re-expand, second tab, or refresh re-runs one
``git diff`` subprocess over the range (~44 ms on the heaviest file of the
charlie-bot root..HEAD manifest) to recompute an immutable result: a per-file
diff between two commits never changes (SHAs are content-addressed). The fixed
handler memoizes the body on the M41 manifest key plus the pathspec — repo,
resolved base/head SHAs, mode, .gitattributes signature (diff drivers feed the
emitted hunks), (old_path, path) — and builds the miss path's range spec from
the resolved SHAs, so a ref moving mid-request can never key one pair's body
under another; a repeat pays zero subprocesses — the M41 ref-state signature
proves the resolution current and the body memo serves the bytes. The cost is
per file expand, invisible to the standing HTTP probes, so the collector drives
``diff_file`` over the heaviest file of the charlie-bot checkout's root..HEAD
manifest (read-only, scratch ``CHARLIEBOT_HOME``), from the checkout under
test: the first expand, as at page open with a cold memo, then seven timed
repeats, asserting bodies repeat-identical. Evidence while the live server
runs older code points the same collector at the branch checkout (``CHECKOUT``
at the worktree root), the same shape as the M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, os, subprocess, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.config import CharlieBotConfig
from src.api.git import diff_files, diff_file

REPO = Path("/home/chaoli/workspace/charlie-bot")
BASE = subprocess.run(["git", "rev-list", "--max-parents=0", "HEAD"],
                      cwd=REPO, capture_output=True, text=True, check=True).stdout.splitlines()[0]
cfg = CharlieBotConfig(charliebot_home=Path(tempfile.mkdtemp(prefix="m43-home-")),
                       paths={"workspace_dirs": ["/home/chaoli/workspace"]})

async def main():
    manifest = await diff_files(repo=str(REPO), base=BASE, head="HEAD", mode="three-dot", cfg=cfg)
    row = max(manifest["files"], key=lambda f: f["additions"] + f["deletions"])
    path, old_path = row["path"], row.get("old_path")
    t0 = time.perf_counter()
    first = await diff_file(repo=str(REPO), base=BASE, head="HEAD", mode="three-dot",
                            path=path, old_path=old_path, force=False, cfg=cfg)
    cold = time.perf_counter() - t0
    times = []
    repeat = None
    for _ in range(7):
        t0 = time.perf_counter()
        repeat = await diff_file(repo=str(REPO), base=BASE, head="HEAD", mode="three-dot",
                                 path=path, old_path=old_path, force=False, cfg=cfg)
        times.append(time.perf_counter() - t0)
    times.sort()
    assert repeat == first, "served file diff differs between repeat calls"
    print(f"heaviest manifest file {path} (+{row['additions']}/-{row['deletions']}), "
          f"size_bytes {first['size_bytes']}; first view {cold:.4f} s; "
          f"repeat-view median {times[3]:.4f} s, max {times[-1]:.4f} s")

asyncio.run(main())
EOF
```

M44 — scheduled-list next-run resolution, steady state. Every grouped sidebar
render pairs ``GET /api/sessions/scheduled`` with ``GET /api/cron/tasks`` (the
project-manager refresh fires on every list render), and the handler resolved
each scheduled row's next fire with one ``croniter(...).get_next`` expand per
row per request (~248 µs each measured, ~3 ms at the 12-task live corpus) — a
pure function of (cron, timezone, now) whose answer stays valid until the fire
time it names, so every repeat request inside that window recomputed an
identical string. The fixed handler serves rows from a memo keyed on (cron,
timezone), entries valid until their named fire time passes; a fire that went
by recomputes on the next request. The cost is a sidebar-render latency
invisible to the standing HTTP probes, so the collector drives the endpoint
raw-ASGI — the served path the middleware and route actually run; a TestClient
drive adds ~1.5 ms of httpx harness per request and skips the gzip middleware
whose deflate the browser's fetch always pays (the vacuous-read class the
M57/M70/M72 repairs called out) — over the live session + cron corpora
(read-only), managers built once as the server's dependency singletons are:
one cold pass, as at first scheduled-tab open after a server start, then nine
timed requests, asserting the body is repeat-identical. Evidence while the
live server runs older code points the same collector at the branch checkout
(``CHECKOUT`` at the worktree root), the same shape as the M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio
import gzip
import hashlib
import json
import os
import sys
import time

sys.path.insert(0, os.environ["CHECKOUT"])
from pathlib import Path

from fastapi import FastAPI
from server import _CharlieBotGZipMiddleware

from src.api.deps import get_config, get_session_manager, get_thread_manager, get_trigger_manager
from src.api.sessions import router as sessions_router
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager
from src.core.triggers import TriggerManager


async def main():
  # Scratch wiring against the live home: managers built once, as the server's
  # dependency singletons are; read-only over the live session + cron corpus.
  import src.api.deps as deps
  cfg = CharlieBotConfig(charliebot_home=Path.home() / ".charliebot")
  mgr = SessionManager(cfg)
  deps._trigger_manager = TriggerManager(cfg, mgr)
  app = FastAPI()
  app.include_router(sessions_router, prefix="/api/sessions")
  app.dependency_overrides[get_session_manager] = lambda: mgr
  app.dependency_overrides[get_thread_manager] = lambda: ThreadManager(cfg)
  app.dependency_overrides[get_trigger_manager] = lambda: deps._trigger_manager
  app.dependency_overrides[get_config] = lambda: cfg
  # The production middleware chain: the browser's fetch always sends
  # Accept-Encoding: gzip, so the body's deflate is part of the served
  # shape — a bare app reads the handler floor alone.
  app.add_middleware(_CharlieBotGZipMiddleware, minimum_size=1000, compresslevel=1)

  url = "/api/sessions/scheduled"

  def scope():
    return {
        "type": "http", "asgi": {"version": "3.0", "spec_version": "2.3"},
        "http_version": "1.1", "method": "GET", "scheme": "http",
        "path": url, "raw_path": url.encode(), "query_string": b"", "root_path": "",
        "headers": [(b"host", b"test"), (b"accept-encoding", b"gzip")],
        "client": ("test", 123), "server": ("test", 80),
    }

  async def drive():
    body = b""
    out = {"status": 0, "encoding": b""}

    async def receive():
      return {"type": "http.request", "body": b"", "more_body": False}

    async def send(msg):
      nonlocal body
      if msg["type"] == "http.response.start":
        out["status"] = msg["status"]
        out["encoding"] = dict(msg.get("headers", [])).get(b"content-encoding", b"")
      elif msg["type"] == "http.response.body":
        body += msg.get("body", b"")

    t0 = time.perf_counter()
    await app(scope(), receive, send)
    return time.perf_counter() - t0, body, out

  def decoded_body(body, encoding):
    return gzip.decompress(body) if encoding == b"gzip" else body

  _, cold_body, cold_out = await drive()  # cold pass, as at first scheduled-tab open after a server start; not timed
  assert cold_out["status"] == 200, (cold_out["status"],)
  times = []
  digests = set()
  for _ in range(9):
    dt, body, out = await drive()
    times.append(dt)
    decoded = decoded_body(body, out["encoding"])
    digests.add(hashlib.sha256(json.dumps(json.loads(decoded), sort_keys=True).encode()).hexdigest()[:12])
  times.sort()
  # A body changing between repeats is live churn, not determinism (a next-fire
  # rollover changes content at the same length): re-measure rather than
  # compare noise across arms.
  if len(digests) != 1:
    print("live churn during measurement; re-run")
    raise SystemExit(1)
  wire = len(body)
  decoded = decoded_body(body, out["encoding"])
  digest = hashlib.sha256(json.dumps(json.loads(decoded), sort_keys=True).encode()).hexdigest()[:12]
  rows = len(json.loads(decoded))
  print(f"{rows} scheduled rows, wire {wire} B, decoded {len(decoded)} B, digest {digest}; "
        f"served /scheduled median {times[4]*1000:.2f} ms, max {times[-1]*1000:.2f} ms over 9")


asyncio.run(main())
EOF
```

M45 — session-WS catchup replay event-loop lag, stale-cursor reconnect. When a session
WebSocket (re)connects behind the live event count (a mid-turn reconnect after a network
flap), `_send_session_catchup` replays the events past the cursor through
`_replay_aggregated_catchup`, which must feed the FULL event list to rebuild aggregator
state (a run interval that opened before the cursor drives the deltas after it). The
pre-fix form ran that feed loop inline on the event loop — 22 ms measured 2026-09-04
on the 20534-event worst live corpus — freezing every concurrent request and WebSocket
for the walk's wall time; the 2026-09-04 form moved it to a whole-corpus thread run,
which traded the inline freeze for loop-lag behind GIL handoffs (10.2 ms measured
2026-09-05); the fixed form feeds the same walk in ~1 ms on-loop slices through
`_CatchupWalk` and sends pre-rendered wire text in order, so no slice holds the loop
past the poll cadences' resolution (identical frame list and wire bytes, identical
stop-at-first-failure count).
The cursor==total fast-skip never enters the replay, which the past day's 85 live
`session_ws_catchup_sent` log lines confirm (all sent=0), so this is a cold-path
insurance metric, invisible to the standing probes; the collector resolves the session
whose live chat file carries the most events, copies only that session into a scratch
`CHARLIEBOT_HOME` under /tmp (metadata.json and data/ only; live home read once for
the copy, never written), warms the events cache as the server's to_thread load does,
and replays at cursor = total − 50 (a reconnect 50 events behind) through a stub
socket with a concurrent 5 ms ticker, reporting the replay's worst ticker gap plus
wall — one cold pass, as at the first stale-cursor reconnect after a server start,
then five timed replays, with a frame-list digest pinning cross-checkout parity.
Evidence while the live server runs older code points the same collector at the
branch checkout (`CHECKOUT` at the worktree root), the same shape as the M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, hashlib, json, os, shutil, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager
from server import _replay_aggregated_catchup

# Worst replay corpus: the session whose LIVE chat file carries the most
# events; a stale-cursor replay feeds every event before the cursor.
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    p = d / "data" / "chat_events.jsonl"
    if p.is_file():
        with open(p, errors="replace") as f:
            n = sum(1 for _ in f)
        if n > best_n:
            best, best_n = d, n
SID = best.name

# Isolation: scratch CHARLIEBOT_HOME under /tmp holding only a copy of that
# session's metadata.json and data/; live home read once for the copy, never written.
home = Path(tempfile.mkdtemp(prefix="m45-catchup-home-", dir="/tmp"))
dst = home / "sessions" / SID
dst.mkdir(parents=True)
shutil.copy2(best / "metadata.json", dst / "metadata.json")
shutil.copytree(best / "data", dst / "data")
cfg = CharlieBotConfig(charliebot_home=home)
mgr = SessionManager(cfg)

class Stub:
    def __init__(self):
        self.raw = []
    async def send_json(self, data):
        self.raw.append(json.dumps(data, separators=(",", ":"), ensure_ascii=False))
    async def send_text(self, text):
        self.raw.append(text)  # O(1), like the real transport write; parsed after timing for the digest

async def main():
    events = await asyncio.to_thread(mgr.load_chat_events_sync, SID)
    total = len(events)
    cursor = total - 50  # stale-cursor reconnect shape: the client is 50 events behind
    stub = Stub()
    async def run_once():
        gaps = []
        stop = False
        async def ticker():
            prev = time.perf_counter()
            while not stop:
                await asyncio.sleep(0.005)
                now = time.perf_counter()
                gaps.append(now - prev)
                prev = now
        t = asyncio.create_task(ticker())
        t0 = time.perf_counter()
        sent = await _replay_aggregated_catchup(stub, events, cursor, SID, event_index_offset=0)
        wall = time.perf_counter() - t0
        stop = True
        await t
        return sent, (max(gaps) if gaps else wall), wall
    await run_once()  # cold pass, as at the first stale-cursor reconnect after a server start; not timed
    results = []
    for _ in range(5):
        results.append(await run_once())
    walls = sorted(r[2] for r in results)
    gaps = sorted(r[1] for r in results)
    stub.frames = [json.loads(t) for t in stub.raw]
    digest = hashlib.sha256(json.dumps(stub.frames, sort_keys=True, default=str).encode()).hexdigest()[:12]
    print(f"{total} events, cursor {cursor}, {results[0][0]} frames replayed, digest {digest}; "
          f"replay wall median {walls[2]:.4f} s, max {walls[-1]:.4f} s; "
          f"loop-lag median {gaps[2]:.4f} s, max {gaps[-1]:.4f} s")

asyncio.run(main())
shutil.rmtree(home)
EOF
```

M46 — cron tasks list payload and handler time, steady state. Every grouped
sidebar render pairs ``GET /api/cron/tasks`` with ``GET /api/sessions/scheduled``
(the project-manager refresh fires on every list render), and the pre-fix route
shipped every task's resolved prompt body — ~90 KB of the 96 KB live response,
content only the in-process scheduler/master reads (the UI edits
``prompt_file``) — plus the client pays a same-size JSON.parse per render. The
fixed dump excludes ``prompt``, mirroring the POST/PUT responses which never
carried it. The cost rides the sidebar render path, so the collector drives the
endpoint raw-ASGI — the served path the middleware and route actually run; a
TestClient drive adds ~1.5 ms of httpx harness per request and skips the gzip
middleware whose deflate the browser's fetch always pays (the vacuous-read
class the M44/M56 repair called out) — over the live cron corpus (read-only:
``get_scheduled_tasks`` serves the fingerprint-cached snapshot; nothing is
written), one cold pass, as at first sidebar render after a server start, then
nine timed requests, with a parsed-body digest so a corpus change between arms
cannot masquerade as a payload difference (a digest changing between repeats is
the live config's own churn — re-run rather than compare noise, the M44
guard's shape). Evidence while the live server runs older code points the same
collector at the branch checkout (``CHECKOUT`` at the worktree root), the same
shape as the M44 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, gzip, hashlib, json, os, sys, time
sys.path.insert(0, os.environ["CHECKOUT"])
from fastapi import FastAPI
from server import _CharlieBotGZipMiddleware
from src.api.cron import router as cron_router

# Live cron corpus read-only: get_scheduled_tasks resolves the process config's
# fingerprint-cached snapshot; nothing here writes. The production middleware
# chain: the browser's fetch always sends Accept-Encoding: gzip, so the body's
# deflate is part of the served shape — a bare app reads the handler floor alone.
app = FastAPI()
app.include_router(cron_router, prefix="/api/cron")
app.add_middleware(_CharlieBotGZipMiddleware, minimum_size=1000, compresslevel=1)
url = "/api/cron/tasks"
SCOPE = {
    "type": "http", "asgi": {"version": "3.0", "spec_version": "2.3"},
    "http_version": "1.1", "method": "GET", "scheme": "http",
    "path": url, "raw_path": url.encode(), "query_string": b"", "root_path": "",
    "headers": [(b"host", b"test"), (b"accept-encoding", b"gzip")],
    "client": ("test", 123), "server": ("test", 80),
}


async def drive():
    body = b""
    out = {"status": 0, "encoding": b""}

    async def receive():
        return {"type": "http.request", "body": b"", "more_body": False}

    async def send(msg):
        nonlocal body
        if msg["type"] == "http.response.start":
            out["status"] = msg["status"]
            out["encoding"] = dict(msg.get("headers", [])).get(b"content-encoding", b"")
        elif msg["type"] == "http.response.body":
            body += msg.get("body", b"")

    t0 = time.perf_counter()
    await app(SCOPE, receive, send)
    return time.perf_counter() - t0, body, out


_, cold_body, cold_out = asyncio.run(drive())  # cold pass, as at first sidebar render after a server start; not timed
assert cold_out["status"] == 200, cold_out["status"]
times = []
bodies = set()
digests = set()
for _ in range(9):
    dt, body, out = asyncio.run(drive())
    times.append(dt)
    bodies.add(len(body))
    decoded = gzip.decompress(body) if out["encoding"] == b"gzip" else body
    digests.add(hashlib.sha256(json.dumps(json.loads(decoded), sort_keys=True).encode()).hexdigest()[:12])
times.sort()
if len(bodies) != 1 or len(digests) != 1:
    raise SystemExit("live churn during measurement; re-run")
rows = json.loads(decoded)
prompt_bytes = sum(len(row.get("prompt") or "") for row in rows)
step_prompt_bytes = sum(len(s.get("prompt") or "") for row in rows for s in (row.get("steps") or []))
print(f"{len(rows)} task rows, wire {len(body)} B, decoded {len(decoded)} B, digest {digests.pop()}, "
      f"prompt bytes {prompt_bytes}, step prompt bytes {step_prompt_bytes}; "
      f"served GET /api/cron/tasks median {times[4]*1000:.2f} ms, max {times[-1]*1000:.2f} ms over 9")
EOF
```

M47 — claude declared-window warning stream, steady state. Every claude-session
usage resolution re-derives the headless declared window from the environment
(`_resolve_claude_tier` → `headless_claude_declared_window`), and while a
forwarded-but-unmodelled override is exported (this host sets
`CLAUDE_CODE_MAX_CONTEXT_TOKENS=400000`) each resolution re-fires the same
degradation warning — 62 lines in the 7.89 h live server log sampled 2026-09-04
(~8/h) — while one sighting per process carries the whole signal: the
environment the warning reports cannot change between resolutions. The cost is
background log volume invisible to HTTP probes, so the collector drives the
resolution directly with the override exported: one first-sighting call, as at
a process start, then 60 steady-state repeat calls, counting the event. A
warning in the repeat window is a re-fired alarm; the count is the metric.
Evidence while the live server runs older code points the same collector at the
branch checkout (`sys.path.insert` at the worktree root), the same shape as the
M22 protocol:

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import os, sys
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
os.environ["CLAUDE_CODE_MAX_CONTEXT_TOKENS"] = "400000"
from src.agents.backends import claude_code as claude_code_mod

warns = []
orig = claude_code_mod.log.warning
claude_code_mod.log.warning = lambda event, **kw: warns.append({"event": event, **kw})
try:
    claude_code_mod.headless_claude_declared_window()  # first sighting, as at a process start; not counted
    warns.clear()
    for _ in range(60):  # steady-state repeat resolutions of the usage path
        claude_code_mod.headless_claude_declared_window()
finally:
    claude_code_mod.log.warning = orig
n = sum(1 for w in warns if w["event"] == "claude_declared_window_degraded")
print(f"60 steady-state declared-window resolutions; claude_declared_window_degraded warnings: {n}")
EOF
```

M48 — search content-scan missing-file debug stream, steady state. Every
sidebar search (`GET /api/sessions/search?q=…`) content-scans the active
sessions whose names miss the query, and the scan's stat of a session whose
live chat file cannot be read (a fresh session's data/ stays empty until its
first event lands) logs `search_read_failed` on every request — 30 lines in
the 11.92 h live server log sampled 2026-09-04, all naming the one such
session — while one sighting per (session, error) carries the whole signal:
a repeat round that sees the same failure re-fires a fired alarm. The cost
is background log volume invisible to HTTP probes, so the collector creates
one fresh session in a scratch `CHARLIEBOT_HOME` and drives `search_sessions`
with an absent needle: one first-sighting scan, as at a process start, then
60 steady-state repeat scans, counting the event. A line in the repeat
window is a re-fired alarm; the count is the metric. Evidence while the live
server runs older code points the same collector at the branch checkout
(`sys.path.insert` at the worktree root), the same shape as the M22
protocol:

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, sys, tempfile
from pathlib import Path
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from src.core import sessions as sessions_mod
from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest
from src.core.sessions import SessionManager

# Corpus shape: one fresh active session whose data/ holds no live chat file
# (a scheduled-session creation carries no events until its first turn).
work = Path(tempfile.mkdtemp(prefix="m48-search-scan-"))
# A session's default backend resolves from backends.options (empty by
# default since the sectioned config), so the scratch config carries one.
cfg = CharlieBotConfig(charliebot_home=work / "home",
                       backends={"options": [{"id": "m48", "label": "M48", "type": "cc-claude",
                                              "model": "claude-opus-4-6"}]})
mgr = SessionManager(cfg)
asyncio.run(mgr.create_session(CreateSessionRequest(name="M48")))

events = []
orig = sessions_mod.log.debug
sessions_mod.log.debug = lambda event, **kw: events.append(event)
try:
    asyncio.run(mgr.search_sessions("zzq48neverpresent"))  # first sighting, as at a process start; not counted
    events.clear()
    for _ in range(60):  # steady-state repeat content scans of the sidebar search
        asyncio.run(mgr.search_sessions("zzq48neverpresent"))
finally:
    sessions_mod.log.debug = orig
n = sum(1 for event in events if event == "search_read_failed")
print(f"60 steady-state content scans of a no-live-file session; search_read_failed debug lines: {n}")
EOF
```

M49 — opencode part-unhandled debug stream, steady state. Every opencode SSE
`message.part.updated` frame routes through `_translate_part`, and a part type
the translator does not map logs `opencode_part_unhandled` per part — 152 lines
in the 12.88 h live server log sampled 2026-09-04 (~12/h), every one of them
`type=patch`, so a stream of unhandled parts re-fires the same line forever
while one sighting per part type carries the whole signal: the set of mapped
types is code, fixed for the process. The cost is background log volume
invisible to HTTP probes, so the collector drives `_translate_part` on a
synthetic unhandled part: one first-sighting call, as at a process start, then
60 steady-state repeat calls, counting the event. A line in the repeat window
is a re-fired alarm; the count is the metric. Evidence while the live server
runs older code points the same collector at the branch checkout
(`sys.path.insert` at the worktree root), the same shape as the M22 protocol:

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import sys
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from src.agents.backends import opencode as opencode_mod

backend = opencode_mod.OpenCodeBackend()
part = {"id": "m49-p1", "messageID": "m49-m1", "type": "patch"}

events = []
orig = opencode_mod.log.debug
opencode_mod.log.debug = lambda event, **kw: events.append(event)
try:
    backend._translate_part(part)  # first sighting, as at a process start; not counted
    events.clear()
    for _ in range(60):  # steady-state repeat parts of the same unhandled type
        backend._translate_part(part)
finally:
    opencode_mod.log.debug = orig
n = sum(1 for event in events if event == "opencode_part_unhandled")
print(f"60 steady-state unhandled patch parts; opencode_part_unhandled debug lines: {n}")
EOF
```

M50 — ext-usage credentials read warning stream, steady state. Every
claude-account poll round re-reads the account's credentials file
(`ClaudeUsageProvider.fetch` → `_read_credentials`), and while the file is
missing or carries no access token each round re-fires the same warning —
135 lines in the 13.89 h live server log sampled 2026-09-04 (~10/h), every
one of them `ext_usage_no_access_token` naming the same path — while one
sighting per (event, path) per broken streak carries the whole signal: the
file is re-read every round, so a recovery (a read that returns a token)
re-arms the path and a later relapse is a new onset, earning one new line.
The cost is background log volume invisible to HTTP probes, so the collector
drives `_read_credentials` over a synthetic tokenless credentials file: one
first-sighting read, as at a process start, then 60 steady-state repeat
reads, counting the event. A warning in the repeat window is a re-fired
alarm; the count is the metric. Evidence while the live server runs older
code points the same collector at the branch checkout (`sys.path.insert` at
the worktree root), the same shape as the M22 protocol:

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import json, sys, tempfile
from pathlib import Path
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from src.api import ext_usage as ext_usage_mod

work = Path(tempfile.mkdtemp(prefix="m50-cred-read-"))
creds = work / ".credentials.json"
creds.write_text(json.dumps({"claudeAiOauth": {"accessToken": "", "refreshToken": "r"}}))

warns = []
orig = ext_usage_mod.log.warning
ext_usage_mod.log.warning = lambda event, **kw: warns.append(event)
try:
    ext_usage_mod._read_credentials(creds)  # first sighting, as at a process start; not counted
    warns.clear()
    for _ in range(60):  # steady-state repeat reads of the poller's fetch rounds
        ext_usage_mod._read_credentials(creds)
finally:
    ext_usage_mod.log.warning = orig
n = sum(1 for event in warns if event == "ext_usage_no_access_token")
print(f"60 steady-state credential reads of a tokenless file; ext_usage_no_access_token warnings: {n}")
EOF
```

M51 — sidebar dirty-session deep probe, post-write. The 3 s status poll deep-probes a
session whenever its stat-only probe-input signature moved — which is every poll that
follows any write to the session (session/thread metadata writes mark it dirty), i.e.
continuously during an active turn. The deep probe's read path re-enters
`iter_recent_thread_metas`, which read and re-parsed every in-window (30-day) thread
metadata file per probe. The collector copies the session whose threads dir carries the
most metadata files (the M5 resolution rule) into a scratch `CHARLIEBOT_HOME` under /tmp
(live home read once for the copy, never written), warms the probe as a server start
does, then drives the post-write poll's shape: one atomic metadata rewrite (the
tmp-file rename every thread-metadata writer performs) dirties the signature, and the
sweep that follows is the deep probe — one cold pass, then five timed post-write sweeps,
victim rotation resolved once so the pick itself stays out of the timing. Evidence while
the live server runs older code points the same collector at the branch checkout
(`CHECKOUT` at the worktree root), the same shape as the M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import os, shutil, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core import sidebar_state
from src.core.sessions import probe_sidebar_state_sync, selective_probe_sidebar_state

# Worst deep-probe corpus: the session whose threads dir carries the most
# metadata files (the M5 resolution rule).
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    t = d / "threads"
    if t.is_dir():
        n = sum(1 for p in t.iterdir() if (p / "metadata.json").is_file())
        if n > best_n:
            best, best_n = d, n
SID = best.name
print(f"worst deep-probe corpus: session {SID}, {best_n} thread metadata files")

# Isolation: scratch CHARLIEBOT_HOME under /tmp holding only a copy of that
# session's threads/; live home read once for the copy, never written.
home = Path(tempfile.mkdtemp(prefix="m51-deep-probe-", dir="/tmp"))
dst = home / "sessions" / SID / "threads"
dst.parent.mkdir(parents=True)
shutil.copytree(best / "threads", dst)
spec = (SID, dst, home / "sessions" / SID / "triggers", home / "sessions" / SID / "plans.json")

sidebar_state.reset_for_tests()
probe_sidebar_state_sync([spec])  # cold pass, as at a server start; not timed

# Victim rotation, resolved once: each post-write sweep renames the next
# metadata file. (Picking the newest victim per round with a Path.glob would
# measure the pick, not the probe.)
victims = sorted(dst.glob("*/metadata.json"), key=lambda p: p.stat().st_mtime_ns)

def post_write_sweep() -> None:
    # One thread-metadata write dirties the session: the tmp inode's fresh
    # mtime_ns rides the atomic rename, the stat-only signature moves, and the
    # next poll's sweep deep-probes.
    victim = victims[post_write_sweep.i % len(victims)]
    post_write_sweep.i += 1
    tmp = victim.with_name("metadata.json.m51")
    tmp.write_text(victim.read_text(encoding="utf-8"), encoding="utf-8")
    os.replace(tmp, victim)
    entries, sigs = selective_probe_sidebar_state([spec], deep=False)
    assert entries, "deep probe returned no entry"
    for sid, sig in sigs.items():
        sidebar_state.store_probe_signature(sid, sig)  # the poll's on-loop storage half

post_write_sweep.i = 0
post_write_sweep()  # first post-write poll, as after any write; not timed
times = []
for _ in range(5):
    t0 = time.perf_counter()
    post_write_sweep()
    times.append(time.perf_counter() - t0)
times.sort()
print(f"{best_n} thread metadata files; post-write deep probe median "
      f"{times[2] * 1000:.2f} ms, max {times[-1] * 1000:.2f} ms")
shutil.rmtree(home)
EOF
```

M52 — chat-event append, per event. Every streamed turn appends one chat event per
delta through `save_chat_event` (`persist_and_broadcast` awaits it before each
broadcast), and every worker event lands through the same `append_ndjson` funnel;
the append is O(1) disk work but its per-call overhead rides the delta path. The
cost is write-side thread-pool time invisible to the read-side standing rows, so
the collector copies the session whose live chat file carries the most events into
a scratch `CHARLIEBOT_HOME` under /tmp (metadata.json and data/ only; live home
read once for the copy, never written), warms the events cache as a live streamed
turn does, and times 50 `save_chat_event` appends of one probe event, asserting
the appended lines parse back from disk in order. Evidence while the live server
runs older code points the same collector at the branch checkout (`CHECKOUT` at
the worktree root), the same shape as the M26 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, json, os, shutil, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager

# Worst append corpus: the session whose LIVE chat file carries the most
# events; a streamed turn appends one event per delta to exactly this file.
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    p = d / "data" / "chat_events.jsonl"
    if p.is_file():
        with open(p, errors="replace") as f:
            n = sum(1 for _ in f)
        if n > best_n:
            best, best_n = d, n
SID = best.name

# Isolation: scratch CHARLIEBOT_HOME under /tmp holding only a copy of that
# session's metadata.json and data/; live home read once for the copy, never written.
home = Path(tempfile.mkdtemp(prefix="m52-append-home-", dir="/tmp"))
dst = home / "sessions" / SID
dst.mkdir(parents=True)
shutil.copy2(best / "metadata.json", dst / "metadata.json")
shutil.copytree(best / "data", dst / "data")

cfg = CharlieBotConfig(charliebot_home=home)
mgr = SessionManager(cfg)
mgr.load_chat_events_sync(SID)  # warm the events cache, as a live streamed turn does

probe_base = {"type": "assistant", "message": {"content": [{"type": "text", "text": "m52 probe chunk " + "y" * 400}]}}

async def main():
    times = []
    for i in range(50):
        ev = {**probe_base, "id": f"m52-probe-{i}", "timestamp": f"2026-09-04T19:01:{i:02d}Z"}
        t0 = time.perf_counter()
        await mgr.save_chat_event(SID, ev)
        times.append(time.perf_counter() - t0)
    times.sort()
    cached = mgr.load_chat_events_sync(SID)
    with open(home / "sessions" / SID / "data" / "chat_events.jsonl", "rb") as f:
        lines = f.read().split(b"\n")[:-1]
    on_disk = [json.loads(line) for line in lines[best_n:]]
    ids = [e["id"] for e in on_disk]
    parity = ids == [f"m52-probe-{i}" for i in range(50)] and len(cached) == best_n + 50
    print(f"{best_n}-event corpus; save_chat_event append median {times[24] * 1e6:.0f} us, "
          f"max {times[-1] * 1e6:.0f} us over 50; parity {parity}")
    shutil.rmtree(home)

asyncio.run(main())
EOF
```

M53 — config reload failure re-fire, broken steady state. `get_config` is the
per-request config read (the auth middleware calls it on every HTTP request,
the scheduler on every tick), and while the corpus stays broken a failed
reload re-ran the full YAML parse + model validation and re-fired
`config_reload_failed` on every call — the pre-fix form never recorded the
failed reload's fingerprint, so the reload condition never went false (the live
burst: 4431 lines in a 24.9 h server log, ~1/s inside the 16:00-18:00 window
of 2026-09-04, three distinct error strings). The fixed form memoizes the
failed reload on its fingerprint — re-parse only when a file moves, the same
freshness rule the success path follows — and routes the warning through a
warn-once registry, one line per error string per process, cleared on a
successful load so a relapse earns one new line. The cost is per-request work
invisible to the standing probes while the corpus is broken (a state the live
host entered for a 2 h window), so the collector seeds a good config in a
scratch `CHARLIEBOT_HOME`, breaks it with a key the model does not declare
(the burst's error shape), and drives `get_config`:
one onset pass, as at the first call after the corpus breaks, then 60 timed
steady-state calls asserting the served instance's identity, then one
fingerprint-move round asserting the freshness survived. Evidence points the
same collector at the before and after checkouts (`CHECKOUT` at each root),
the same shape as the M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import os, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core import config as core_config

# Broken-config corpus: config.yaml carrying one key the model does not
# declare — the live burst's error shape (unknown config key(s) ...). The key
# goes into config.yaml itself: the reload fingerprint stats exactly that file
# (mtime, size), and since the sectioned config config.d/ holds only cron.d/,
# a fragment there is rejected outright and can never move the fingerprint.
# Scratch CHARLIEBOT_HOME; the live home is never read or written here.
work = Path(tempfile.mkdtemp(prefix="m53-reload-"))
home = work / "home"
(home / "config.d").mkdir(parents=True)
(home / "config.yaml").write_text("", encoding="utf-8")
os.environ["CHARLIEBOT_HOME"] = str(home)

core_config._config_cache.reset()
cached = core_config.get_config()  # seed: the running server's last-good config; not timed

(home / "config.yaml").write_text("unknown_m53_key: 1\n", encoding="utf-8")

warns = []
parses = []
orig_warn = core_config.log.warning
orig_load = core_config.load_config
core_config.log.warning = lambda event, **kw: warns.append({"event": event, **kw})
core_config.load_config = lambda: (parses.append(1), orig_load())[1]
try:
    core_config.get_config()  # onset pass, as at the first call after the corpus breaks; not timed
    onset_warns, onset_parses = len(warns), len(parses)
    times = []
    for _ in range(60):  # steady-state repeat calls of the per-request auth path
        t0 = time.perf_counter()
        got = core_config.get_config()
        times.append(time.perf_counter() - t0)
        assert got is cached, "served config identity changed across a broken steady state"
    steady_warns, steady_parses = len(warns), len(parses)
    os.utime(home / "config.yaml", (0, 0))  # fingerprint move: freshness must survive
    core_config.get_config()
    moved_warns, moved_parses = len(warns) - steady_warns, len(parses) - steady_parses
finally:
    core_config.log.warning = orig_warn
    core_config.load_config = orig_load
times.sort()
print(f"60 steady-state get_config calls of a persistently-broken corpus; config_reload_failed warnings "
      f"{steady_warns} (onset {onset_warns}), re-parses {steady_parses} (onset {onset_parses}); "
      f"call wall median {times[30] * 1000:.2f} ms, max {times[-1] * 1000:.2f} ms; "
      f"fingerprint-move round: re-parse {moved_parses}, new warnings {moved_warns}")
EOF
```

M54 — stream-draft paint work with the page's real highlight.js build. The
M33 harness stubs hljs, so the standing replay metric never sees
highlightAuto's cost: with the served 11.9.0 common build (36 languages),
highlightAuto scores the block against every language (~0.26 s per 24 KB
measured), and every streaming paint re-parses the whole draft, re-running it
on every unchanged code block in the draft. The collector loads the pinned
hljs build into the same harness and replays the largest on-disk assistant
draft containing a bare code fence — the corpus shape whose paint cost
highlightAuto dominates. Evidence points the collector at the before and
after checkouts (`CHECKOUT` at each root, live state read-only), the same
shape as the M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} node /home/chaoli/workspace/charlie-bot/tests/stream_hl_render_collector.js
```

M55 — artifact compare-view serve, steady state. The plan panel's Compare with previous
toggle and every artifact `?diff=` link run the file server's annotate path:
the pre-fix handler read both pages and ran `plan_diff.annotate` inline on the
event loop per request — ~0.25 s of loop freeze per compare view of a 1 MB
pair (the M14 pathology), re-computing an immutable result on every repeat
view (the toggle flip, a plan update re-render, a refresh). The fixed handler
builds the page in one thread hop and memoizes it on both files' (path,
mtime_ns, size) signatures — the marks are a pure
function of the two files' bytes and artifact pages are only ever written
whole, so a repeat view re-runs zero annotate. The compare view's repeat also
ships its pre-compressed gzip form from a memo beside the plain one, the M70
mechanism: one off-loop deflate per distinct annotated body replaces the
middleware's per-request pass, Content-Encoding set upstream making the
middleware skip. The cost is a per-click latency
no standing probe covers, so the collector snapshots the worst artifact pair
(the session whose artifacts dir carries the most .html bytes; target =
biggest page, base = runner-up) into a scratch `CHARLIEBOT_HOME` under /tmp
(live home read once for the copy, never written) and drives the route raw-ASGI
— the served path the middleware and route actually run, the production gzip
middleware mounted and the browser's Accept-Encoding shape — in each checkout's
process: one cold pass, as at first compare-view
open, then nine timed repeats, asserting byte-identical wire bodies, with a
concurrent 5 ms ticker reporting the request's worst event-loop gap. The cold pass
is the first-view sub-metric (the annotate the repeat memo serves from); the
nine timed repeats are the steady state. Evidence
points the same collector at the before and after checkouts (``CHECKOUT`` at
each root, shared snapshot home), the same shape as the M35 protocol.
Snapshot once:

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import shutil, tempfile
from pathlib import Path

# Worst artifact-pair corpus: the session whose artifacts dir carries the most
# .html bytes; target = biggest page, base = runner-up (the compare-view pair).
root = Path.home() / ".charliebot" / "sessions"
best, best_n, target, base = None, -1, None, None
for d in root.iterdir():
    art = d / "artifacts"
    if not art.is_dir():
        continue
    pages = sorted(art.glob("*.html"), key=lambda p: p.stat().st_size, reverse=True)
    if len(pages) < 2:
        continue
    n = sum(p.stat().st_size for p in pages)
    if n > best_n:
        best, best_n = d, n
        target, base = pages[0].name, pages[1].name
print(f"worst artifact pair: session {best.name}, {best_n / 1e6:.1f} MB html "
      f"({target} vs {base})")

# Isolation: scratch CHARLIEBOT_HOME under /tmp holding only the copied pair;
# live home read once for the copy, never written.
home = Path(tempfile.mkdtemp(prefix="m55-artifact-home-", dir="/tmp"))
dst = home / "sessions" / best.name / "artifacts"
dst.mkdir(parents=True)
shutil.copy2(best / "artifacts" / target, dst / target)
shutil.copy2(best / "artifacts" / base, dst / base)
print(f"export M55_HOME={home} M55_SID={best.name} M55_TARGET={target} M55_BASE={base}")
EOF
```

Then run per checkout (``eval`` the snapshot export first):

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, shutil, gzip, hashlib, os, sys, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from fastapi import FastAPI
from src.api.files import router as files_router
from src.core.config import CharlieBotConfig
import src.api.files as files_mod
from server import _CharlieBotGZipMiddleware

home = Path(os.environ["M55_HOME"])
SID = os.environ["M55_SID"]
TARGET = os.environ["M55_TARGET"]
BASE = os.environ["M55_BASE"]

# Scratch wiring: the router's get_config resolves the snapshot home, never the
# live one; the tray injects unconditionally (the auth middleware owns the
# access gate and this harness mounts none), so the timed request carries the
# artifact-comments injection like a real view.
cfg = CharlieBotConfig(charliebot_home=home)
files_mod.get_config = lambda: cfg
app = FastAPI()
app.include_router(files_router, prefix="/absolute_filepath")
# The production middleware chain: every served response passes the whole-body
# gzip whose deflate is part of the view's cost.
app.add_middleware(_CharlieBotGZipMiddleware, minimum_size=1000, compresslevel=1)
# The file server addresses pages by absolute filesystem path under the one
# canonical /absolute_filepath prefix (the /files alias is unmounted); ?diff=
# stays session-relative.
PATH = f"/absolute_filepath/{home}/sessions/{SID}/artifacts/{TARGET}"
QS = f"diff=artifacts/{BASE}".encode()
HEADERS = [(b"host", b"test"), (b"accept-encoding", b"gzip")]  # the browser shape
SCOPE = {
    "type": "http", "asgi": {"version": "3.0", "spec_version": "2.3"},
    "http_version": "1.1", "method": "GET", "scheme": "http",
    "path": PATH, "raw_path": (PATH + "?" + QS.decode()).encode(),
    "query_string": QS,
    "root_path": "", "headers": HEADERS,
    "client": ("test", 123), "server": ("test", 80),
}


async def drive():
    body = b""
    encoding = b""

    async def receive():
        return {"type": "http.request", "body": b"", "more_body": False}

    async def send(msg):
        nonlocal body, encoding
        if msg["type"] == "http.response.start":
            encoding = dict(msg.get("headers", [])).get(b"content-encoding", b"")
        elif msg["type"] == "http.response.body":
            body += msg.get("body", b"")

    t0 = time.perf_counter()
    await app(SCOPE, receive, send)
    return time.perf_counter() - t0, body, encoding


async def main():
    cold, body, encoding = await drive()  # cold pass, as at first compare-view open; not timed
    assert encoding == b"gzip", f"browser shape served without gzip: {encoding!r}"
    assert "This version · vs previous" in gzip.decompress(body).decode("utf-8", "replace"), \
        "annotated page missing the compare header"
    worst, walls = [], []
    bodies = set()
    digest = ""
    for _ in range(9):
        stop = False
        gaps = []

        async def ticker():
            prev = time.perf_counter()
            while not stop:
                await asyncio.sleep(0.005)
                now = time.perf_counter()
                gaps.append(now - prev)
                prev = now

        t = asyncio.create_task(ticker())
        dt, body, _ = await drive()
        stop = True
        await t
        worst.append(max(gaps) if gaps else dt)
        walls.append(dt)
        bodies.add(len(body))
        digest = hashlib.sha256(body).hexdigest()[:12]
    worst.sort()
    walls.sort()
    assert len(bodies) == 1, f"repeat bodies differ: {bodies}"
    print(f"{TARGET} vs {BASE}; first view {cold:.4f} s; repeat-view median {walls[4]:.4f} s, "
          f"max {walls[-1]:.4f} s over 9, gzip wire {bodies.pop()} B, digest {digest}; "
          f"loop-lag median {worst[4]:.4f} s, max {worst[-1]:.4f} s")


try:
    asyncio.run(main())
finally:
    shutil.rmtree(home)  # every exit path removes the scratch copy: the hourly cadence leaks one copy per skipped removal
EOF
```

M56 — sidebar status poll, steady state. The sidebar polls `GET /api/sessions/status?ids=…` every
3 s per open dashboard tab with the sessions it renders (this host's second-busiest route after
the workers-panel list); the handler resolves every id's metadata plus the derived sidebar state
and the pre-fix mapped return paid FastAPI's jsonable_encoder pass over the 41-row dict. The cost
is per-poll latency invisible to the standing HTTP probes (M3 reads the 401 floor), so the
collector drives the endpoint raw-ASGI — the served path the middleware and route actually run; a
TestClient drive adds ~1.5 ms of httpx harness per request and skips the gzip middleware whose
deflate the browser's poll always pays (the vacuous-read class the M57/M70/M72 repairs called
out) — over the live corpus (read-only), ids resolved from the active-session listing, from the
checkout under test: one cold pass, as at first sidebar paint after a server start, then nine
timed requests, with a parsed-body digest so a corpus change between arms cannot masquerade as a
payload difference (a digest changing between repeats is the sidebar's own live churn — the
assert re-runs the round, the M44 guard's shape).

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, gzip, hashlib, json, os, sys, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from fastapi import FastAPI
from server import _CharlieBotGZipMiddleware
from src.api.deps import get_session_manager
from src.api.sessions import router as sessions_router
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager

# The sidebar polls the sessions it renders; the active set is that corpus.
async def ids():
    cfg = CharlieBotConfig(charliebot_home=Path.home() / ".charliebot")
    mgr = SessionManager(cfg)
    metas = await asyncio.to_thread(mgr.list_active_session_metas)
    return ",".join(m.id for m in metas)

IDS = asyncio.run(ids())

cfg = CharlieBotConfig(charliebot_home=Path.home() / ".charliebot")
mgr = SessionManager(cfg)
app = FastAPI()
app.include_router(sessions_router, prefix="/api/sessions")
app.dependency_overrides[get_session_manager] = lambda: mgr
# The production middleware chain: the browser's poll always sends
# Accept-Encoding: gzip, so the body's deflate is part of the served
# shape — a bare app reads the handler floor alone.
app.add_middleware(_CharlieBotGZipMiddleware, minimum_size=1000, compresslevel=1)

QUERY = f"ids={IDS}".encode()

def scope():
    return {
        "type": "http", "asgi": {"version": "3.0", "spec_version": "2.3"},
        "http_version": "1.1", "method": "GET", "scheme": "http",
        "path": "/api/sessions/status", "raw_path": ("/api/sessions/status?" + IDS).encode(),
        "query_string": QUERY, "root_path": "",
        "headers": [(b"host", b"test"), (b"accept-encoding", b"gzip")],
        "client": ("test", 123), "server": ("test", 80),
    }


async def drive():
    body = b""
    out = {"status": 0, "encoding": b""}

    async def receive():
        return {"type": "http.request", "body": b"", "more_body": False}

    async def send(msg):
        nonlocal body
        if msg["type"] == "http.response.start":
            out["status"] = msg["status"]
            out["encoding"] = dict(msg.get("headers", [])).get(b"content-encoding", b"")
        elif msg["type"] == "http.response.body":
            body += msg.get("body", b"")

    t0 = time.perf_counter()
    await app(scope(), receive, send)
    return time.perf_counter() - t0, body, out


def digest(decoded):
    return hashlib.sha256(json.dumps(json.loads(decoded), sort_keys=True).encode()).hexdigest()[:12]


async def main():
    _, _, out = await drive()  # cold pass, as at first sidebar paint after a server start; not timed
    assert out["status"] == 200, out["status"]
    times, wire, decoded_size, digests = [], 0, 0, set()
    for _ in range(9):
        dt, body, out = await drive()
        decoded = gzip.decompress(body) if out["encoding"] == b"gzip" else body
        times.append(dt)
        wire = len(body)
        decoded_size = len(decoded)
        digests.add(digest(decoded))
    times.sort()
    assert len(digests) == 1, f"repeat bodies differ: {digests}"
    print(f"checkout {os.environ['CHECKOUT'].rsplit('/', 1)[-1]}: {len(IDS.split(','))} sidebar ids; "
          f"/status served median {times[4]*1000:.2f} ms, max {times[-1]*1000:.2f} ms, "
          f"wire {wire} B, decoded {decoded_size} B, digest {digests.pop()}")


asyncio.run(main())
EOF
```

M57 — plan-registry poll, steady state. The plan panel polls `GET /api/sessions/{id}/plans` every
3 s while open; M27 memoized the registry read itself (10.6 µs steady state) and the endpoint
renders through FastJsonResponse. The cost is per-poll latency invisible to the standing HTTP
probes, so the collector drives the endpoint raw-ASGI — the served path the middleware and route
actually run; a TestClient drive adds ~1.5 ms of httpx harness per request, the vacuous-read class
the M70/M72 repair called out — over the worst on-disk plans corpus (the session whose plans.json
carries the most bytes, live state read-only), from the checkout under test: one cold pass, as at
first panel paint after a server start, then nine timed requests, with a parsed-body digest.

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, hashlib, json, os, sys, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from fastapi import FastAPI
from src.api.deps import get_plan_manager
from src.api.sessions import router as sessions_router
from src.core.config import CharlieBotConfig
from src.core.plans import PlanRegistryManager
from src.core.sessions import SessionManager

# Worst plans corpus: the session whose plans.json carries the most bytes.
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    p = d / "plans.json"
    if p.is_file():
        n = p.stat().st_size
        if n > best_n:
            best, best_n = p, n
SID = best.parent.name

cfg = CharlieBotConfig(charliebot_home=Path.home() / ".charliebot")
mgr = SessionManager(cfg)
plan_mgr = PlanRegistryManager(cfg, mgr)
app = FastAPI()
app.include_router(sessions_router, prefix="/api/sessions")
app.dependency_overrides[get_plan_manager] = lambda: plan_mgr
url = f"/api/sessions/{SID}/plans"
SCOPE = {
    "type": "http", "asgi": {"version": "3.0", "spec_version": "2.3"},
    "http_version": "1.1", "method": "GET", "scheme": "http",
    "path": url, "raw_path": url.encode(), "query_string": b"", "root_path": "",
    "headers": [(b"host", b"test")], "client": ("test", 123), "server": ("test", 80),
}

def digest(body):
    return hashlib.sha256(json.dumps(json.loads(body), sort_keys=True).encode()).hexdigest()[:12]

async def drive():
    body = b""

    async def receive():
        return {"type": "http.request", "body": b"", "more_body": False}

    async def send(msg):
        nonlocal body
        if msg["type"] == "http.response.body":
            body += msg.get("body", b"")

    t0 = time.perf_counter()
    await app(SCOPE, receive, send)
    return time.perf_counter() - t0, body


async def main():
    cold, body = await drive()  # cold pass, as at first panel paint after a server start; not timed
    times, bodies, digests = [], set(), set()
    for _ in range(9):
        dt, body = await drive()
        times.append(dt)
        bodies.add(len(body))
        digests.add(digest(body))
    times.sort()
    assert len(bodies) == 1 and len(digests) == 1, f"repeat bodies differ: {bodies} {digests}"
    print(f"checkout {os.environ['CHECKOUT'].rsplit('/', 1)[-1]}: session {SID}, {best_n / 1e3:.1f} KB plans.json; "
          f"first view {cold * 1000:.2f} ms; /plans request median {times[4] * 1000:.2f} ms, max {times[-1] * 1000:.2f} ms over 9, "
          f"body {bodies.pop()} B, digest {digests.pop()}")


asyncio.run(main())
EOF
```

M58 — per-request config read, steady state. `get_config` is the per-request config read (the
auth middleware calls it on every HTTP request, the scheduler on every tick), and its reload
check re-derives the config fingerprint on every call — after M53 the steady state is one
fingerprint walk per call, so that walk is the per-request floor's dominant slice (the raw-ASGI
401 path measured ~150 µs total, ~130 µs of it the pre-fix fingerprint's pathlib machinery).
The cost is per-request latency the HTTP standing probes only see mixed into their totals, so
the collector times `get_config` itself over the live config corpus (read-only: a fresh stat
set per call, never a write), from the checkout under test: one cold pass, as at a server
start, then nine timed calls. The healthy range bounds the fresh-stat walk; a jump toward the
parse-sized costs (the M53 broken-corpus wall) means a fingerprint miss is landing per call.

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import os, sys, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
os.environ.pop("CHARLIEBOT_HOME", None)  # the live profile: ~/.charliebot
from src.core import config as core_config

core_config.get_config()  # cold pass, as at a server start; not timed
times = []
for _ in range(9):
    t0 = time.perf_counter()
    core_config.get_config()
    times.append(time.perf_counter() - t0)
times.sort()
frag = len(os.listdir(Path.home() / ".charliebot" / "config.d"))
print(f"{frag} config.d entries; steady-state get_config "
      f"median {times[4] * 1e6:.1f} us, max {times[-1] * 1e6:.1f} us")
EOF
```

M59 — worker thread-detail poll payload and handler time, steady state. The workers panel polls
`GET /api/threads/{sid}/threads/{tid}` every 5 s per expanded running worker (in the same
`Promise.all` as the M34 events poll) and the pre-fix route served the whole row — description-KB
payload, an uncached aiofiles read+parse per call, and FastAPI's response-model validation plus
jsonable_encoder render — for the client to read two derived fields (`attach_command`,
`attach_available`); the full row remains the description modal's once-per-click fetch. The cost
is a poll slice invisible to the standing HTTP probes, so the collector drives the endpoint
raw-ASGI — the served path the middleware and route actually run; a TestClient drive adds ~1.5 ms
of httpx harness per request and skips the gzip middleware whose deflate the browser's poll always
pays (the vacuous-read class the M57/M70/M72 repairs called out) — over the thread whose
metadata.json carries the most bytes (live state read-only), from the checkout under test: one
cold pass per mode, as at first panel expand after a server start, then nine timed requests of the
full row and nine of the attach-mode pair, with the payload contracts asserted. Evidence while the
live server runs older code points the same collector at the branch checkout (`CHECKOUT` at the
worktree root), the same shape as the M36 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, gzip, hashlib, json, os, sys, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from fastapi import FastAPI
from server import _CharlieBotGZipMiddleware
from src.api.deps import get_thread_manager, get_config
from src.api.threads import router as threads_router
from src.core.config import CharlieBotConfig
from src.core.threads import ThreadManager

# Worst detail-poll corpus: the thread whose metadata.json carries the most
# bytes; the endpoint reads live state read-only. Managers and config are
# built once, as the server's dependency singletons are.
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for p in root.glob("*/threads/*/metadata.json"):
    n = p.stat().st_size
    if n > best_n:
        best, best_n = p, n
SID, TID = best.parts[-4], best.parts[-2]

cfg = CharlieBotConfig(charliebot_home=Path.home() / ".charliebot")
app = FastAPI()
app.include_router(threads_router, prefix="/api/threads")
app.dependency_overrides[get_thread_manager] = lambda: ThreadManager(cfg)
app.dependency_overrides[get_config] = lambda: cfg
# The production middleware chain: the browser's poll always sends
# Accept-Encoding: gzip, so the full row's deflate is part of the served
# shape — a bare app reads the handler floor alone.
app.add_middleware(_CharlieBotGZipMiddleware, minimum_size=1000, compresslevel=1)

def scope(url, query=b""):
    return {
        "type": "http", "asgi": {"version": "3.0", "spec_version": "2.3"},
        "http_version": "1.1", "method": "GET", "scheme": "http",
        "path": url, "raw_path": url.encode(), "query_string": query, "root_path": "",
        "headers": [(b"host", b"test"), (b"accept-encoding", b"gzip")],
        "client": ("test", 123), "server": ("test", 80),
    }


async def drive(url, query=b""):
    body = b""
    out = {"status": 0, "encoding": b""}

    async def receive():
        return {"type": "http.request", "body": b"", "more_body": False}

    async def send(msg):
        nonlocal body
        if msg["type"] == "http.response.start":
            out["status"] = msg["status"]
            out["encoding"] = dict(msg.get("headers", [])).get(b"content-encoding", b"")
        elif msg["type"] == "http.response.body":
            body += msg.get("body", b"")

    t0 = time.perf_counter()
    await app(scope(url, query), receive, send)
    return time.perf_counter() - t0, body, out


def digest(decoded):
    return hashlib.sha256(json.dumps(json.loads(decoded), sort_keys=True).encode()).hexdigest()[:12]


async def main():
    url = f"/api/threads/{SID}/threads/{TID}"

    async def run_mode(query):
        _, _, out = await drive(url, query)  # cold pass, as at first panel expand; not timed
        times, decoded_size, wire_size, digests, last = [], 0, 0, set(), b""
        for _ in range(9):
            dt, body, out = await drive(url, query)
            decoded = gzip.decompress(body) if out["encoding"] == b"gzip" else body
            times.append(dt)
            decoded_size = len(decoded)
            wire_size = len(body)
            digests.add(digest(decoded))
            last = decoded
        times.sort()
        assert len(digests) == 1, f"repeat bodies differ: {digests}"
        return times, decoded_size, wire_size, last, digests.pop()

    ftimes, fdec, fwire, flast, fdigest = await run_mode(b"")
    fjson = json.loads(flast)
    assert "context" not in fjson and fjson["description"], "full-row contract changed"
    atimes, adec, awire, alast, adigest = await run_mode(b"attach=1")
    assert set(json.loads(alast)) == {"attach_command", "attach_available"}, "attach-mode contract changed"
    print(f"checkout {os.environ['CHECKOUT'].rsplit('/', 1)[-1]}: {best_n / 1e3:.1f} KB metadata.json; "
          f"full row median {ftimes[4]*1000:.2f} ms, max {ftimes[-1]*1000:.2f} ms, decoded {fdec} B wire {fwire} B, digest {fdigest}; "
          f"attach mode median {atimes[4]*1000:.2f} ms, max {atimes[-1]*1000:.2f} ms, body {adec} B")

asyncio.run(main())
EOF
```

M60 — chat message-body markdown parse, repeat page render. Every session switch
rebuilds the turn engine, so the same page's message bodies re-run the parse
(marked + fence fix) on every re-entry and every repeat render; the streaming
draft paint keeps its own path (M33/M54). The collector loads the checkout's
markdown-renderer.js with the page's real marked + highlight.js builds, resolves
the worst page corpus (the 40 largest message bodies of the live chat file
carrying the most bytes; live state read-only), and times full page passes — one
cold pass, as at the first render after a page load, then five timed repeats.
The pre-fix form (no `renderProseMarkdown`) re-parses every repeat; the
post-fix form serves them from the memo; both forms are the same command, the
checkout under test decides. The cold first paint reports two slices since the
deferral PR: the deferred parse (the bytes the user sees on first paint, code
blocks escaped-plain) and the highlight flush (the same hljs work carried by the
scheduled flush that swaps settled bytes into the DOM and settles the memo
entries), so the first-paint cost and the deferred work stay visible
separately. Evidence points the same collector at the before
and after checkouts (`CHECKOUT` at each root), the same shape as the M18
protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} node /home/chaoli/workspace/charlie-bot/tests/message_render_collector.js
```

M61 — session-metadata read after TTL expiry, idle-cold. Every dashboard listing (`GET /api/sessions`,
`/starred`, `/archived`, `/scheduled`, search) and every single-session read routes through the
metadata cache, whose entries expire after `_METADATA_CACHE_TTL` (30 s) — the state any of those
requests hits when no tab has polled for 30 s. Archived entries never expire (served regardless of
age), so the idle cost is the non-archived set's revalidation: the pre-fix form re-read and re-parsed
each expired active metadata.json (three aiofiles executor hops per single read, one batched
read+parse per listing), while the fixed form revalidates the entry with one stat against the
(st_mtime_ns, st_size) its read took before parsing — every writer publishes through the atomic tmp
rename, so unchanged bytes always match and only a moved file re-reads. The cost is a per-request
latency no standing probe isolates (the polls keep their own sessions fresh), so the collector warms
every entry as the live server's polls do, then rewinds each entry's timestamp past the TTL before
every timed call (preserving the entry's signature half, whatever the checkout's tuple shape) —
replaying the idle window without waiting it, read-only over the live home. One cold pass, as at the
first read after the idle window, then five timed calls per scenario. Evidence points the same
collector at the before and after checkouts (`CHECKOUT` at each root), the same shape as the M18
protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, os, statistics, sys, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager

TTL = 30.0

def age_cache(mgr: SessionManager) -> None:
    for sid, entry in list(mgr._metadata_cache.items()):
        meta = entry[0]
        sig = entry[2] if len(entry) > 2 else None
        aged = (meta, time.monotonic() - TTL - 1.0, sig) if len(entry) > 2 else (meta, time.monotonic() - TTL - 1.0)
        mgr._metadata_cache[sid] = aged

async def main():
    cfg = CharlieBotConfig(charliebot_home=Path.home() / ".charliebot")
    mgr = SessionManager(cfg)
    await mgr._load_session_metas()  # warm every entry, as the live server's polls do
    n = len(mgr._metadata_cache)
    n_active = sum(1 for entry in mgr._metadata_cache.values() if entry[0].status.value != "archived")

    async def timed(call):
        age_cache(mgr)
        t0 = time.perf_counter()
        await call()
        return time.perf_counter() - t0

    async def archived_page():
        await mgr.list_archived_page(limit=100)

    async def all_sessions():
        await mgr.list_sessions(status=None, scheduled=False, include_running_status=True,
                                include_pending_trigger_status=True, include_pending_plan_approval=True)

    async def single_get():
        sid = next(sid for sid, entry in mgr._metadata_cache.items() if entry[0].status.value != "archived")
        await mgr.get_session(sid)

    async def bare_listing():
        await mgr._load_session_metas()

    scenarios = {"archived-page": archived_page, "all-sessions": all_sessions,
                 "single-get": single_get, "bare-listing": bare_listing}
    for call in scenarios.values():
        age_cache(mgr)
        await call()  # cold pass, as at the first read after the idle window; not timed
    med, mx = {}, {}
    for name, call in scenarios.items():
        times = [await timed(call) for _ in range(5)]
        med[name] = statistics.median(times)
        mx[name] = max(times)
    print(f"checkout {os.environ['CHECKOUT'].rsplit('/', 1)[-1]}: {n} cached metas ({n_active} non-archived); idle-cold "
          f"archived-page median {med['archived-page']*1000:.2f} ms (max {mx['archived-page']*1000:.2f}), "
          f"all-sessions median {med['all-sessions']*1000:.2f} ms (max {mx['all-sessions']*1000:.2f}), "
          f"single get_session median {med['single-get']*1000:.3f} ms (max {mx['single-get']*1000:.3f}), "
          f"bare listing median {med['bare-listing']*1000:.2f} ms (max {mx['bare-listing']*1000:.2f})")

asyncio.run(main())
EOF
```

M62 — spawn base-resolution chain, base-less launch. An unattended launch resolves its
worktree base from the remote itself: the default branch (HEAD symref) plus that branch's
published tip, then the start point. The pre-fix chain asked the remote three times in
series — a filtered symref ls-remote, a second filtered ls-remote for the same branch, and
an unconditional fetch — while one unfiltered listing answers both probes and the probe's
own tip SHA proves the fetch a no-op whenever the remote-tracking ref is current. The cost
sits on every worker spawn's pre-process latency (delegations, improve-loop iterations,
cron launches), invisible to the standing HTTP probes, so the collector drives the
resolution chain over the real workspace repo (read-only git ops; resolve fetches only
when its probe shows the tracking ref behind), from the checkout under test: one warm
pass, as at the first spawn after a server start, then five timed rounds, asserting the
start point is round-stable. Evidence while the live server runs older code points the
same collector at the branch checkout (`CHECKOUT` at the worktree root), the same shape as
the M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, os, sys, time
from pathlib import Path

sys.path.insert(0, os.environ["CHECKOUT"])
from src.core import git as git_mod

REPO = Path("/home/chaoli/workspace/charlie-bot")

async def run_once() -> tuple[float, str]:
    t0 = time.perf_counter()
    branch, tip = await git_mod.git_remote_default_branch_and_tip(REPO)
    resolution = await git_mod.resolve_base_branch(REPO, f"origin/{branch}", remote_tip=tip)
    return time.perf_counter() - t0, resolution.start_point

async def main():
    await run_once()  # warm pass, as at the first spawn after a server start; not timed
    results = [await run_once() for _ in range(5)]
    walls = sorted(r[0] for r in results)
    starts = {r[1] for r in results}
    assert len(starts) == 1, f"start point moved between rounds: {starts}"
    print(f"base-less base-resolution chain median {walls[2]:.4f} s, max {walls[-1]:.4f} s over 5; "
          f"start_point {starts.pop()[:12]}")

asyncio.run(main())
EOF
```

M63 — session view thread payload, worst on-disk threads corpus. Every SPA switch and
session open fetches `GET /api/sessions/{id}/view`, whose `threads` array rode as whole
`ThreadMetadata` dumps — task-spec-length descriptions included, ~7.8 KB per row at the
worst corpus — while the workers tab it feeds paints one CSS-truncated description line
per card and its full-text modal fetches the thread row on click (the M36 list contract,
which the same card builder already consumes). The fix ships the M36 prefixed rows, so
the view body carries one prefix per thread instead of the whole metadata. The view is a
side-effect-free read — the old write-once mark_read moved to the client's post-render
`POST /read` — so the scratch-home copy is pure read-only corpus isolation rather than
write avoidance: the collector resolves the session whose threads directory carries the
most metadata files (the M5 resolution rule), copies
that session (metadata.json, data/, threads/) into a scratch `CHARLIEBOT_HOME` under /tmp
(live home read once for the copy, never written), and times the handler function from the
checkout under test: one cold pass, as at first view after a server start, then nine timed
calls. The request seam carries no Accept-Encoding header — the no-gzip client shape — so
the timed call reads the handler's plain-path work; the served gzip-negotiated request
rides the M35 collector's view row. The TestClient-level request cost rides the same
harness floor on both arms and travels in the PR's Evidence section, not in this row.

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, os, shutil, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager
from src.api import sessions as sessions_api
from src.api import deps as deps_api

# Worst view-payload corpus: the session whose threads dir carries the most
# metadata files (the M5 resolution rule); each thread row rides the view body.
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    t = d / "threads"
    if t.is_dir():
        n = sum(1 for p in t.iterdir() if (p / "metadata.json").is_file())
        if n > best_n:
            best, best_n = d, n
SID = best.name

# Isolation: scratch CHARLIEBOT_HOME under /tmp holding only a copy of that
# session's metadata.json, data/, and threads/; live home read once for the
# copy, never written (the view is a side-effect-free read; the copy pins the
# corpus and keeps the timed calls off the live home).
home = Path(tempfile.mkdtemp(prefix="m63-view-home-", dir="/tmp"))
dst = home / "sessions" / SID
dst.mkdir(parents=True)
shutil.copy2(best / "metadata.json", dst / "metadata.json")
shutil.copytree(best / "data", dst / "data")
shutil.copytree(best / "threads", dst / "threads")

cfg = CharlieBotConfig(charliebot_home=home)
mgr = SessionManager(cfg)
tm = ThreadManager(cfg)
deps_api._trigger_manager = None

async def main():
    from starlette.requests import Request
    # The handler's request seam with no Accept-Encoding header (the no-gzip
    # client shape): the timed call reads the plain-path handler work.
    request = Request({"type": "http", "method": "GET",
                       "path": f"/api/sessions/{SID}/view", "headers": [],
                       "query_string": b""})
    meta = await mgr.get_session(SID)
    await sessions_api.get_session_view(SID, request, meta, mgr, tm, cfg)  # cold pass, as at first view after a server start; not timed
    times, bodies = [], []
    for _ in range(9):
        t0 = time.perf_counter()
        resp = await sessions_api.get_session_view(SID, request, meta, mgr, tm, cfg)
        times.append(time.perf_counter() - t0)
        bodies.append(len(resp.body))
    times.sort()
    print(f"checkout {os.environ['CHECKOUT'].rsplit('/', 1)[-1]}: session {SID}, {best_n} thread metadata files; "
          f"/view handler median {times[4]*1000:.2f} ms, max {times[-1]*1000:.2f} ms, body {bodies[0]} B")

asyncio.run(main())
shutil.rmtree(home)
EOF
```

M65 — big-page gzip event-loop stall, whole-body JSON response. The app mounts
`_CharlieBotGZipMiddleware` (level 1, bodies ≥ 1 KB), and Starlette's GZipResponder runs a
whole-body response's entire deflate inside the send path — so every JSON page the browser
fetches with `Accept-Encoding: gzip` freezes the event loop for the full compression while
it also renders, an invisible sibling of the M14/M55 stalls. The collector drives the real
app stack (gzip + auth middleware) raw-ASGI, with a concurrent 5 ms ticker, against a scratch
`CHARLIEBOT_HOME` (its config carries an empty access key, which the auth middleware passes
through) holding a copy of the worst on-disk live events corpus (metadata.json and data/,
live home read once for the copy, never written), fetching the 200-message events page with
gzip accepted: one cold pass, as at the first big page after a server start, then nine timed
runs reporting the worst ticker gap and wall each (a drive faster than the ticker cadence
records no tick and reports its own wall, the M14 never-yields rule). Streaming bodies (SSE)
keep the inline per-chunk path and are out of this metric's shape.

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, os, shutil, sys, tempfile, time
from pathlib import Path

sys.path.insert(0, os.environ["CHECKOUT"])

# Worst big-page corpus: the session whose live chat file carries the most events.
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    p = d / "data" / "chat_events.jsonl"
    if p.is_file():
        with open(p, errors="replace") as f:
            n = sum(1 for _ in f)
        if n > best_n:
            best, best_n = d, n
SID = best.name

# Isolation: scratch CHARLIEBOT_HOME under /tmp holding a copy of that session's
# metadata.json and data/ (live home read once for the copy, never written); the
# scratch credentials carry an empty access key, which the auth middleware passes through.
home = Path(tempfile.mkdtemp(prefix="m65-gzip-home-"))
(home / "sessions" / SID).mkdir(parents=True)
shutil.copy2(best / "metadata.json", home / "sessions" / SID / "metadata.json")
shutil.copytree(best / "data", home / "sessions" / SID / "data")
(home / "credentials.yaml").write_text("charliebot:\n  access_key: ''\n")
os.environ["CHARLIEBOT_HOME"] = str(home)

import server  # noqa: E402  (the real app stack: _CharlieBotGZipMiddleware + AuthMiddleware)

QUERY = f"/api/sessions/{SID}/events?before={best_n}&limit=200"
SCOPE = {
    "type": "http", "asgi": {"version": "3.0", "spec_version": "2.3"},
    "http_version": "1.1", "method": "GET", "scheme": "http",
    "path": f"/api/sessions/{SID}/events", "raw_path": QUERY.encode(),
    "query_string": f"before={best_n}&limit=200".encode(),
    "root_path": "", "headers": [(b"host", b"test"), (b"accept-encoding", b"gzip")],
    "client": ("test", 123), "server": ("test", 80),
}


async def drive():
    body = b""

    async def receive():
        return {"type": "http.request", "body": b"", "more_body": False}

    async def send(msg):
        nonlocal body
        if msg["type"] == "http.response.body":
            body += msg.get("body", b"")

    t0 = time.perf_counter()
    await server.app(SCOPE, receive, send)
    return time.perf_counter() - t0, body


async def main():
    await drive()  # cold pass, as at the first big page after a server start; not timed
    worst, walls, wire = [], [], 0
    for _ in range(9):
        stop = False
        gaps = []

        async def ticker():
            prev = time.perf_counter()
            while not stop:
                await asyncio.sleep(0.005)
                now = time.perf_counter()
                gaps.append(now - prev)
                prev = now

        t = asyncio.create_task(ticker())
        dt, wire = await drive()
        stop = True
        await t
        worst.append(max(gaps) if gaps else dt)
        walls.append(dt)
    worst.sort()
    walls.sort()
    print(f"{best_n}-event corpus, {len(wire)} B gzip wire; "
          f"loop-lag median {worst[4] * 1000:.2f} ms, max {worst[-1] * 1000:.2f} ms; "
          f"wall median {walls[4] * 1000:.2f} ms, max {walls[-1] * 1000:.2f} ms over 9")
    shutil.rmtree(home)


asyncio.run(main())
EOF
```

M66 — perfetto merged-trace build wall, steady state. The first view of a merged trace awaits
`merge_traces` in the server's process pool — a full `json.load` of every input plus a per-event
re-serialize into the gzip stream — so the build wall is user-visible first-view latency (the
cache answers repeat views). The cost is background pool work invisible to HTTP probes, so the
collector times the build over the largest Chrome-JSON trace on disk (read-only; scratch output
under /tmp), from the checkout under test: one cold pass, as at the first view of a corpus, then
three timed builds. The trace roots are the host's documented trace homes (~/data, ~/scripts);
no qualifying file prints nothing and the round treats the metric as unmeasured. Evidence while
the live server runs older code points the same collector at the branch checkout (`CHECKOUT` at
the worktree root), the same shape as the M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import os, shutil, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
import orjson
from src.core.trace_merge import _trace_events_or_raise, merge_traces

# Worst build corpus: the largest Chrome-JSON *.json trace under the documented
# trace roots (~/data, ~/scripts); no qualifying file prints nothing. The shape
# contract is the build's own (_trace_events_or_raise): a size-ranked walk alone
# cannot tell a trace whose traceEvents sits behind deviceProperties from a JSON
# body that parses cleanly yet carries no events (an analysis manifest measured
# as the worst corpus on 2026-09-15 and built an empty artifact), so candidates
# qualify largest-first by parse+shape, stopping at the first passing file.
candidates = []
for root in (Path.home() / "data", Path.home() / "scripts"):
    if not root.is_dir():
        continue
    for p in root.rglob("*.json"):
        try:
            n = p.stat().st_size
        except OSError:
            continue
        with p.open("rb") as f:
            prefix = f.read(64).lstrip(b" \t\n\r")
        if prefix[:1] in (b"{", b"["):
            candidates.append((n, p))
candidates.sort(reverse=True)
best, best_n = None, -1
for n, p in candidates:
    try:
        with p.open("rb") as f:
            _trace_events_or_raise(orjson.loads(f.read()), p)
    except ValueError:  # orjson.JSONDecodeError subclasses ValueError; so does the shape rejection
        continue
    best, best_n = p, n
    break
if best is None:
    raise SystemExit(0)
print(f"worst build corpus: {best}, {best_n / 1e6:.1f} MB")

work = Path(tempfile.mkdtemp(prefix="m66-merge-"))
out = work / "merged.json.gz"
try:
    merge_traces([best], out, slim=False)  # cold pass, as at the first view of a corpus; not timed
    times = []
    for _ in range(3):
        t0 = time.perf_counter()
        merge_traces([best], out, slim=False)
        times.append(time.perf_counter() - t0)
    times.sort()
    print(f"merged build median {times[1]:.2f} s, max {times[-1]:.2f} s over 3; artifact {out.stat().st_size / 1e6:.1f} MB.gz")
finally:
    shutil.rmtree(work)  # every exit path removes the scratch copy: the hourly cadence leaks one copy per skipped removal
EOF
```

M67 — sidebar deep-probe trigger scan, steady state. The status poll's dirty-session deep probe
(the M51 shape: every poll that follows any write to the session) runs `pending_trigger_state_sync`,
whose pre-fix form read and parsed every trigger `*.json` of the session on every call while its two
sibling probe reads (thread metadata via the M51 memo, plans via the M27 memo) were already
memoized. The cost is background probe work invisible to the standing HTTP probes, so the collector
times the function over the session whose triggers directory carries the most files (read-only over
the live state), from the checkout under test: one cold pass, as at a server start with an empty
memo, then nine timed calls. The fixed reader memoizes each trigger file's parsed dict on
(mtime_ns, size): the steady state is one scandir and one stat per file and zero corpus bytes, and a
`_save_trigger` rewrite (schedule/cancel/fire) re-reads only that file. Trigger files change only
through that atomic rewrite, so the key is sound. Evidence while the live server runs older code
points the same collector at the branch checkout (`sys.path.insert` at the worktree root), the same
shape as the M7 protocol:

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import sys, time
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from pathlib import Path
from src.core.sessions import pending_trigger_state_sync

# Worst trigger corpus: the session whose triggers directory carries the most
# files; the deep probe re-reads every one per poll that follows a write.
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.glob("*/triggers"):
    n = sum(1 for p in d.glob("*.json") if p.is_file())
    if n > best_n:
        best, best_n = d, n
SID = best.parent.name

pending_trigger_state_sync(best)  # cold pass, as at a server start with an empty memo; not timed
times = []
result = None
for _ in range(9):
    t0 = time.perf_counter()
    result = pending_trigger_state_sync(best)
    times.append(time.perf_counter() - t0)
times.sort()
print(f"session {SID}, {best_n} trigger files (pending {result[0]}); steady-state probe trigger scan "
      f"median {times[4] * 1e6:.0f} us, max {times[-1] * 1e6:.0f} us")
EOF
```

M68 — worker-list marked changed-poll rebuild. During an active turn the running
worker's metadata.json rewrites continuously, so nearly every 3 s poll of that
session's workers panel takes the list body's rebuild path instead of the M36
memo hit. The rebuild's shape is one writer mark plus one poll; the collector
copies the session whose threads directory carries the most metadata bytes into
a scratch `CHARLIEBOT_HOME` under /tmp (live home read once for the copy, never
written), wires the copy through the config dependency the endpoint resolves
(`get_config_on_loop` — overriding `get_config` alone leaves the endpoint on
the live home, where the scratch rewrites are invisible and no rebuild ever
happens; the pre-repair collector measured walk-plus-memo-serve, the vacuous
class), and drives the marked shape: one cold build, one memo-hit poll, then
eight rounds of (one atomic metadata rewrite + the writer funnel's
`mark_sidebar_dirty` with the published path, one timed request) — every timed
request a genuine rebuild, answered by the incremental proof the writers'
path-carrying marks enable. The unchanged-poll steady state has no row here;
M36 owns it.

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, gzip, hashlib, json, os, shutil, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from fastapi import FastAPI
from server import _CharlieBotGZipMiddleware
from src.api.deps import get_config, get_config_on_loop, get_thread_manager, get_trigger_manager
from src.api.threads import router as threads_router
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager
from src.core.triggers import TriggerManager
from src.core.sidebar_state import mark_sidebar_dirty

# Worst worker-list corpus: the session whose threads carry the most metadata
# bytes; live home read once for the copy, never written; the marked rewrites
# land on the scratch copy only.
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    t = d / "threads"
    if t.is_dir():
        n = sum((p / "metadata.json").stat().st_size for p in t.iterdir() if (p / "metadata.json").is_file())
        if n > best_n:
            best, best_n = d, n
SID = best.name
home = Path(tempfile.mkdtemp(prefix="m68-rebuild-", dir="/tmp"))
shutil.copytree(best, home / "sessions" / SID)
cfg = CharlieBotConfig(charliebot_home=home)
thread_mgr = ThreadManager(cfg)
trigger_mgr = TriggerManager(cfg, SessionManager(cfg))
app = FastAPI()
app.include_router(threads_router, prefix="/api/threads")
app.dependency_overrides[get_thread_manager] = lambda: thread_mgr
app.dependency_overrides[get_trigger_manager] = lambda: trigger_mgr
app.dependency_overrides[get_config] = lambda: cfg
app.dependency_overrides[get_config_on_loop] = lambda: cfg
# The production middleware chain, so the drive reads the shape the serve
# path runs. For this route the endpoint's body-keyed gzip memo serves
# Content-Encoding set upstream and the middleware skips its own pass — the
# mount keeps the drive honest against a route change rather than adding
# deflate work.
app.add_middleware(_CharlieBotGZipMiddleware, minimum_size=1000, compresslevel=1)


def scope(url, query=b""):
    return {
        "type": "http", "asgi": {"version": "3.0", "spec_version": "2.3"},
        "http_version": "1.1", "method": "GET", "scheme": "http",
        "path": url, "raw_path": url.encode(), "query_string": query, "root_path": "",
        "headers": [(b"host", b"test"), (b"accept-encoding", b"gzip")],
        "client": ("test", 123), "server": ("test", 80),
    }


async def drive(url, query=b""):
    body = b""
    out = {"status": 0, "encoding": b""}

    async def receive():
        return {"type": "http.request", "body": b"", "more_body": False}

    async def send(msg):
        nonlocal body
        if msg["type"] == "http.response.start":
            out["status"] = msg["status"]
            out["encoding"] = dict(msg.get("headers", [])).get(b"content-encoding", b"")
        elif msg["type"] == "http.response.body":
            body += msg.get("body", b"")

    t0 = time.perf_counter()
    await app(scope(url, query), receive, send)
    return time.perf_counter() - t0, body, out


def digest(decoded):
    return hashlib.sha256(json.dumps(json.loads(decoded), sort_keys=True).encode()).hexdigest()[:12]


async def main():
    url = f"/api/threads/{SID}/list"
    session_dir = home / "sessions" / SID
    metas = sorted((session_dir / "threads").glob("*/metadata.json"), key=lambda p: p.stat().st_mtime_ns)

    def dirty(i):
        victim = metas[i % len(metas)]
        tmp = victim.with_name("metadata.json.m68probe")
        tmp.write_text(victim.read_text(encoding="utf-8"), encoding="utf-8")
        os.replace(tmp, victim)
        mark_sidebar_dirty(SID, str(victim))  # the writer funnel's mark: the published path rides it

    await drive(url)  # cold build, as at first panel paint after a server start; not timed
    await drive(url)  # memo hit; not timed
    dirty(0)
    times, decoded_size, wire_size, digests = [], 0, 0, set()
    for i in range(8):
        t0 = time.perf_counter()
        dt, body, out = await drive(url)
        assert out["status"] == 200, out["status"]
        d = gzip.decompress(body) if out["encoding"] == b"gzip" else body
        times.append(dt)
        decoded_size = len(d)
        wire_size = len(body)
        digests.add(digest(d))
        dirty(i + 1)
    times.sort()
    assert len(digests) == 1, f"rebuild bodies differed: {digests}"
    print(f"checkout {os.environ['CHECKOUT'].rsplit('/', 1)[-1]}: {best_n / 1e3:.0f} KB thread metadata in session {SID} "
          f"({len(metas)} rows); marked changed-poll rebuild median {times[4] * 1000:.2f} ms, "
          f"max {times[-1] * 1000:.2f} ms over 8, decoded {decoded_size} B wire {wire_size} B, digest {digests.pop()}")
    shutil.rmtree(home)


asyncio.run(main())
EOF
```

M69 — opencode SSE unhandled-event debug stream, steady state. Every `opencode serve`
SSE frame routes through `_translate_sse_event`, and an event type the translator
neither maps nor ignores logs `opencode_sse_event_unhandled` per frame — 306 lines
in the 68.85 h live server log sampled 2026-09-06, 295 of them `type=todo.updated`
(one per todo write) and 11 `type=session.compacted` — so a stream of unhandled
frames re-fires the same line forever while one sighting per event type carries
the whole signal: the set of handled types is code, fixed for the process. The
cost is background log volume invisible to HTTP probes, so the collector drives
`_translate_sse_event` on a synthetic unhandled frame: one first-sighting call,
as at a process start, then 60 steady-state repeat calls, counting the event. A
line in the repeat window is a re-fired alarm; the count is the metric. Evidence
while the live server runs older code points the same collector at the branch
checkout (`sys.path.insert` at the worktree root), the same shape as the M22
protocol:

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import sys
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from src.agents.backends import opencode as opencode_mod

backend = opencode_mod.OpenCodeBackend()
event = {"type": "todo.updated", "properties": {"sessionID": "m69-probe"}}

events = []
orig = opencode_mod.log.debug
opencode_mod.log.debug = lambda event, **kw: events.append(event)
try:
    backend._translate_sse_event(event)  # first sighting, as at a process start; not counted
    events.clear()
    for _ in range(60):  # steady-state repeat frames of the same unhandled type
        backend._translate_sse_event(event)
finally:
    opencode_mod.log.debug = orig
n = sum(1 for event in events if event == "opencode_sse_event_unhandled")
print(f"60 steady-state unhandled todo.updated frames; opencode_sse_event_unhandled debug lines: {n}")
EOF
```

M70 — artifact clean-view serve, steady state. Every view of a session artifact page
(`GET /absolute_filepath/…/<session>/artifacts/<page>.html`) read and re-injected the whole page per request,
while the `?diff=` sibling served repeats from the M55 annotate memo. The chat log links plan and
report pages that are re-opened repeatedly (794 of the 823 artifact views in the 69.85 h live log
sampled 2026-09-06 were repeats of an already-viewed file), so each repeat paid the full-file read
(~4.8 ms on the 1.08 MB worst artifact) plus the injection for identical bytes. The collector
copies the largest on-disk artifact page into a scratch `CHARLIEBOT_HOME` under /tmp (live home
read once for the copy, never written) and drives the files router raw-ASGI in each checkout's
process with the production gzip middleware mounted and the browser's `Accept-Encoding: gzip`
header set — the whole-body deflate the server adds to every artifact response is part of the
served path, and a bare-app client times a shape production never runs (the pre-gzip-middleware
reading, 0.0026 s, is the vacuous-read class the M68 repair called out). The drive is raw-ASGI
(the M101 pattern) because the TestClient/httpx layer reads ~9 ms of harness per request on this
~0.8 MB wire body — 9.0 of the standing collector's 9.9 ms at the 2026-09-14 round, a floor that
drowned both the served path and the M70 landing's own fix — so one cold pass, as at first
artifact view, then nine timed requests, with the tray injecting unconditionally (the auth
middleware owns the access gate and the collector mounts none), so the drive carries the
injected view a real reader gets. Snapshot once:

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import shutil, tempfile
from pathlib import Path

# Worst clean-view corpus: the largest .html artifact page on disk; the served
# view reads and re-injects the whole page per request.
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for p in root.glob("*/artifacts/*.html"):
    n = p.stat().st_size
    if n > best_n:
        best, best_n = p, n
SID = best.parent.parent.name
home = Path(tempfile.mkdtemp(prefix="m70-artifact-home-", dir="/tmp"))
dst = home / "sessions" / SID / "artifacts"
dst.mkdir(parents=True)
shutil.copy2(best, dst / best.name)
print(f"worst artifact: session {SID}, {best.name}, {best_n / 1e6:.2f} MB")
print(f"export M70_HOME={home} M70_SID={SID} M70_NAME={best.name} M70_SIZE={best_n}")
EOF
```

Then run per checkout (``eval`` the snapshot export first):

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, shutil, gzip, hashlib, os, sys, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from fastapi import FastAPI
from src.api.files import router as files_router
from src.core.config import CharlieBotConfig
import src.api.files as files_mod
from server import _CharlieBotGZipMiddleware

home = Path(os.environ["M70_HOME"])
SID = os.environ["M70_SID"]
NAME = os.environ["M70_NAME"]

# Scratch wiring: the router's get_config resolves the snapshot home, never the
# live one; the tray injects unconditionally (the auth middleware owns the
# access gate and this harness mounts none), so the timed request carries the
# artifact-comments injection like a real view.
cfg = CharlieBotConfig(charliebot_home=home)
files_mod.get_config = lambda: cfg
app = FastAPI()
app.include_router(files_router, prefix="/absolute_filepath")
# The production middleware chain: every served response passes the whole-body
# gzip whose deflate is part of the view's cost.
app.add_middleware(_CharlieBotGZipMiddleware, minimum_size=1000, compresslevel=1)
url = f"/absolute_filepath/{home}/sessions/{SID}/artifacts/{NAME}"
HEADERS = [(b"host", b"test"), (b"accept-encoding", b"gzip")]  # the browser shape
SCOPE = {
    "type": "http", "asgi": {"version": "3.0", "spec_version": "2.3"},
    "http_version": "1.1", "method": "GET", "scheme": "http",
    "path": url, "raw_path": url.encode(), "query_string": b"", "root_path": "",
    "headers": HEADERS, "client": ("test", 123), "server": ("test", 80),
}


async def drive():
    body = b""
    encoding = b""

    async def receive():
        return {"type": "http.request", "body": b"", "more_body": False}

    async def send(msg):
        nonlocal body, encoding
        if msg["type"] == "http.response.start":
            encoding = dict(msg.get("headers", [])).get(b"content-encoding", b"")
        elif msg["type"] == "http.response.body":
            body += msg.get("body", b"")

    t0 = time.perf_counter()
    await app(SCOPE, receive, send)
    return time.perf_counter() - t0, body, encoding


async def main():
    cold, body, encoding = await drive()  # cold pass, as at first artifact view; not timed
    assert encoding == b"gzip", f"browser shape served without gzip: {encoding!r}"
    decoded = gzip.decompress(body)
    assert b"comment_post.js" in decoded, "artifact-comments injection missing"
    times = []
    bodies = set()
    digest = ""
    for _ in range(9):
        dt, body, _ = await drive()
        times.append(dt)
        bodies.add(len(body))
        digest = hashlib.sha256(body).hexdigest()[:12]
    times.sort()
    assert len(bodies) == 1, f"repeat bodies differ: {bodies}"
    print(f"{NAME} ({os.environ['M70_SIZE']} B); first view {cold:.4f} s; repeat-view median {times[4]:.4f} s, "
          f"max {times[-1]:.4f} s over 9, gzip body {bodies.pop()} B, digest {digest}")


try:
    asyncio.run(main())
finally:
    shutil.rmtree(home)  # every exit path removes the scratch copy: the hourly cadence leaks one copy per skipped removal
EOF
```

M71 — sidebar search capped name-match response, steady state. The search box's
short queries are the route's worst shape: a one-character query matches
hundreds of archived names, and the pre-fix manager copied and sidebar-populated
every match before the 200-row cap applied at the end. The cost is a per-
keystroke latency invisible to the standing probes (M8 reads the absent-needle
shape, whose result rows are zero), so the collector snapshots the search corpus
(every session's metadata.json, the active sessions' live chat files, and every
session's triggers/ — the derived trigger fields ride the response) into a
scratch `CHARLIEBOT_HOME` under /tmp (live home read once for the copy, never
written), resolves the single character matching the most session names, and
drives the route raw-ASGI behind the production gzip middleware in each
checkout's process: one cold pass,
as at the first capped search after a server start, then nine timed requests,
with a parsed-body digest so a corpus difference between arms cannot masquerade
as a payload difference. Evidence points the same collector at the before and
after checkouts (``CHECKOUT`` at each root, shared snapshot), the same shape as
the M35 protocol. Snapshot once:

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import json, shutil, tempfile
from pathlib import Path

# Worst capped-search corpus snapshot: every session's metadata.json, the
# active sessions' live chat files, and every session's triggers/ — exactly
# what the search route reads. Live home read once for the copy, never written.
root = Path.home() / ".charliebot" / "sessions"
home = Path(tempfile.mkdtemp(prefix="m71-search-home-", dir="/tmp"))
counts: dict[str, int] = {}
total_meta = 0
total_live_bytes = 0
for d in root.iterdir():
    meta_p = d / "metadata.json"
    if not meta_p.is_file():
        continue
    try:
        raw = json.loads(meta_p.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        continue
    dst = home / "sessions" / d.name
    dst.mkdir(parents=True)
    shutil.copy2(meta_p, dst / "metadata.json")
    total_meta += 1
    name = (raw.get("name") or "").lower()
    for ch in set(name):
        counts[ch] = counts.get(ch, 0) + 1
    if raw.get("status") == "active":
        live = d / "data" / "chat_events.jsonl"
        if live.is_file():
            (dst / "data").mkdir()
            shutil.copy2(live, dst / "data" / "chat_events.jsonl")
            total_live_bytes += live.stat().st_size
    triggers = d / "triggers"
    if triggers.is_dir():
        shutil.copytree(triggers, dst / "triggers")

best_q, best_n = None, -1
for ch, n in counts.items():
    if ch.isspace():
        continue  # a whitespace-only query routes to the route's list branch, never the capped search
    if n > best_n:
        best_q, best_n = ch, n
print(f"{total_meta} session metas, {total_live_bytes / 1e6:.1f} MB active live chat files")
print(f"worst capped query: {best_q!r} matching {best_n} names (cap 200)")
print(f"export M71_HOME={home} M71_Q={best_q}")
EOF
```

Then run per checkout (``eval`` the snapshot export first). The drive reads the
served path raw-ASGI behind the production gzip middleware — the browser's
search fetch always sends ``Accept-Encoding: gzip``, so the body's deflate is
part of the served shape; a TestClient drive adds ~1.5-2 ms of httpx harness
per request and skips the middleware whose deflate the browser's fetch always
pays, the vacuous-read class the M36/M56/M59/M70/M72 repairs called out:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, shutil, hashlib, json, os, sys, time
sys.path.insert(0, os.environ["CHECKOUT"])
from pathlib import Path
from fastapi import FastAPI
from server import _CharlieBotGZipMiddleware
import src.api.deps as deps
from src.api.deps import get_session_manager
from src.api.sessions import router as sessions_router
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager
from src.core.triggers import TriggerManager

home = Path(os.environ["M71_HOME"])
Q = os.environ["M71_Q"]
cfg = CharlieBotConfig(charliebot_home=home)
mgr = SessionManager(cfg)
deps._trigger_manager = TriggerManager(cfg, mgr)
app = FastAPI()
app.include_router(sessions_router, prefix="/api/sessions")
app.dependency_overrides[get_session_manager] = lambda: mgr
# The production middleware chain: the browser's search fetch always sends
# Accept-Encoding: gzip, so the body's deflate is part of the served shape —
# a bare app reads the render floor alone.
app.add_middleware(_CharlieBotGZipMiddleware, minimum_size=1000, compresslevel=1)

def scope():
    return {"type": "http", "asgi": {"version": "3.0", "spec_version": "2.3"},
            "http_version": "1.1", "method": "GET", "scheme": "http",
            "path": "/api/sessions/search", "raw_path": f"/api/sessions/search?q={Q}".encode(),
            "query_string": f"q={Q}".encode(), "root_path": "",
            "headers": [(b"host", b"test"), (b"accept-encoding", b"gzip")],
            "client": ("test", 123), "server": ("test", 80)}

async def drive():
    body = b""
    out = {"status": 0, "enc": b""}

    async def receive():
        return {"type": "http.request", "body": b"", "more_body": False}

    async def send(msg):
        nonlocal body
        if msg["type"] == "http.response.start":
            out["status"] = msg["status"]
            out["enc"] = dict(msg.get("headers", [])).get(b"content-encoding", b"")
        elif msg["type"] == "http.response.body":
            body += msg.get("body", b"")

    t0 = time.perf_counter()
    await app(scope(), receive, send)
    return time.perf_counter() - t0, body, out

def digest(decoded):
    return hashlib.sha256(json.dumps(json.loads(decoded), sort_keys=True).encode()).hexdigest()[:12]

async def main():
    _, _, cold = await drive()  # cold pass, as at the first capped search after a server start; not timed
    if cold["status"] != 200:
        raise SystemExit(f"M71 FAILED, cold status {cold['status']}")
    times = []
    decoded = wire_body = out = None
    for _ in range(9):
        dt, wire_body, out = await drive()
        times.append(dt)
        if out["status"] != 200:
            raise SystemExit(f"M71 FAILED, status {out['status']}")
    times.sort()
    decoded, wire, enc = wire_body, len(wire_body), out["enc"]
    if enc == b"gzip":
        import gzip
        decoded = gzip.decompress(decoded)
    print(f"checkout {os.environ['CHECKOUT'].rsplit('/', 1)[-1]}: {len(json.loads(decoded))} rows, "
          f"decoded {len(decoded)} B, wire {wire} B, enc {enc.decode() or 'identity'}, digest {digest(decoded)}; "
          f"capped search median {times[4]*1000:.2f} ms, max {times[-1]*1000:.2f} ms")

try:
    asyncio.run(main())
finally:
    shutil.rmtree(home)  # every exit path removes the scratch copy: the hourly cadence leaks one copy per skipped removal
EOF
```

M72 — file-browser directory listing. The file server renders a directory's
listing per request (`GET /absolute_filepath/<dir>` and `HEAD`); the browser's navigation
clicks pay the walk. The page memoizes on the walk's own entry snapshot
(per-entry (is_dir, name, size, mtime), the resolved dir and URL prefix around
it): equal walked state proves the stored page equals what this walk would
build, so a repeat view pays the walk plus one lookup, and a corpus move since
the stored page keyed — a metadata rename into a session dir moves exactly that
dir's mtime — keys the miss to a rebuild. The rebuild renders each row through
the row memo keyed on the same entry tuple plus the prefix; unchanged rows
serve as strings and only the moved entries re-render. The cost is a
navigation click, invisible to the standing HTTP probes (the chat log's
file-server traffic is artifact pages, the M70 shape; directory listings are
rare — 16 in the 78.85 h live log sampled 2026-09-07), so the collector drives
the files router raw-ASGI behind the production gzip middleware over the
sessions root — the file browser's own starting directory and the largest
entry count the UI navigates on this host, read-only — from the checkout under
test, with the browser's Accept-Encoding: gzip request shape and a fail-loud
negotiation assert (a bare app reads the walk floor alone; the M70 repair's
vacuous-read class). The drive is raw-ASGI (the M101 pattern) because the
TestClient/httpx layer reads ~2-3 ms of harness per request on this wire body —
2.2-2.9 ms of the standing collector's 8.1-9.0 ms at the 2026-09-14 round, the
floor that false-tripped the line while the served path sat inside it — so one
cold pass, as at the first
browser open, then nine timed requests, with the served-body sha1 so a corpus
difference between arms cannot masquerade as a payload difference.

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, gzip, hashlib, os, sys, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from fastapi import FastAPI
from src.api.files import router as files_router
from server import _CharlieBotGZipMiddleware

# Worst listing corpus: the sessions root — the file browser's starting
# directory and the largest entry count the UI navigates on this host.
corpus = Path.home() / ".charliebot" / "sessions"
n = sum(1 for _ in os.scandir(corpus))

app = FastAPI()
app.include_router(files_router, prefix="/absolute_filepath")
# The production middleware chain: every served response passes the whole-body
# gzip whose deflate is part of the view's cost — a bare app reads the walk
# floor alone.
app.add_middleware(_CharlieBotGZipMiddleware, minimum_size=1000, compresslevel=1)
url = f"/absolute_filepath{corpus}"
SCOPE = {
    "type": "http", "asgi": {"version": "3.0", "spec_version": "2.3"},
    "http_version": "1.1", "method": "GET", "scheme": "http",
    "path": url, "raw_path": url.encode(), "query_string": b"", "root_path": "",
    "headers": [(b"host", b"test"), (b"accept-encoding", b"gzip")],
    "client": ("test", 123), "server": ("test", 80),
}


async def drive():
    body = b""
    encoding = b""

    async def receive():
        return {"type": "http.request", "body": b"", "more_body": False}

    async def send(msg):
        nonlocal body, encoding
        if msg["type"] == "http.response.start":
            encoding = dict(msg.get("headers", [])).get(b"content-encoding", b"")
        elif msg["type"] == "http.response.body":
            body += msg.get("body", b"")

    t0 = time.perf_counter()
    await app(SCOPE, receive, send)
    return time.perf_counter() - t0, body, encoding


async def main():
    _, body, encoding = await drive()  # cold pass, as at the first browser open; not timed
    assert encoding == b"gzip", f"browser shape served without gzip: {encoding!r}"
    decoded = gzip.decompress(body)
    times = []
    for _ in range(9):
        dt, _, _ = await drive()
        times.append(dt)
    times.sort()
    print(f"{n} entries; served-path listing repeat median {times[4]*1000:.2f} ms, max {times[-1]*1000:.2f} ms, "
          f"decoded body {len(decoded)} B, sha1 {hashlib.sha1(decoded).hexdigest()[:12]}")


asyncio.run(main())
EOF
```

M72 changed-round — the rebuild behind a view whose corpus moved since the stored page keyed (the
navigation shape between two browser opens with any session write between them; the standing
collector's back-to-back requests never cross a move — 0 rebuilds across 27 timed requests at the
2026-09-12 corpus). The harness drops the page memo per timed round instead of writing the live
corpus — the key miss the move produces — and leaves the row memo warm, as the long-running
server's is; the walk, sort, and join re-run per round and unchanged rows serve as strings:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import os, sys, time
sys.path.insert(0, os.environ["CHECKOUT"])
from pathlib import Path
import src.api.files as files_mod

# Worst listing corpus: the sessions root (the M72 standing shape), read-only.
corpus = Path.home() / ".charliebot" / "sessions"
prefix = f"/absolute_filepath{corpus}"
checkout = os.environ["CHECKOUT"].rsplit("/", 1)[-1]

# Cold pass, as at a first browser open: rows memoized the way a first view
# leaves them; not timed.
files_mod._dir_listing_page(corpus, prefix, None)
times = []
for _ in range(9):
    # The changed-round shape: a corpus move since the stored page keyed
    # walks to a key miss; the harness drops the page memo instead of writing
    # the live corpus, and the row memo stays warm as the running server's is.
    files_mod._listing_memo.clear()
    t0 = time.perf_counter()
    files_mod._dir_listing_page(corpus, prefix, None)
    times.append(time.perf_counter() - t0)
times.sort()
print(f"checkout {checkout}: {sum(1 for _ in os.scandir(corpus))} entries; "
      f"changed-round rebuild median {times[4]*1000:.2f} ms, max {times[-1]*1000:.2f} ms over 9")
EOF
```

M73 — plan-verb validation event-loop lag. Every plan present/amend runs
`_validate_new_version_file`, whose registration gate is the full plan assertion set
(`run_assertions`): the DOM checks plus the page-height measurement, which renders the page
through a headless-Chrome subprocess — hundreds of ms of wall time per page. The pre-fix
form ran that inline in the async verb, so every plan delivery froze the event loop for the
full Chrome render — the M14 pathology on the plan-delivery path (the live server log shows
the freeze as POST /api/internal/plan/amend and /plan/present request times of ~0.6-0.7 s).
The fixed form hops the assertion run to a thread; the loop-lag is the metric (the wall is
the Chrome render's own cost, now off-loop, and only bounds health). The cost is per
plan-delivery latency invisible to the standing HTTP probes (the verbs fire when the master
agent delivers or amends a plan, not on any poll), so the collector copies the smallest plan
page bound in a live plans.json that still passes the current pure assertion set (read-only
resolution; the page-height half runs through the host's real headless renderer, so the
corpus must be one that passes it) into a scratch `CHARLIEBOT_HOME` under /tmp, drives
`PlanRegistryManager` from the checkout under test — one cold present, as at the first plan
delivery after a server start, then five timed amends, each validating a fresh unbound copy,
with a concurrent 5 ms ticker reporting the worst gap plus wall:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, json, os, shutil, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.artifact_check import _ASSERTION_RUNNERS, _Context, _parse_dom
from src.core.config import CharlieBotConfig, get_config
from src.core.models import CreateSessionRequest
from src.core.plans import PlanRegistryManager
from src.core.sessions import SessionManager

# Validation shape: the smallest plan page bound in a live plans.json that still
# passes the current pure assertion set (page-height checked by the verb itself,
# through the host's real headless renderer); read-only resolution, one copy out.
PURE = ("style-verbatim", "sections-numbered", "foot-present", "fork-open-shape", "fork-explainer", "goal-budget",
        "ordinal-named")
root = Path.home() / ".charliebot" / "sessions"
candidates: list[tuple[int, Path]] = []
for d in root.iterdir():
    reg, art = d / "plans.json", d / "artifacts"
    if not (reg.is_file() and art.is_dir()):
        continue
    try:
        bound = {v["file"] for p in json.loads(reg.read_text()).get("plans", []) for v in p.get("versions", [])}
    except (OSError, ValueError):
        continue
    for name in bound:
        p = d / name
        if p.is_file():
            candidates.append((p.stat().st_size, p))
candidates.sort()

cfg = CharlieBotConfig(charliebot_home=Path.home() / ".charliebot")
best_src = None
for size, p in candidates:
    try:
        ctx = _Context(genre="plan", artifact=p, root=_parse_dom(p.read_text(encoding="utf-8")), cfg=cfg)
    except (OSError, ValueError):
        continue
    if not [o.name for n in PURE for o in _ASSERTION_RUNNERS[n](ctx) if not o.passed]:
        best_src = p
        break

# The headless renderer is host state: the live config's chrome bin (read-only read).
chrome_bin = get_config().headless_chrome_bin

# Isolation: scratch CHARLIEBOT_HOME under /tmp; live home read once for the copy, never written.
home = Path(tempfile.mkdtemp(prefix="m73-plan-verb-home-", dir="/tmp"))
# A session's default backend resolves from backends.options (empty by
# default since the sectioned config), so the scratch config carries one.
verb_cfg = CharlieBotConfig(charliebot_home=home, headless_chrome_bin=chrome_bin,
                            backends={"options": [{"id": "m73", "label": "M73", "type": "cc-claude",
                                                   "model": "claude-opus-4-6"}]})
mgr = SessionManager(verb_cfg)
plan_mgr = PlanRegistryManager(verb_cfg, mgr)

async def main():
    meta = await mgr.create_session(CreateSessionRequest(name="M73"))
    art_dir = home / "sessions" / meta.id / "artifacts"
    art_dir.mkdir(parents=True)
    shutil.copy2(best_src, art_dir / "plan_probe_v0.html")

    async def verb(file_name):
        gaps = []
        stop = False
        async def ticker():
            prev = time.perf_counter()
            while not stop:
                await asyncio.sleep(0.005)
                now = time.perf_counter()
                gaps.append(now - prev)
                prev = now
        t = asyncio.create_task(ticker())
        await asyncio.sleep(0.01)  # the ticker must be mid-sleep, or an inline block starves it unrecorded
        t0 = time.perf_counter()
        await plan_mgr.amend(meta.id, file=f"artifacts/{file_name}", note="m73 probe")
        wall = time.perf_counter() - t0
        stop = True
        await t
        return (max(gaps) if gaps else wall), wall

    await plan_mgr.present(meta.id, file="artifacts/plan_probe_v0.html", title="M73")  # cold pass; not timed
    results = []
    for i in range(1, 6):
        shutil.copy2(best_src, art_dir / f"plan_probe_v{i}.html")
        results.append(await verb(f"plan_probe_v{i}.html"))
    lags = sorted(r[0] for r in results)
    walls = sorted(r[1] for r in results)
    print(f"{best_src.name} ({best_src.stat().st_size / 1e3:.0f} KB); amend validation loop-lag median {lags[2]:.4f} s, "
          f"max {lags[-1]:.4f} s; wall median {walls[2]:.4f} s, max {walls[-1]:.4f} s over 5")
    shutil.rmtree(home)

asyncio.run(main())
EOF
```

M74 — master turn-end raw-log rescan. Every claude-family master turn ends with
the model-attribution rescan: a whole read+parse+project of the turn's own raw
log through a fresh backend translate, tens of ms on a multi-MB turn — inline
on the event loop before the fix, freezing every concurrent request and
WebSocket at the exact moment the client renders the turn's result (the M14
pathology on the turn-end path); the fixed site hops to a thread, which trades
the inline freeze for the GIL-handoff surcharge the M45 history documented
(~11 ms worst-corpus vs the 5 ms ticker floor on this host). The cost is a
per-turn freeze invisible to the standing HTTP probes, so the collector
resolves the largest on-disk master-run raw log among the sessions the turn-end
gate scans — the session's backend option must be claude-family, the
``_CLAUDE_RESUME_FLAG_BACKEND_TYPES`` set the live call site checks before
scanning; other families' logs never reach this scan, so their sizes cannot
price it (read-only) — and drives the scan through the production call shape
with the corpus session's own fresh translate and a concurrent 5 ms ticker,
from the checkout under test: one cold pass, as at a first turn end, then five
timed scans. The pre-fix numbers in the landing row are the same scan inline (the
pre-fix call shape, `runs.project_raw_events(runs.parse_raw_lines(...))`
without the hop). Evidence while the live server runs older code points the
same collector at the branch checkout (``CHECKOUT`` at the worktree root), the
same shape as the M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, json, os, sys, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
import structlog
structlog.configure(wrapper_class=structlog.make_filtering_bound_logger(30))  # silence the translate's debug stream
from src.core import runs
from src.core.config import get_config
from src.agents.master_cc_run import _CLAUDE_RESUME_FLAG_BACKEND_TYPES, _build_fresh_translate

# Worst turn-end rescan corpus: the largest on-disk master-run raw log whose
# session's backend option is claude-family — the session set the turn-end
# model-attribution gate scans (the same _CLAUDE_RESUME_FLAG_BACKEND_TYPES
# check the live call site runs before scanning). Live home read-only.
cfg = get_config()
best, best_n, best_backend = None, -1, None
for p in Path.home().glob(".charliebot/sessions/*/data/master_runs/*/agent.raw.ndjson"):
    try:
        backend = json.loads((p.parents[3] / "metadata.json").read_text()).get("backend")
    except (OSError, ValueError):
        continue
    session_option = cfg.get_backend_option(backend) if backend else None
    if session_option is None or session_option.type not in _CLAUDE_RESUME_FLAG_BACKEND_TYPES:
        continue
    n = p.stat().st_size
    if n > best_n:
        best, best_n, best_backend = p, n, backend
print(f"worst raw log: {best_n / 1e6:.1f} MB ({best})")

# The corpus session's own option builds the scan's translate, fresh per scan
# as the call site builds it.
option = cfg.get_backend_option(best_backend)
print(f"session backend {best_backend!r} -> option {option.id} ({option.type})")

def fresh_translate():
    return _build_fresh_translate(cfg, option)  # a fresh translate per scan, as the call site builds

async def run_once():
    gaps = []
    stop = False
    async def ticker():
        nonlocal stop
        prev = time.perf_counter()
        while not stop:
            await asyncio.sleep(0.005)
            now = time.perf_counter()
            gaps.append(now - prev)
            prev = now
    t = asyncio.create_task(ticker())
    await asyncio.sleep(0.01)  # the ticker must be mid-sleep, or an inline block starves it unrecorded
    t0 = time.perf_counter()
    events = await asyncio.to_thread(runs.project_raw_file, best, fresh_translate())
    wall = time.perf_counter() - t0
    stop = True
    await t
    return len(events), (max(gaps) if gaps else wall), wall

async def main():
    await run_once()  # cold pass, as at a first turn end; not timed
    results = []
    for _ in range(5):
        results.append(await run_once())
    lags = sorted(r[1] for r in results)
    walls = sorted(r[2] for r in results)
    print(f"{results[0][0]} projected events; turn-end rescan loop-lag median {lags[2]:.4f} s, "
          f"max {lags[-1]:.4f} s; wall median {walls[2]:.4f} s, max {walls[-1]:.4f} s")

asyncio.run(main())
EOF
```

M75 — live-aggregator catch-up, first streamed event. The first
`persist_and_broadcast` for a session after server start caught the live
aggregator up to the whole on-disk live corpus — one whole parse plus one
feed of every persisted event, with a per-event draft-snapshot build whose
delta the catch-up discards — inline on the event loop before the fix,
freezing every concurrent request and WebSocket at the session's first
persisted event after every server restart (the M14 pathology on the
streamed-turn funnel); the fixed site hops the catch-up to a thread behind a
per-session init lock, which trades the inline freeze for the GIL-handoff
surcharge the M45/M74 history documented (~11 ms worst-corpus vs the 5 ms
ticker floor on this host), and the feeds that discard stream deltas
(catch-up, history projection, ``events_to_messages``/``events_to_view``)
construct with ``emit_stream_deltas=False`` so the discarded snapshots are
never built. The cost is a per-session freeze invisible to the standing HTTP
probes, so the collector copies the session whose live chat file carries the
most events (live home read-only) into a scratch ``CHARLIEBOT_HOME`` under
/tmp — each timed round builds its own scratch home and copies the corpus
before the timed region — and drives the init through the production call
shape with a concurrent 5 ms ticker, from the checkout under test: one cold
pass, as at a server start, then five timed inits, each on a fresh manager
and corpus copy (the call shape differs across the fix — sync inline before,
awaited after — so the collector dispatches on ``iscoroutinefunction``).
The post-fix hop's GIL-handoff floor measured 0.0108-0.0157 s across rounds
(above M74's 0.015 s pin — the catch-up's parse+feed holds the GIL longer
than M74's scan), so this PR calibrates the range at < 0.020 s. Evidence
while the live server runs older code points the same collector at the
branch checkout (``CHECKOUT`` at the worktree root), the same shape as the
M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, inspect, os, shutil, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager

# Worst funnel corpus: the session whose LIVE chat file carries the most events;
# its first persist_and_broadcast after a server start pays the whole catch-up.
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    p = d / "data" / "chat_events.jsonl"
    if p.is_file():
        with open(p, errors="replace") as f:
            n = sum(1 for _ in f)
        if n > best_n:
            best, best_n = d, n
SID = best.name
print(f"worst funnel corpus: session {SID}, {best_n} live chat events")

async def run_once():
    cfg = CharlieBotConfig(charliebot_home=Path(tempfile.mkdtemp(prefix="m75-cfg-", dir="/tmp")))
    (cfg.charliebot_home / "sessions").mkdir(parents=True, exist_ok=True)
    shutil.copytree(best, cfg.charliebot_home / "sessions" / SID)
    mgr = SessionManager(cfg)
    init = mgr._get_or_init_aggregator
    gaps = []
    stop = False

    async def ticker():
        prev = time.perf_counter()
        while not stop:
            await asyncio.sleep(0.005)
            now = time.perf_counter()
            gaps.append(now - prev)
            prev = now

    t = asyncio.create_task(ticker())
    t0 = time.perf_counter()
    if inspect.iscoroutinefunction(init):
        await init(SID)
    else:
        init(SID)
    wall = time.perf_counter() - t0
    stop = True
    await t
    shutil.rmtree(cfg.charliebot_home)
    return (max(gaps) if gaps else wall), wall

async def main():
    await run_once()  # cold pass, as at the first streamed event after a server start; not timed
    worst, walls = [], []
    for _ in range(5):
        gap, wall = await run_once()
        worst.append(gap)
        walls.append(wall)
    worst.sort()
    walls.sort()
    print(f"{best_n}-event corpus; catch-up loop-lag median {worst[2]:.4f} s, max {worst[-1]:.4f} s; "
          f"wall median {walls[2]:.4f} s, max {walls[-1]:.4f} s over 5")

asyncio.run(main())
EOF
```

M76 — finalize-judgment reads, warm chain. Every worker and reviewer completion runs the
finalize chain's two idempotency judgments over the delegating session's whole chat history —
the duplicate-summary check (`terminal_summary_present` behind `_persist_worker_summary_once`)
and the wake judgment (`master_woke_after_summary` behind `_trigger_master_judged`) — and the
pre-fix forms ran both scans plus the event-list load inline on the event loop. The fixed form
serves both answers from the chat-event store's per-session finalize fold in O(1) (glossary on
`_FinalizeFold`; rules imported from finalize_effects' own predicates, parity pinned by test),
with a cold cache paying one threaded whole-file load. The cost is per-completion loop time
invisible to the standing HTTP probes, so the collector copies the session whose live chat file
carries the most events into a scratch `CHARLIEBOT_HOME` under /tmp (metadata.json and data/
only; live home read once for the copy, never written), warms the cache as the chain's first
judgment does, and times both judgments back to back with a concurrent 5 ms ticker, from the
checkout under test — one cold pass, as at the first finalize after a server start, then five
timed rounds. The collector dispatches on the fold methods' presence: a checkout without them
runs the pre-fix inline load+scan shapes. Evidence while the live server runs older code points
the same collector at the branch checkout (`CHECKOUT` at the worktree root), the same shape as
the M75 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, os, shutil, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager
from src.core import finalize_effects

# Worst judgment corpus: the session whose LIVE chat file carries the most
# events; every delegation's finalize chain scans exactly this session's
# history (the delegating master session is the busiest chat file).
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    p = d / "data" / "chat_events.jsonl"
    if p.is_file():
        with open(p, errors="replace") as f:
            n = sum(1 for _ in f)
        if n > best_n:
            best, best_n = d, n
SID = best.name

# Isolation: scratch CHARLIEBOT_HOME under /tmp holding only a copy of that
# session's metadata.json and data/; live home read once for the copy, never written.
home = Path(tempfile.mkdtemp(prefix="m76-fold-home-", dir="/tmp"))
dst = home / "sessions" / SID
dst.mkdir(parents=True)
shutil.copy2(best / "metadata.json", dst / "metadata.json")
shutil.copytree(best / "data", dst / "data")
cfg = CharlieBotConfig(charliebot_home=home)
mgr = SessionManager(cfg)
TID = "m76-probe-thread"  # an absent thread id: the prove-absence-over-history shape every first finalize runs

async def run_once():
    gaps = []
    stop = False
    async def ticker():
        prev = time.perf_counter()
        while not stop:
            await asyncio.sleep(0.005)
            now = time.perf_counter()
            gaps.append(now - prev)
            prev = now
    t = asyncio.create_task(ticker())
    t0 = time.perf_counter()
    if hasattr(mgr, "finalize_summary_present"):
        present = await mgr.finalize_summary_present(SID, TID)
        woke = await mgr.finalize_master_woke(SID, TID)
    else:
        events = mgr.load_chat_events_sync(SID)
        present = finalize_effects.terminal_summary_present(events, TID)
        woke = finalize_effects.master_woke_after_summary(events, TID)
    wall = time.perf_counter() - t0
    stop = True
    await t
    return present, woke, (max(gaps) if gaps else wall), wall

async def main():
    await run_once()  # cold pass, as at the first finalize after a server start; not timed
    results = []
    for _ in range(5):
        results.append(await run_once())
    lags = sorted(r[2] for r in results)
    walls = sorted(r[3] for r in results)
    print(f"{best_n}-event corpus, present={results[0][0]} woke={results[0][1]}; "
          f"judgment-pair loop-lag median {lags[2]:.5f} s, max {lags[-1]:.5f} s; "
          f"wall median {walls[2] * 1000:.3f} ms, max {walls[-1] * 1000:.3f} ms over 5")

asyncio.run(main())
shutil.rmtree(home)
EOF
```

M77 — session-switch projection reuse, rotating tabs. Every SPA switch re-reads the
session through the message projection (view, bootstrap, events page), whose per-session
LRU window must cover the tabs' rotation breadth: a re-entry past the window re-pays the
M26 cold build. The collector resolves the 12 active live sessions carrying the most
events (the rotation is wider than the pre-fix window of 8), warms every projection once
as the tabs' first visits do, then times 3 rounds of re-entries over the rotation,
counting the re-entries that re-built (a rebuild parses the corpus; a warm hit is a dict
read + len compare, so the 1 ms split is unambiguous). Live home read-only; from the
checkout under test:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import json, os, sys, time
sys.path.insert(0, os.environ["CHECKOUT"])
from pathlib import Path
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager

# Worst rotation corpus: the ACTIVE live sessions carrying the most events —
# the tabs the switch diagnostic rotates among; 12 exceeds the pre-fix LRU
# window of 8, so steady-state re-entries land past the window and re-build.
root = Path.home() / ".charliebot" / "sessions"
sizes = []
for d in root.iterdir():
    p = d / "data" / "chat_events.jsonl"
    if not p.is_file():
        continue
    meta = d / "metadata.json"
    try:
        status = json.loads(meta.read_text()).get("status") if meta.is_file() else None
    except (OSError, ValueError):
        status = None
    if status == "archived":
        continue
    with open(p, errors="replace") as f:
        n = sum(1 for _ in f)
    sizes.append((n, d.name))
sizes.sort(reverse=True)
ROTATION = [sid for _, sid in sizes[:12]]
print(f"rotation: {len(ROTATION)} active sessions, largest {sizes[0][0]} events")

cfg = CharlieBotConfig(charliebot_home=Path.home() / ".charliebot")
mgr = SessionManager(cfg)
for sid in ROTATION:  # first visits, as the tabs' initial renders do; not timed
    mgr.get_message_projection(sid)

times = []
rebuilt = 0
for _ in range(3):
    for sid in ROTATION:
        t0 = time.perf_counter()
        mgr.get_message_projection(sid)
        dt = time.perf_counter() - t0
        times.append(dt)
        if dt > 0.001:  # a warm hit is a dict read + len compare; a rebuild parses the corpus
            rebuilt += 1
times.sort()
n = len(times)
print(f"re-entry median {times[n // 2] * 1000:.2f} ms, max {times[-1] * 1000:.2f} ms over {n}; "
      f"{rebuilt}/{n} re-entries re-built the projection")
EOF
```

M78 — ndjson event parse, cold whole-file. The events cache, the M13 worker-events
reader, and every range/tail reader parse through one funnel whose per-line parse
dominates every cold events load (catch-up, projection build, usage, first view). The
collector times `parse_ndjson_file` over the worst on-disk live chat file and the
largest on-disk worker log (read-only), from the checkout under test: one cold pass,
as at a first view after a server start, then five timed calls each. Evidence while
the live server runs older code points the same collector at the branch checkout
(`CHECKOUT` at the worktree root), the same shape as the M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import os, sys, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.ndjson import parse_ndjson_file

# Worst parse corpus: the live chat file carrying the most bytes, plus the
# largest on-disk worker log; the events cache and the M13 reader parse both.
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    p = d / "data" / "chat_events.jsonl"
    if p.is_file():
        n = p.stat().st_size
        if n > best_n:
            best, best_n = p, n
wlog, wlog_n = None, -1
for p in root.glob("*/threads/*/data/events.jsonl"):
    n = p.stat().st_size
    if n > wlog_n:
        wlog, wlog_n = p, n

parse_ndjson_file(best)  # cold pass, as at first view after a server start; not timed
times = []
events = []
for _ in range(5):
    t0 = time.perf_counter()
    events = parse_ndjson_file(best)
    times.append(time.perf_counter() - t0)
times.sort()
parse_ndjson_file(wlog)
wtimes = []
wevents = []
for _ in range(5):
    t0 = time.perf_counter()
    wevents = parse_ndjson_file(wlog)
    wtimes.append(time.perf_counter() - t0)
wtimes.sort()
print(f"checkout {os.environ['CHECKOUT'].rsplit('/', 1)[-1]}: {best_n / 1e6:.1f} MB chat file ({len(events)} events) "
      f"parse median {times[2] * 1000:.1f} ms, max {times[-1] * 1000:.1f} ms; "
      f"{wlog_n / 1e6:.1f} MB worker log ({len(wevents)} events) median {wtimes[2] * 1000:.1f} ms, max {wtimes[-1] * 1000:.1f} ms")
EOF
```

M79 — git branches list, steady state. The /diff viewer's branch picker fetches
``GET /api/git/branches`` on every page open; each fetch ran one ``git branch -a
--sort=-committerdate`` subprocess to recompute a listing that is a pure function
of the repo's ref state — the ref set (a branch add/remove/rename renames a loose
ref file or rewrites packed-refs), every listed branch's committerdate (a ref
value change rewrites its file), and HEAD all publish through the ref mutations
the ref-state signature already covers, so an unchanged signature proves the
listing current. The listing cost grows with the ref count, and the ref count
grows one file per branch this workflow leaves behind (2,718 refs at the
landing measurement, 852 loose), an unbounded trend like the one the M41 row
documented for the resolution walk. The fixed handler serves a repeat listing
from a bounded memo keyed on (repo, ref-state signature) — the ref-resolution
memo's own key shape — with the signature walk and any subprocess in one thread
hop. The cost is per picker open, invisible to the standing HTTP probes, so the
collector drives the ``list_branches`` handler over the charlie-bot checkout
(read-only, scratch ``CHARLIEBOT_HOME``), from the checkout under test: the
first view, as at a picker open with a cold memo, then seven timed repeats,
with the served-list digest so a corpus difference between arms cannot
masquerade as a payload difference. Evidence points the same collector at the
before and after checkouts (``CHECKOUT`` at each root), the same shape as the
M41 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, hashlib, os, subprocess, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.config import CharlieBotConfig
from src.api.git import list_branches, _refs_signature

REPO = Path("/home/chaoli/workspace/charlie-bot")
REFS = subprocess.run(["git", "for-each-ref"], cwd=REPO, capture_output=True, text=True,
                      check=True).stdout.count("\n")
cfg = CharlieBotConfig(charliebot_home=Path(tempfile.mkdtemp(prefix="m79-home-")),
                       paths={"workspace_dirs": ["/home/chaoli/workspace"]})

async def main():
    t0 = time.perf_counter()
    first = await list_branches(repo=str(REPO))
    cold = time.perf_counter() - t0
    digest = hashlib.sha1(",".join(first).encode()).hexdigest()[:12]
    times = []
    last = None
    for _ in range(7):
        t0 = time.perf_counter()
        last = await list_branches(repo=str(REPO))
        times.append(time.perf_counter() - t0)
    assert last == first
    times.sort()
    sig_walks = []
    for _ in range(5):
        t0 = time.perf_counter()
        _refs_signature(REPO)
        sig_walks.append(time.perf_counter() - t0)
    sig_walks.sort()
    print(f"{REFS} refs; first view {cold:.4f} s; repeat-view median {times[3]:.4f} s, "
          f"max {times[-1]:.4f} s over 7; list digest {digest} ({len(first)} names); "
          f"signature walk median {sig_walks[2]*1000:.1f} ms")

asyncio.run(main())
EOF
```

M80 — token-tally changed round under append churn. The tally's cached parse re-read a moved
log file whole, so the /token-usage page paid a whole-transcript re-read for every file an
active turn appended to since the last collect — the production p90 the M80 definition row
quotes, invisible to the standing M7 probes (the warm page and the quiet changed round read 0
moved files). The fixed parse proves the unchanged prefix from a guard hash of its final
window plus the boundary newline and parses only the appended tail. The cost is the
busy-turn page load, so the collector copies the worst claude transcript and the worst codex
rollout into a scratch corpus (live home read once for the copy, never written), cold-collects
to build the cache, appends the corpus's own final ~1 MB (line-aligned, verbatim replay lines)
to both files, and times the changed-round collect, from the checkout under test: one round per
invocation; evidence pairs the before and after checkouts back-to-back. Rows digest across arms
so a corpus difference cannot masquerade as a payload difference:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import hashlib, json, os, shutil, sys, tempfile, time
from pathlib import Path

sys.path.insert(0, os.environ["CHECKOUT"])
from src.core import token_tally as tt

# Worst churn corpus: the largest claude transcript and the largest codex rollout.
best_claude = max(((p.stat().st_size, p) for p in (Path.home() / ".claude/projects").rglob("*.jsonl")))[1]
best_codex = max(((p.stat().st_size, p) for p in (Path.home() / ".codex/sessions").rglob("*.jsonl")))[1]

scratch = Path(tempfile.mkdtemp(prefix="m80-churn-"))
claude_home = scratch / "claude"
codex_home = scratch / "codex"
(sess_dir := claude_home / "projects" / "rel" / "big").mkdir(parents=True)
(roll_dir := codex_home / "sessions" / "big").mkdir(parents=True)
claude_log, codex_log = sess_dir / "big.jsonl", roll_dir / "rollout.jsonl"
shutil.copy2(best_claude, claude_log)
shutil.copy2(best_codex, codex_log)


def collect():
    return tt.collect_token_usage(claude_homes={"scratch": claude_home}, codex_homes={"scratch": codex_home},
                                  opencode_db=scratch / "absent.db", cache_path=scratch / "cache.json")


collect()  # cold pass builds the cache; not timed

for log in (claude_log, codex_log):  # one busy-turn append per file: its own final ~1 MB, line-aligned
    with log.open("rb") as fh:
        fh.seek(max(0, log.stat().st_size - 1024 * 1024))
        data = fh.read()
    with log.open("ab") as fh:
        fh.write(data[data.find(b"\n") + 1:])

t0 = time.perf_counter()
changed = collect()
wall = time.perf_counter() - t0
rows = [[r.source, r.model, r.calls, r.in_fresh, r.cache_write, r.cache_read, r.output] for r in changed.rows]
digest = hashlib.sha256(json.dumps(rows, sort_keys=True).encode()).hexdigest()[:12]
print(f"checkout {os.environ['CHECKOUT'].rsplit('/', 1)[-1]}: changed round after ~1 MB appends to both "
      f"transcripts: wall {wall:.4f} s, scanned {changed.scanned_bytes / 1e6:.2f} MB, rows digest {digest}")
shutil.rmtree(scratch)
EOF
```

M81 — chat math-walk, delimiter gate. `renderChatMath` runs the KaTeX auto-render walk over every
`.prose-msg` on every message re-render and the streamed paint runs it over the whole draft on
every coalesced paint; the walk scans every prose text node for the four delimiters even when the
message carries no math — M60's repeat-page metric and M33's replay stubbed exactly this walk, so
neither standing number saw it. The gate skips the walk when the message's own source (the
streamed draft text, else the `.prose-msg[data-raw]` source) carries none of the three delimiter
initials (`$`, `\(`, `\[`), and any character reference (which the browser decodes into the
walk's text nodes) forces it, so the skip is byte-identical. The cost is client-side, invisible
to every HTTP probe, so the collector loads the checkout's real renderer code with the page's
CDN-pinned katex 0.16.21 build over a jsdom DOM (one-time scratch install
`npm i --prefix /tmp jsdom@24`, resolved through `JSDOM_HOME`, default /tmp/node_modules) and
times the walk through the page's own call shapes over the live corpora (read-only): the worst
message page (the M60 corpus — the 40 largest assistant bodies of the live chat file carrying
the most bytes) and the largest math-free streamed draft at the coalesced paint cadence. Evidence
points the collector at the before and after checkouts (`CHECKOUT` at each root), the same shape
as the M33 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot}; node "$CHECKOUT/tests/katex_walk_collector.js"
```

M82 — worker events-log append, per event. Every worker event (text delta, tool use, tool result,
thinking) lands through the streamed-turn loop's per-event append before its broadcast, so the
append's executor-hop count rides the same path the chat-event append (M52) rides. The collector
copies no state: it appends one probe event to a scratch worker log under /tmp through the
checkout's real append shape — the run holds one append handle for its whole life, so the timed
shape is the per-event append exactly as the streamed-turn loop issues it (the append helper is
read as a direct attribute so a renamed helper fails the collector instead of silently timing the
removed pre-fix aiofiles shape, the M89/M90 repair standard, and the probe line is the checkout's
own ``_event_line`` output — the persisted shape, bytes since the streamed-turn landing — so a
line-shape change can never desync the collector).
One warm pass, as a run's first events, then 50 timed appends. Evidence points the same
collector at the before and after checkouts (``CHECKOUT`` at each root), the same shape as the
M76 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, os, sys, tempfile, time
sys.path.insert(0, os.environ["CHECKOUT"])
from src.agents import worker as worker_mod

# Scratch worker log under /tmp; the live home is never touched.
path = os.path.join(tempfile.mkdtemp(prefix="m82-append-"), "events.jsonl")
probe = {"type": "assistant", "message": {"content": "m82 probe " + "y" * 200}}

append = worker_mod._append_event_line
event_line = worker_mod._event_line


async def main():
    line = event_line(probe)
    fd = os.open(path, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o666)

    async def one():
        await append(fd, line)

    for _ in range(5):
        await one()  # warm, as a run's first events; not timed
    times = []
    for _ in range(50):
        t0 = time.perf_counter()
        await one()
        times.append(time.perf_counter() - t0)
    times.sort()
    print(f"checkout {os.environ['CHECKOUT'].rsplit('/', 1)[-1]}: worker events-log append "
          f"median {times[24] * 1e6:.0f} us, max {times[-1] * 1e6:.0f} us over 50")


asyncio.run(main())
EOF
```

M83 — versioned static-asset revalidation, warm page load. Every template-referenced asset URL
carries ``?v=<static_asset_version>`` (the runtime git version plus the served tree's content
digest, refreshed per page render, so the token tracks the bytes the URL serves even when a
working-tree edit lands between restarts), but the static mount served default caching, so the
browser revalidated all of them on every page load — one If-None-Match round trip plus ~0.5 ms
of serve work per asset (46 assets, ~21 ms of serve work per page load measured), on exactly the
files the latency loop edits most often. The fixed mount marks a 200 response whose request
named a version ``public, max-age=31536000, immutable`` — a response that named its version
names its content — so a warm-cache page load issues zero asset requests; a request without a
version parameter keeps default caching because its URL can outlive its content, and a 304
keeps the cached 200's own headers. The cost is per-page-load latency and serve CPU
invisible to the standing HTTP probes, so the collector resolves the dashboard's template asset
set from the checkout and drives each asset through the real app stack (gzip + auth middleware)
raw-ASGI: one cold pass (first load, 200), then five revalidation-shaped requests per asset,
reporting the per-asset wall and the page-load serve work the pre-fix shape multiplied into;
the warm-cache revalidation-request count reads off the header's presence. Evidence points the
collector at the branch checkout (``CHECKOUT`` at the worktree root), the same shape as the M18
protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'PYEOF2'
import asyncio
import os
import re
import sys
import time
from pathlib import Path

sys.path.insert(0, os.environ["CHECKOUT"])

import server  # the real app stack: gzip + auth middleware

CHECKOUT = Path(os.environ["CHECKOUT"])
ASSETS = sorted(set(re.findall(r'/(?:static/[\w/.\-]+\.(?:js|css))',
                               "\n".join(p.read_text(encoding="utf-8", errors="replace")
                                         for p in (CHECKOUT / "web" / "templates").rglob("*.html")))))
VERSION = "collector-v1"


async def drive(asset: str, extra_headers: list[tuple[bytes, bytes]]):
    url = f"{asset}?v={VERSION}"
    scope = {
        "type": "http", "asgi": {"version": "3.0", "spec_version": "2.3"},
        "http_version": "1.1", "method": "GET", "scheme": "http",
        "path": asset, "raw_path": url.encode(), "query_string": f"v={VERSION}".encode(),
        "root_path": "", "headers": [(b"host", b"test")] + extra_headers,
        "client": ("test", 123), "server": ("test", 80),
    }
    status = 0
    headers = {}
    chunks = []

    async def receive():
        return {"type": "http.request", "body": b"", "more_body": False}

    async def send(msg):
        nonlocal status
        if msg["type"] == "http.response.start":
            status = msg["status"]
            for k, v in msg["headers"]:
                headers[k.decode().lower()] = v.decode()
        elif msg["type"] == "http.response.body":
            chunks.append(msg.get("body", b""))

    await server.app(scope, receive, send)
    return status, headers, b"".join(chunks)


async def main():
    immutable_all = True
    revalidate_ms = []
    cold_ms = []
    for asset in ASSETS:
        status, headers, body = await drive(asset, [])  # cold first load; not timed
        assert status == 200, (asset, status)
        etag = headers.get("etag", "")
        cc = headers.get("cache-control", "")
        if cc != "public, max-age=31536000, immutable":
            immutable_all = False
        times = []
        for _ in range(5):
            t0 = time.perf_counter()
            status, _, _ = await drive(asset, [(b"if-none-match", etag.encode())])
            times.append(time.perf_counter() - t0)
            assert status == 304, (asset, status)
        times.sort()
        revalidate_ms.append(times[2])
        t0 = time.perf_counter()
        await drive(asset, [])
        cold_ms.append(time.perf_counter() - t0)
    revalidate_ms.sort()
    cold_ms.sort()
    mid = revalidate_ms[len(revalidate_ms) // 2]
    mid_cold = cold_ms[len(cold_ms) // 2]
    total = sum(revalidate_ms)
    print(f"{len(ASSETS)} versioned assets; revalidate-request median {mid * 1000:.2f} ms/asset, "
          f"max {revalidate_ms[-1] * 1000:.2f} ms; cold 200 median {mid_cold * 1000:.2f} ms; "
          f"page-load serve work at the pre-fix one-request-per-asset shape {total * 1000:.1f} ms; "
          f"immutable header on every versioned response: {immutable_all} "
          f"(warm-cache revalidation requests per page load: {0 if immutable_all else len(ASSETS)})")


asyncio.run(main())
PYEOF2
```

M84 — backend stream-line parse, worst on-disk raw log. The backend stream
funnels parse every streamed line of every covered backend's turn: the raw-log
tail-follow loop (the live and re-attach read side of the claude-family
backends) and the spawned-stdout NDJSON reader (the codex/opencode run loops)
in ``src/agents/backends/base.py``, the opencode SSE payload reader, and the
anthropic proxy's upstream chunk reader. The funnels' parser cost is invisible
to the standing HTTP probes (it rides the stream loop, not a request), so the
collector resolves the largest ``agent.raw.ndjson`` under the sessions root,
copies it to a scratch directory (live home read once for the copy, never
written), and replays it through the real funnel functions — the tail-follow
loop with a counting translate and a dead producer, the stdout reader over the
same lines — one cold pass then seven timed replays per funnel; the cursor
advance is disabled, so the replay writes nothing anywhere. A pre-pass asserts
parser parity: stdlib json and the funnels' parser must agree on every line of
the corpus (0 divergences), which is also the boundary claim's standing
evidence. Evidence while a change is under review points the same collector at
the branch checkout (``CHECKOUT`` at the worktree root), the same shape as the
M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, json, os, shutil, sys, tempfile, time
from pathlib import Path
import orjson
sys.path.insert(0, os.environ["CHECKOUT"])
from src.agents.backends.base import iter_ndjson_events, tail_follow_events

# Worst stream-parse corpus: the largest agent.raw.ndjson under the sessions
# root (master runs and thread transports) — the bytes every covered backend's
# streamed turn parses line by line, live through the tail-follow loop.
root = Path.home() / ".charliebot" / "sessions"
cands = [*root.glob("*/data/master_runs/*/agent.raw.ndjson"),
         *root.glob("*/threads/*/data/agent.raw.ndjson")]
best, best_n = None, -1
for p in cands:
    n = p.stat().st_size
    if n > best_n:
        best, best_n = p, n

# Isolation: scratch copy of the raw log; live home read once for the copy,
# never written. cursor=None disables the cursor advance, so the replay
# writes nothing anywhere.
work = Path(tempfile.mkdtemp(prefix="m84-raw-", dir="/tmp"))
raw = work / best.name
shutil.copy2(best, raw)

# Parser parity over the whole corpus: the stream funnels may swap the parser
# only while every live line keeps its parsed value.
diverged = 0
lines = []
with open(raw, "rb") as f:
    for raw_line in f:
        line = raw_line.decode("utf-8", errors="replace").strip()
        if not line:
            continue
        lines.append(raw_line)
        try:
            a = json.loads(line)
        except json.JSONDecodeError:
            a = None
        try:
            b = orjson.loads(line)
        except orjson.JSONDecodeError:
            b = None
        if a != b:
            diverged += 1

async def tail_replay():
    count = 0

    def translate(event):
        nonlocal count
        count += 1
        return [event]

    async for _ in tail_follow_events(
        raw, translate=translate, is_alive=lambda: False,
        cursor=None, start_offset=0, post_result_timeout=60.0,
    ):
        pass
    return count

class _LineReader:
    def __init__(self, lines):
        self._lines = lines
        self._i = 0

    def __aiter__(self):
        return self

    async def __anext__(self):
        if self._i >= len(self._lines):
            raise StopAsyncIteration
        line = self._lines[self._i]
        self._i += 1
        return line

async def stdout_replay():
    count = 0
    async for _ in iter_ndjson_events(_LineReader(lines)):
        count += 1
    return count

async def main():
    await tail_replay()  # cold pass, as at first mount after a server start; not timed
    times = []
    total = 0
    for _ in range(7):
        t0 = time.perf_counter()
        total = await tail_replay()
        times.append(time.perf_counter() - t0)
    times.sort()

    await stdout_replay()  # cold pass; not timed
    stimes = []
    s_total = 0
    for _ in range(7):
        t0 = time.perf_counter()
        s_total = await stdout_replay()
        stimes.append(time.perf_counter() - t0)
    stimes.sort()
    print(f"checkout {os.environ['CHECKOUT'].rsplit('/', 1)[-1]}: {best_n / 1e6:.1f} MB raw log, "
          f"{len(lines)} lines, parity divergences {diverged}; "
          f"tail-follow replay median {times[3] * 1000:.1f} ms, max {times[-1] * 1000:.1f} ms; "
          f"stdout-stream replay median {stimes[3] * 1000:.1f} ms, max {stimes[-1] * 1000:.1f} ms "
          f"({total}/{s_total} events) over 7")

try:
    asyncio.run(main())
finally:
    shutil.rmtree(work)  # every exit path removes the scratch copy: the hourly cadence leaks one copy per skipped removal
EOF
```

M85 — verify-finalize report read, steady state. Every verify worker's finalize chain
(`spawner_finalize._verify_report_for_task`, on the worker finalize and again on the
startup-reconcile replay) runs `read_verify_final_report` over the verify thread's events
log, and the pre-fix reader full-parsed the whole log (`parse_ndjson_file`) to scan its
last events — ~13.5 ms on the 6.7 MB worst on-disk log — although the RESULT event the
report quotes sits at the log tail on every on-disk verify thread. The fixed reader walks
512 KiB segments from the end through `iter_ndjson_events_from_end` (the M31
`parse_ndjson_tail_parseable` mechanics, now the shared generator under it), stopping once
both judgments settle — the last result event's payload, or the first non-empty assistant
text from the end; identical output, blank and malformed lines never counting in
either form. The cost is finalize-path thread time invisible to HTTP probes, so the
collector times the function the finalize chain awaits over the largest on-disk worker
log (read-only), from the checkout under test: one cold pass, as at first finalize after
a server start, then five timed calls. Evidence while the live server runs older code
points the same collector at the branch checkout (`CHECKOUT` at the worktree root), the
same shape as the M7 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, os, sys, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.config import CharlieBotConfig
from src.core.threads import ThreadManager
from src.core.verify_trailer import read_verify_final_report

root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for p in root.glob("*/threads/*/data/events.jsonl"):
    n = p.stat().st_size
    if n > best_n:
        best, best_n = p, n
SID, TID = best.parts[-5], best.parts[-3]

async def main():
    thread_mgr = ThreadManager(CharlieBotConfig(charliebot_home=Path.home() / ".charliebot"))
    report = await read_verify_final_report(SID, TID, thread_mgr)  # cold pass, as at first finalize after a server start; not timed
    times = []
    for _ in range(5):
        t0 = time.perf_counter()
        report = await read_verify_final_report(SID, TID, thread_mgr)
        times.append(time.perf_counter() - t0)
    times.sort()
    print(f"{best_n / 1e6:.1f} MB worker log, report {len(report)} chars; "
          f"read_verify_final_report median {times[2]*1000:.1f} ms, max {times[-1]*1000:.1f} ms over 5")

asyncio.run(main())
EOF
```

M86 — delegation takeoff-gate scan, delegation-flow shape. Every
`/api/internal/delegate` and `/api/internal/improve` POST runs
`check_takeoff_gate` over the delegating session's whole chat history (the
delegation target is always the busiest master session), on the default
executor pool. The pre-scan form walked every event and re-normalized every
real user message's full content on each call — O(history) per delegation,
growing with the master session without bound. The current form reads the
verdict's two answers (the file-last real user message's takeoff phrase, the
file-last parseable pre-takeoff stamp) through an answers memo keyed on the
chat-events cache's list identity: a cold or replaced list pays one full
backward walk with no early break (the stored answers must be complete prefix
facts), and an appended suffix folds by scanning only the suffix — a user
message in the suffix is the new file-last one, and the prefix's stored stamp
answer is the file-older bound the backward continuation would stop at — so a
steady-state call is a memo read plus an O(appended-since-last-call) scan
while the verdict stays the forward walk's. The cost is thread-pool time on
the spawn path, invisible to the
standing HTTP probes, so the collector copies the session whose live chat file
carries the most events into a scratch `CHARLIEBOT_HOME` under /tmp
(metadata.json and data/ only; live home read once for the copy, never
written), times nine warm calls over the corpus as it stands (the parity
witness — its verdict is whatever the live state is, reported not asserted),
then appends the delegation-flow shape — one authorized real user message, the
take-off instruction a delegate POST follows — and times nine warm calls,
asserting the allowed verdict:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, os, shutil, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager
from src.core.takeoff_gate import DelegationBlockedError, check_takeoff_gate

# Worst gate corpus: the session whose LIVE chat file carries the most events;
# the delegation target is always the busiest master session's chat history.
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    p = d / "data" / "chat_events.jsonl"
    if p.is_file():
        with open(p, errors="replace") as f:
            n = sum(1 for _ in f)
        if n > best_n:
            best, best_n = d, n
SID = best.name
print(f"worst gate corpus: session {SID}, {best_n} live chat events")

# Isolation: scratch CHARLIEBOT_HOME under /tmp holding only a copy of that
# session's metadata.json and data/; live home read once for the copy, never written.
home = Path(tempfile.mkdtemp(prefix="m86-gate-home-", dir="/tmp"))
dst = home / "sessions" / SID
dst.mkdir(parents=True)
shutil.copy2(best / "metadata.json", dst / "metadata.json")
shutil.copytree(best / "data", dst / "data")
cfg = CharlieBotConfig(charliebot_home=home)
mgr = SessionManager(cfg)


def timed_gate():
    times, blocked = [], False
    for _ in range(9):
        t0 = time.perf_counter()
        try:
            check_takeoff_gate(SID, mgr)
        except DelegationBlockedError:
            blocked = True
        times.append(time.perf_counter() - t0)
    times.sort()
    return times[4], times[-1], blocked


async def main():
    try:
        check_takeoff_gate(SID, mgr)  # cold pass, as at a server start with a cold events cache; not timed
    except DelegationBlockedError:
        pass
    a_med, a_max, a_blocked = timed_gate()
    print(f"corpus as it stands: {'blocked' if a_blocked else 'allowed'}; warm gate median {a_med * 1000:.2f} ms, "
          f"max {a_max * 1000:.2f} ms over 9 (parity witness; the verdict is the live state's)")
    await mgr.save_chat_event(SID, {
        "type": "user",
        "content": "take off — proceed with the delegated task",
        "timestamp": "2026-09-09T18:00:00+00:00",
    })
    try:
        check_takeoff_gate(SID, mgr)  # cold pass over the appended corpus; not timed
    except DelegationBlockedError:
        pass
    times, blocked = [], False
    for _ in range(9):
        t0 = time.perf_counter()
        try:
            check_takeoff_gate(SID, mgr)
        except DelegationBlockedError:
            blocked = True
        times.append(time.perf_counter() - t0)
    times.sort()
    assert not blocked, "the delegation-flow shape must gate-allow"
    print(f"{best_n}-event corpus, one authorized user message appended; delegation-flow gate "
          f"median {times[4] * 1000:.2f} ms, max {times[-1] * 1000:.2f} ms over 9")
    shutil.rmtree(home)

asyncio.run(main())
EOF
```

M87 — opencode abort client round-trip. `_abort_session` runs at every
opencode turn's cleanup (and `terminate`), posting to the run's local serve
over the shared outbound client (`src.core.http.get_http_client`); the
run-start attempt client (`_check_health` through the SSE stream) keeps its
own per-attempt construction and passes the process-wide prebuilt SSL
context (`_SERVE_SSL_CONTEXT` — httpx builds a fresh default SSL context
per AsyncClient when `verify` is left at its default, ~20 ms of event-loop
CPU per construction on this host), while the serve URL is plain localhost
HTTP and never uses the context for TLS. The cost is
turn-boundary event-loop work invisible to HTTP probes, so the collector
drives the real `_abort_session` (read-only: the run's session id is a
collector literal) against a local stub serve with a concurrent 5 ms ticker,
from the checkout under test: one cold pass, as at the first cleanup after a
process start, then nine timed calls. Evidence while the live server runs
older code points the same collector at the branch checkout (`CHECKOUT` at
the worktree root), the same shape as the M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, os, sys, threading, time
from http.server import BaseHTTPRequestHandler, HTTPServer
sys.path.insert(0, os.environ["CHECKOUT"])
from src.agents.backends.opencode import OpenCodeBackend

class _AbortHandler(BaseHTTPRequestHandler):
  def do_POST(self):
    self.rfile.read(int(self.headers.get("Content-Length", 0)))
    self.send_response(200)
    self.end_headers()
  def log_message(self, *args):
    pass

server = HTTPServer(("127.0.0.1", 0), _AbortHandler)
port = server.server_address[1]
threading.Thread(target=server.serve_forever, daemon=True).start()

backend = OpenCodeBackend(model="provider/model")
backend._server_url = f"http://127.0.0.1:{port}"
backend._session_id = "collector"

async def run_once():
  gaps = []
  stop = False
  async def ticker():
    prev = time.perf_counter()
    while not stop:
      await asyncio.sleep(0.005)
      now = time.perf_counter()
      gaps.append(now - prev)
      prev = now
  t = asyncio.create_task(ticker())
  t0 = time.perf_counter()
  await backend._abort_session()
  wall = time.perf_counter() - t0
  stop = True
  await t
  return (max(gaps) if gaps else wall), wall

async def main():
  await run_once()  # cold pass, as at the first cleanup after a process start; not timed
  lags, walls = [], []
  for _ in range(9):
    lag, wall = await run_once()
    lags.append(lag)
    walls.append(wall)
  lags.sort()
  walls.sort()
  print(f"checkout {os.environ['CHECKOUT'].rsplit('/', 1)[-1]}: _abort_session over a local stub serve; "
        f"loop-lag median {lags[4]*1000:.1f} ms, max {lags[-1]*1000:.1f} ms; "
        f"wall median {walls[4]*1000:.1f} ms, max {walls[-1]*1000:.1f} ms over 9")

asyncio.run(main())
server.shutdown()
EOF
```

M88 — perfetto direct-pass build, worst on-disk trace corpus. The first view of a single
Chrome-JSON trace (``/perfetto/merged?trace=<file>``, single input, not slim) runs
``_build_direct_pass_gzip``: a full parse validating the file, then a stream-compress of the
original bytes — so the build wall is user-visible first-view latency (the cache answers repeat
views). The cost is background executor work invisible to HTTP probes, so the collector times
the build over the largest Chrome-JSON trace on disk (read-only; scratch output under /tmp),
from the checkout under test: one cold pass, as at the first view of a corpus, then three timed
builds. The trace roots are the host's documented trace homes (~/data, ~/scripts); no
qualifying file prints nothing and the round treats the metric as unmeasured. Evidence while
the live server runs older code points the same collector at the branch checkout (``CHECKOUT``
at the worktree root), the same shape as the M66 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import os, subprocess, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
import orjson
from src.api.pages import _build_direct_pass_gzip
from src.core.trace_merge import _trace_events_or_raise

# Worst direct-pass corpus: the largest Chrome-JSON *.json trace under the documented
# trace roots (~/data, ~/scripts) — the M66 resolution rule; the single-trace first-view
# build reads and validates exactly this file. Qualification is the build's own shape
# contract applied largest-first by parse (the M66 repair: a size-ranked walk alone
# cannot tell a trace from a JSON body that parses cleanly yet carries no events).
candidates = []
for root in (Path.home() / "data", Path.home() / "scripts"):
    if not root.is_dir():
        continue
    for p in root.rglob("*.json"):
        try:
            n = p.stat().st_size
        except OSError:
            continue
        with p.open("rb") as f:
            prefix = f.read(64).lstrip(b" \t\n\r")
        if prefix[:1] in (b"{", b"["):
            candidates.append((n, p))
candidates.sort(reverse=True)
best, best_n = None, -1
for n, p in candidates:
    try:
        with p.open("rb") as f:
            _trace_events_or_raise(orjson.loads(f.read()), p)
    except ValueError:  # orjson.JSONDecodeError subclasses ValueError; so does the shape rejection
        continue
    best, best_n = p, n
    break
if best is None:
    raise SystemExit(0)
print(f"worst direct-pass corpus: {best}, {best_n / 1e6:.1f} MB")

work = Path(tempfile.mkdtemp(prefix="m88-direct-pass-", dir="/tmp"))
out = work / "direct.json.gz"

_build_direct_pass_gzip(best, out)  # cold pass, as at the first view of a corpus; not timed
times = []
for _ in range(3):
    t0 = time.perf_counter()
    _build_direct_pass_gzip(best, out)
    times.append(time.perf_counter() - t0)
times.sort()
print(f"direct-pass build median {times[1]:.2f} s, max {times[-1]:.2f} s over 3; artifact {out.stat().st_size / 1e6:.1f} MB.gz")
subprocess.run(["rm", "-rf", str(work)], check=True)
EOF
```

M89 — backend stderr pump, per chunk. Every covered backend's run tees subprocess stderr to the
run's stderr.log (0.6-4 MB on disk per run) through the streamed pump before the in-memory tail
update, and the pump's per-chunk cost is invisible to the HTTP probes above. The collector drives
the pump exactly as the run issues it — a scripted 8 KB-chunk stream (400 chunks) through
`AgentBackend._stream_stderr` over a fresh scratch stderr.log per round under /tmp (one round is
one run's pump, and a run opens its log once at pump start and never re-truncates a log it just
wrote — `_rotate_stale_transport` moves a prior attempt's log aside instead, so a re-trunc per
round prices an ext4 dirty-truncate no run pays), one warm pass then five timed pump rounds
reported per chunk — so the reading carries the pump's whole per-chunk cost, whatever
a checkout implements it with (timing a helper directly would keep reading a removed per-chunk
write after the pump stops issuing one, the vacuous-read class the M68/M70 repairs called out).
Evidence points the same collector at the before and after checkouts (``CHECKOUT`` at each root):

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, os, sys, tempfile, time
sys.path.insert(0, os.environ["CHECKOUT"])
from src.agents.backends.base import AgentBackend

CHUNKS = 400
chunk = b"x" * 8192

class _StubStream:
    def __init__(self):
        self._left = CHUNKS
    async def read(self, size):
        if not self._left:
            return b""
        self._left -= 1
        return chunk

class _StubBackend:
    def __init__(self):
        self._proc = type("P", (), {"stderr": _StubStream()})()
        self._stderr_tail = bytearray()

async def main():
    work = tempfile.mkdtemp(prefix="m89-stderr-tee-")
    stub = _StubBackend()
    await AgentBackend._stream_stderr(stub, os.path.join(work, "stderr-warm.log"))  # warm, as a run's first stderr bytes; not timed
    times = []
    for r in range(5):
        stub = _StubBackend()
        t0 = time.perf_counter()
        await AgentBackend._stream_stderr(stub, os.path.join(work, f"stderr-{r}.log"))
        times.append(time.perf_counter() - t0)
    times.sort()
    per_chunk = times[2] / CHUNKS
    print(f"checkout {os.environ['CHECKOUT'].rsplit('/', 1)[-1]}: stderr pump "
          f"{CHUNKS} chunks in {times[2] * 1000:.2f} ms median; per-chunk "
          f"{per_chunk * 1e6:.1f} us, max {times[-1] / CHUNKS * 1e6:.1f} us over 5 pump rounds")

asyncio.run(main())
EOF
```

M90 — backend stdout pump, per chunk or startup line. The opencode run tees `opencode serve`'s
stdout through the streamed pump (the startup wait appends each printed line, then the pump
writes every 8 KB chunk), and the antigravity envelope pump writes its whole stdout the same
way; the claude-family backends' raw stdout lands through the spawn fd, so those runs pay no
per-chunk write. The chunk shape drives `OpenCodeBackend._stream_stdout` exactly as the run
issues it (a scripted 400-chunk stream over a scratch stdout.log under /tmp, one warm pass then
five timed pump rounds reported per chunk, the same vacuous-read guard as M89); the startup-line
shape stays the per-line write the startup wait issues, timed through the pumps' module-level
write read as a direct attribute so a renamed helper fails the collector instead of silently
timing a removed shape. Evidence points the same collector at the before and after checkouts
(``CHECKOUT`` at each root):

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, os, sys, tempfile, time
sys.path.insert(0, os.environ["CHECKOUT"])
import src.agents.backends.base as base_mod
from src.agents.backends.opencode import OpenCodeBackend

CHUNKS = 400
chunk = b"x" * 8192
line = b"2026-09-11T04:00:00.000Z  INFO serve listening on 127.0.0.1:4099\n"
path = os.path.join(tempfile.mkdtemp(prefix="m90-stdout-pump-"), "stdout.log")

class _StubStream:
    def __init__(self):
        self._left = CHUNKS
    async def read(self, size):
        if not self._left:
            return b""
        self._left -= 1
        return chunk

class _StubBackend:
    def __init__(self, fd):
        self._proc = type("P", (), {"stdout": _StubStream()})()
        self._stdout_fd = fd

# The startup line's write; a checkout whose base module lacks the name has no
# shape worth timing, so the AttributeError is the finding.
helper = base_mod._write_chunk

async def main():
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o666)
    stub = _StubBackend(fd)
    await OpenCodeBackend._stream_stdout(stub)  # warm, as a run's first bytes; not timed
    times = []
    for _ in range(5):
        stub = _StubBackend(fd)
        t0 = time.perf_counter()
        await OpenCodeBackend._stream_stdout(stub)
        times.append(time.perf_counter() - t0)
    times.sort()
    print(f"chunk {CHUNKS} chunks in {times[2] * 1000:.2f} ms median; per-chunk "
          f"{times[2] / CHUNKS * 1e6:.1f} us, max {times[-1] / CHUNKS * 1e6:.1f} us over 5 pump rounds")

    async def line_one():
        await helper(fd, line)
    for _ in range(5):
        await line_one()  # warm, as the startup wait's first lines; not timed
    times = []
    for _ in range(50):
        t0 = time.perf_counter()
        await line_one()
        times.append(time.perf_counter() - t0)
    times.sort()
    print(f"line median {times[24] * 1e6:.0f} us, max {times[-1] * 1e6:.0f} us over 50")
    os.close(fd)

asyncio.run(main())
EOF
```

M91 — worker per-event quota-scan head, streamed-turn replay. The worker's
`_process_event` runs per streamed event on the worker's loop beside the append
and broadcast; its quota-pattern check read both payload fields through
`str().lower()` copies on every event although only ERROR events can match. The
collector drives the real `_process_event` in file order over the worst on-disk
worker events log (read-only) with the append pointed at a scratch log under
/tmp (the run's held-fd shape, the M82 collector's) and the real broadcast
manager (zero subscribers), timing each call: one cold pass, as at a run's
first event, then five timed replays. The worst single event carries the scan
(the corpus's biggest payload); the per-event median reads the append floor the
M82 row documents and is expected to hold.

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, json, os, shutil, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.agents.worker import Worker
from src.core.config import CharlieBotConfig
from src.core.models import ThreadMetadata

# Worst per-event corpus: the largest on-disk worker events log (read-only).
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for p in root.glob("*/threads/*/data/events.jsonl"):
    n = p.stat().st_size
    if n > best_n:
        best, best_n = p, n
events = []
with best.open("rb") as f:
    for line in f:
        try:
            events.append(json.loads(line))
        except ValueError:
            continue

# Isolation: the append target is a scratch log under /tmp, never the live home.
scratch = Path(tempfile.mkdtemp(prefix="m91-head-"))
cfg = CharlieBotConfig(charliebot_home=scratch)
log_path = scratch / "events.jsonl"
worker = Worker(ThreadMetadata.model_construct(id="m91"), scratch, log_path, "", cfg)

async def replay():
    fd = os.open(log_path, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o666)
    times = []
    try:
        for ev in events:
            t0 = time.perf_counter()
            await worker._process_event(ev, fd)
            times.append(time.perf_counter() - t0)
    finally:
        os.close(fd)
    return times

async def main():
    await replay()  # cold pass, as at a run's first event; not timed
    walls = []
    per_event = []
    for _ in range(5):
        times = await replay()
        walls.append(sum(times))
        per_event.extend(times)
    walls.sort()
    per_event.sort()
    n = len(per_event)
    print(f"{best_n / 1e6:.1f} MB / {len(events)}-event worst worker log; full-corpus _process_event "
          f"replay median {walls[2]:.4f} s, max {walls[-1]:.4f} s over 5; per-event median "
          f"{per_event[n // 2] * 1e6:.1f} us, worst single event {per_event[-1] * 1e3:.2f} ms over {n}")

asyncio.run(main())
shutil.rmtree(scratch)
EOF
```

M92 — CLI invocation startup, common-family command. Every `charliebot` invocation is a
fresh Python process, and the master and workers run several per turn (memory queries,
plan verbs, delegate, schedule-trigger), so the shared module's import chain is the
per-call floor they all pay. The collector times `schedule-trigger --help` — argparse
exits before any request, so the reading is pure import+dispatch, deterministic, and
independent of server state — with the checkout under test resolved cwd-first (the
editable-install finder sits behind PathFinder, so a subprocess with cwd at the
checkout imports that checkout's code). `CHECKOUT` at the worktree root reads the
branch, the same shape as the M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import os, subprocess, sys, time

CHECKOUT = os.environ["CHECKOUT"]
CODE = "import sys; from src.cli.main import main; sys.exit(main())"

def wall(args):
    t0 = time.perf_counter()
    subprocess.run([sys.executable, "-c", CODE, *args], cwd=CHECKOUT,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True)
    return time.perf_counter() - t0

wall(["schedule-trigger", "--help"])  # warm the interpreter's own page cache; not timed
times = sorted(wall(["schedule-trigger", "--help"]) for _ in range(7))
print(f"checkout {os.path.basename(CHECKOUT)}: schedule-trigger --help (import+dispatch floor, "
      f"no server call) median {times[3]:.3f} s, max {times[-1]:.3f} s over 7")
EOF
```

M93 — thread-detail 500s. The workers panel polls `GET /api/threads/{sid}/threads/{tid}`
(`?attach=1` every 5 s per expanded thread row, full row on click); a 500 here fails that
poll continuously while the panel is open and each failure ships a ~30-line traceback into
the server log beside the structured `http_request … status=500` line the count matches.
The count reads the newest server log (its filename carries the server start time) and
appends `|| [ $? -eq 1 ]` for the same reason M11's count does: `grep -c` exits 1 on the
healthy zero count, and a real grep failure (exit 2) must stay loud:

```bash
LOG=$(ls -1t /tmp/charliebot-logs/server_*.log | head -1); grep -cE "method=GET path=/api/threads/[0-9a-f-]+/threads/[0-9a-f-]+ status=500" "$LOG" || [ $? -eq 1 ]
```

M94 — projection page + stream-delta serialization, giant-tool-output corpus. One message's
`tools` array rides every render path: each page payload (bootstrap, events, view) and each
`stream` delta re-serializes the whole buffered draft, so an unbounded tool row turns one big
tool_result into megabytes on every page build and every switch back to the session (the live
log's worst bootstrap tail: 890 ms, session 4914c102's 9.92 MB Bash output). Every chat wire
shape carries the renderer's preview bound (TOOL_PREVIEW_CHARS, trimmed at ingestion — the
stream delta, the committed message behind the events pages, and the bootstrap payload); the
persisted event keeps the full text. The collector resolves the active session whose live chat file carries the largest single
`tool_result` content (live home read-only; the projection build and the aggregator feed are
pure), builds the projection, times the tail-40 page's json.dumps, and replays the corpus through
the live broadcast shape (one json.dumps per emitted delta). Evidence points the same collector at
the before and after checkouts (`CHECKOUT` at each root), the same shape as the M7 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import json, os, sys, time
sys.path.insert(0, os.environ["CHECKOUT"])
from pathlib import Path
from src.core.message_aggregator import MessageAggregator
from src.core.message_projection import MessageProjection

root = Path.home() / ".charliebot" / "sessions"
best, best_out = None, -1
for d in root.iterdir():
    meta = d / "metadata.json"
    if not meta.is_file():
        continue
    try:
        if json.loads(meta.read_text()).get("status") != "active":
            continue
    except (OSError, ValueError):
        continue
    p = d / "data" / "chat_events.jsonl"
    if not p.is_file():
        continue
    for line in open(p, errors="replace"):
        if '"tool_result"' not in line:
            continue
        try:
            e = json.loads(line)
        except json.JSONDecodeError:
            continue
        if e.get("type") == "tool_result" and isinstance(e.get("content"), str) and len(e["content"]) > best_out:
            best, best_out = p, len(e["content"])

events = [json.loads(l) for l in open(best, errors="replace")]

def replay():
    agg = MessageAggregator(emit_stream_deltas=True)
    total = 0
    t0 = time.perf_counter()
    for ev in events:
        for delta in agg.feed(ev):
            total += len(json.dumps(delta, default=str))
    return time.perf_counter() - t0, total

def page():
    t0 = time.perf_counter()
    proj = MessageProjection(events)
    build = time.perf_counter() - t0
    messages, _, _ = proj.tail(40)
    t0 = time.perf_counter()
    body = json.dumps({"messages": messages}, default=str)
    return build, len(body), time.perf_counter() - t0

replay()  # cold pass; not timed
walls, totals = [], []
for _ in range(3):
    w, total = replay()
    walls.append(w)
    totals.append(total)
walls.sort()
totals.sort()
pages = [page() for _ in range(3)]
builds = sorted(p[0] for p in pages)
bodies = sorted(p[1] for p in pages)
dumps = sorted(p[2] for p in pages)
print(f"{best.parent.parent.name} corpus, {len(events)} events, largest tool output {best_out / 1e6:.2f} MB; "
      f"page body median {bodies[1] / 1e6:.2f} MB, dumps median {dumps[1] * 1000:.1f} ms, build median {builds[1] * 1000:.1f} ms; "
      f"streamed replay serialized median {totals[1] / 1e6:.1f} MB, dumps wall median {walls[1] * 1000:.0f} ms over 3")
EOF
```

M95 — worker-log newest-first scans: the reviewer-completion worker-summary scan and the failed
improve iteration's judgment pair. Both scan the worker's events log newest-first and stop at the
first answer — the review scan at the first non-empty result-or-assistant text, the pair at the
first quota-shaped event (or exhaustion) plus the first summary text — so a corpus whose answer
sits in the tail never reads the older bytes, and the pair's no-match exhaustion is the one shape
that walks the whole log; since the 2026-09-15 landing the walk parses only candidate-typed lines
(the raw-line type prefilter) and skips the non-candidate lines' join, so the multi-megabyte
tool_result lines ride the walk's reads but never its parse (the metric's floor). The cost is
thread-pool time invisible to HTTP probes, so the collector resolves the largest on-disk worker
log (read-only), one cold pass per shape, then five timed rounds of each shape, from the checkout
under test. Evidence while the live
server runs older code points the same collector at the branch checkout (``CHECKOUT`` at the
worktree root), the same shape as the M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import os, sys, time
sys.path.insert(0, os.environ["CHECKOUT"])
from pathlib import Path
from src.core.improve_command import _failed_iteration_judgments, _newest_first_events
from src.core.review import _worker_summary_from_events_log

root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for p in root.glob("*/threads/*/data/events.jsonl"):
    n = p.stat().st_size
    if n > best_n:
        best, best_n = p, n

def review_round():
    t0 = time.perf_counter()
    chosen = _worker_summary_from_events_log(best)
    return time.perf_counter() - t0, chosen

def pair_round():
    t0 = time.perf_counter()
    blocker, summary = _failed_iteration_judgments(_newest_first_events(best), 1, "failed")
    return time.perf_counter() - t0, blocker, summary

review_round()  # cold pass, as at the first reviewer completion after a server start; not timed
pair_round()
rtimes, ptimes = [], []
r = p = None
for _ in range(5):
    out = review_round()
    rtimes.append(out[0]); r = out[1:]
    out = pair_round()
    ptimes.append(out[0]); p = out[1:]
rtimes.sort(); ptimes.sort()
print(f"{best_n / 1e6:.1f} MB worker log; newest-first scans: review-scan median {rtimes[2] * 1000:.2f} ms "
      f"(summary {len(r[0] or '')} chars), failed-iteration judgment pair median {ptimes[2] * 1000:.2f} ms "
      f"(blocker {p[0]}, iteration summary {len(p[1])} chars), maxima {rtimes[-1] * 1000:.2f}/{ptimes[-1] * 1000:.2f} ms over 5")
EOF
```

M96 — switch-bootstrap chat payload, active-session sweep. The SPA switch
(`switchSession`, web/static/js/sidebar/session-view.js) fetches
``GET /api/sessions/{id}/bootstrap`` inside its started→completed window (the
index page's embedded SESSION_BOOTSTRAP is the same payload), and the live
``diag_switch`` telemetry (1751 switched sessions per 40 h on the 2026-09-12
log, joined to their bootstrap server lines) reads: server 6 % of the switch,
client transfer + parse + mount 94 % — median 164 ms elapsed against a 14 ms
server call. The payload's weight is the messages' ``tools`` arrays (95 % of the
body before the trim), each carrying whole input and output although the
renderer displays only a bounded preview (an output's first 500 characters
plain, an input's summary line — 80 chars for Bash, 60 for other named tools,
the full file path/pattern for the file tools) inside a block hidden behind the
"N tool calls" toggle, inside turns that mostly render folded. The collector
measures the body bytes of every active session's bootstrap (live home
read-only); the healthy range is set from the post-trim body. Evidence while the
live server runs older code is a scratch-instance A/B: live-before GETs against
the running server, scratch-after GETs through a TestClient on the changed
checkout with a scratch ``CHARLIEBOT_HOME`` holding the same active sessions
(metadata + data, ``master_runs`` excluded), asserting every message's
non-tools fields byte-identical across arms and every trimmed tool a strict
prefix with its truncation marker set.

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import json, statistics, sys, urllib.request
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from src.core.config import get_credentials

KEY = get_credentials().require("charliebot", "access_key")
BASE = "http://127.0.0.1:18498"

def get(url):
    req = urllib.request.Request(url, headers={"Authorization": f"Bearer {KEY}"})
    return urllib.request.urlopen(req, timeout=20).read()

sessions = json.loads(get(BASE + "/api/sessions/"))
sizes = []
for s in sessions:
    if s.get("worker_thread"):
        # A projected worker-leaf row's id is a thread id; the SPA switch fetches
        # bootstrap only for session rows (a leaf click rides the parent's pane).
        continue
    sizes.append(len(get(f"{BASE}/api/sessions/{s['id']}/bootstrap")))
sizes.sort()
n = len(sizes)
print(f"{n} session rows; bootstrap body bytes: median {sizes[n // 2]}, p90 {sizes[int(n * 0.9)]}, "
      f"max {sizes[-1]}, total {sum(sizes)}")
EOF
```

M97 — plan-CLI command wall, common-family verb. Every plan verb the master delivers and every registry inspection is a `charliebot plan` invocation — a fresh process whose import chain used to drag the server's validation stack (artifact check → backends registry → numpy, fastapi) for one helper import, plus the pydantic model stack for two argparse choices tuples. The collector times the real read-side command (a GET against the live server; the vocabularies ride stdlib-only `src.core.constants`, so parser build stays on the floor), from the checkout under test resolved cwd-first — the same shape as the M92 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import os, subprocess, sys, time
from pathlib import Path

CHECKOUT = os.environ["CHECKOUT"]
SID = None
# Worst plans corpus: the session whose plans.json carries the most bytes (the M27 rule);
# the GET is read-only.
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    p = d / "plans.json"
    if p.is_file():
        n = p.stat().st_size
        if n > best_n:
            best, best_n = p, n
SID = best.parent.name

CODE = "import sys; from src.cli.plan import main; sys.exit(main())"

def wall():
    t0 = time.perf_counter()
    subprocess.run([sys.executable, "-c", CODE, "list", "--session", SID], cwd=CHECKOUT,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True)
    return time.perf_counter() - t0

wall()  # warm the interpreter's own page cache; not timed
times = sorted(wall() for _ in range(7))
print(f"checkout {os.path.basename(CHECKOUT)}: plan list --session {SID} "
      f"({best_n / 1e3:.1f} KB plans.json) median {times[3]:.3f} s, max {times[-1]:.3f} s over 7")
EOF
```

M98 — memory-CLI invocation wall, read verb. Every memory read the cron instructions and master
turns issue on demand is a `charliebot memory` invocation: a fresh process. Its import chain
must stay off structlog.dev (rich, pygments, the traceback formatter) — the read path emits no
log line, so the logging stack loads on the first log call, the deferral the memory chain's
import-weight ban set pins. The collector times the real read command against the live store
(read-only), from the checkout under test resolved cwd-first — the same shape as the M97 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import os, subprocess, sys, time

CHECKOUT = os.environ["CHECKOUT"]
CODE = "import sys; from src.cli.memory import main; sys.exit(main())"

def wall():
    t0 = time.perf_counter()
    subprocess.run([sys.executable, "-c", CODE, "query", "--topic", "charliebot", "--index"], cwd=CHECKOUT,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True)
    return time.perf_counter() - t0

wall()  # warm the interpreter's own page cache; not timed
times = sorted(wall() for _ in range(7))
print(f"checkout {os.path.basename(CHECKOUT)}: memory query --topic charliebot --index "
      f"median {times[3]:.3f} s, max {times[-1]:.3f} s over 7")
EOF
```

M99 — server import floor, fresh process. Every deploy restarts the server process, and the
restart's first cost is `import server` — the module uvicorn imports, whose chain reaches every
router and core stack. The speech stack (numpy via `src.agents.transcriber`, plus the two numpy
SIMD scanners `src.core.ndjson` and `src.core.sessions` carry) serves only background model
provisioning, voice sockets, and the fork's parent-reference stream, so it must load on the
provisioning thread and at the voice use sites instead of the startup path; the import-weight
contract's server case (tests/test_cli_import_weight.py) pins the absence. The collector times
the import wall over five fresh processes per round, from the checkout under test:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import os, statistics, subprocess, sys, time

checkout = os.environ["CHECKOUT"]
code = "import time; t0 = time.perf_counter(); import server; print(f'{time.perf_counter()-t0:.3f}')"

def run_once():
    out = subprocess.run([sys.executable, "-c", code], cwd=checkout, check=True,
                         capture_output=True, text=True).stdout.strip()
    return float(out.split()[0])

run_once()  # warm the page cache; not timed
times = sorted(run_once() for _ in range(5))
print(f"checkout {os.path.basename(checkout)}: import server median "
      f"{statistics.median(times):.3f} s, max {times[-1]:.3f} s over 5")
EOF
```

M100 — run-start session-adopt signal, worker-log read trip and wire. Every
opencode/codex/gemini/charlie-code/antigravity run opens its event stream with the
session-adopt signal naming the attached session id. The master funnel persists it as the chat
history's run-start marker (the stable-history projection's interval key — load-bearing, its
durable append is by design) and the worker funnel keeps it as the worker log's session-id
record (the token tally's codex reconciliation reads the id from the raw line). Neither funnel
renders it: a signal line without a type fails WorkerEvent validation on every cold
read+transform of that log and renders a `type='raw'` row in the workers panel, and the worker
funnel broadcasts a frame no subscriber reads. The signal writes state, so the collector drives
both funnels over a scratch `CHARLIEBOT_HOME` under /tmp (scratch chat file and scratch worker
log; live home untouched): the master funnel through the real `persist_and_broadcast` (the
marker-line parity witness), the worker funnel through a real Worker on a real O_APPEND fd with
the broadcast seam counted, then one cold read+transform of the 51-signal log (one signal per
production log's head, replayed to measurable scale). The signal shape is the checkout's own —
the typed `ET.SESSION_ATTACHED` event where the constant exists, the bare `{"session_id": …}`
dict where the checkout predates it — asserted loud in both directions (a checkout that
declares the constant without emitting it fails the collector instead of timing a shape
nothing produces). Evidence points the same collector at the before and after checkouts
(`CHECKOUT` at each root), the same shape as the M7 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, inspect, os, shutil, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.agents.master_cc_run import _handle_event
from src.agents.worker import Worker
from src.api.threads import read_thread_worker_events
from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest, ThreadMetadata
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager
from src.core import streaming as streaming_mod
import src.agents.backends.opencode as oc_mod

# The run-start signal this checkout's backends yield: the typed
# SESSION_ATTACHED event where the constant exists, the bare session-adopt
# dict where the checkout predates it. The pairing is loud: a checkout that
# declares the constant but does not emit it from the opencode backend fails
# the collector instead of timing a shape nothing produces.
attach_type = getattr(ET, "SESSION_ATTACHED", None)
if attach_type is None:
  SIGNAL = {"session_id": "oc-attach-probe"}
else:
  if "ET.SESSION_ATTACHED" not in inspect.getsource(oc_mod):
    raise SystemExit("checkout declares SESSION_ATTACHED but its opencode backend does not emit it")
  SIGNAL = {"type": attach_type, "session_id": "oc-attach-probe"}

# Isolation: scratch CHARLIEBOT_HOME under /tmp; the chat append target is the
# scratch session's own chat file, the worker append target a scratch log;
# live home untouched.
home = Path(tempfile.mkdtemp(prefix="m100-attach-home-", dir="/tmp"))
cfg = CharlieBotConfig(charliebot_home=home,
                       backends={"options": [{"id": "m100", "label": "M100", "type": "cc-claude",
                                              "model": "claude-opus-4-6"}]})
sessions = SessionManager(cfg)
threads = ThreadManager(cfg)


async def main():
  session = await sessions.create_session(CreateSessionRequest(name="M100"))
  meta = await threads.create_thread(session, "m100")

  # Master funnel parity witness: the signal persists as the chat history's
  # run-start marker (the stable-history projection's interval key) on both
  # shapes — the durable append is load-bearing, not waste.
  await _handle_event(dict(SIGNAL), session.id, None, sessions.persist_and_broadcast)  # cold pass; not timed
  for _ in range(5):
    await _handle_event(dict(SIGNAL), session.id, None, sessions.persist_and_broadcast)
  chat_path = home / "sessions" / session.id / "data" / "chat_events.jsonl"
  marker_lines = sum(1 for _ in chat_path.open(errors="replace"))
  captured = await _handle_event(dict(SIGNAL), session.id, None, sessions.persist_and_broadcast)

  # Worker funnel: the signal's append, broadcast, and read trip. The
  # broadcast counter rides the streaming module the funnel calls; the log
  # carries one line per signal, the shape every production worker log has.
  log_path = home / "sessions" / session.id / "threads" / meta.id / "data" / "events.jsonl"
  worker = Worker(ThreadMetadata.model_construct(id=meta.id), home, log_path, "", cfg)
  broadcasts = []
  orig_broadcast = streaming_mod.streaming_manager.broadcast

  async def counting_broadcast(channel, event):
    broadcasts.append(channel)
    await orig_broadcast(channel, event)

  streaming_mod.streaming_manager.broadcast = counting_broadcast
  fd = os.open(log_path, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o666)
  try:
    await worker._process_event(dict(SIGNAL), fd)  # cold pass; not timed
    worker_times = []
    for _ in range(50):
      t0 = time.perf_counter()
      await worker._process_event(dict(SIGNAL), fd)
      worker_times.append(time.perf_counter() - t0)
  finally:
    os.close(fd)
    streaming_mod.streaming_manager.broadcast = orig_broadcast
  worker_times.sort()
  worker_lines = sum(1 for _ in log_path.open(errors="replace")) if log_path.is_file() else 0

  # Read trip: the projection over the written log. A type-less signal line
  # fails WorkerEvent validation per line (pydantic error construction plus
  # the debug emit) and renders a type='raw' row; the typed line is skipped
  # before row construction.
  t0 = time.perf_counter()
  rows = read_thread_worker_events(log_path)
  read_wall = time.perf_counter() - t0
  raw_rows = sum(1 for r in rows if r.type == "raw")

  print(f"checkout {os.environ['CHECKOUT'].rsplit('/', 1)[-1]} signal "
        f"{'typed ' + str(attach_type) if attach_type else 'bare'}: chat marker lines {marker_lines} "
        f"(captured {captured!r}); worker append median {worker_times[25] * 1e6:.1f} us, max "
        f"{worker_times[-1] * 1e6:.1f} us over 50; broadcast frames per signal {len(broadcasts)}; "
        f"worker-log lines {worker_lines}; cold read+transform wall {read_wall * 1000:.2f} ms, "
        f"raw rows {raw_rows} of {len(rows)} rows")
  shutil.rmtree(home)


asyncio.run(main())
EOF
```

M101 — raw events download, gzip-accepted. The events viewer page fetches the
session's whole `chat_events.jsonl` and the page's download link points at the
same endpoint, and the browser sends `Accept-Encoding: gzip` on both. Starlette's
FileResponse streams the file in 64 KiB chunks and the gzip middleware
compresses every chunk inline on the event loop — ~11 ms worst loop gap per
chunk on the 36 MB worst corpus, 1.3 s of server-side wall per download — while
a pre-compressed body with Content-Encoding set upstream skips the middleware's
pass entirely (the M72 listing-serve mechanism) and moves the read+compress to
one executor hop behind a stat-keyed memo. The collector drives the real app
stack (gzip + auth middleware) raw-ASGI with a concurrent 5 ms ticker against a
scratch `CHARLIEBOT_HOME` (its config carries an empty access key, which the
auth middleware passes through) holding a copy of the worst on-disk live events
corpus (metadata.json and data/, live home read once for the copy, never
written): one first view, as at a fresh events-viewer open (the cold
read+compress; not part of the steady-state medians), then nine timed
steady-state downloads of the unchanged file — the repeat-serve shape the memo
serves. Evidence points the same collector at the before and after checkouts
(`CHECKOUT` at each root), the same shape as the M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, os, shutil, sys, tempfile, time
from pathlib import Path

sys.path.insert(0, os.environ["CHECKOUT"])

# Worst download corpus: the session whose live chat file carries the most bytes.
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    p = d / "data" / "chat_events.jsonl"
    if p.is_file():
        n = p.stat().st_size
        if n > best_n:
            best, best_n = d, n
SID = best.name
print(f"worst download corpus: session {SID}, {best_n / 1e6:.1f} MB")

# Isolation: scratch CHARLIEBOT_HOME under /tmp holding a copy of that session's
# metadata.json and data/ (live home read once for the copy, never written); the
# scratch credentials carry an empty access key, which the auth middleware passes through.
home = Path(tempfile.mkdtemp(prefix="m101-events-home-"))
(home / "sessions" / SID).mkdir(parents=True)
shutil.copy2(best / "metadata.json", home / "sessions" / SID / "metadata.json")
shutil.copytree(best / "data", home / "sessions" / SID / "data")
(home / "credentials.yaml").write_text("charliebot:\n  access_key: ''\n")
os.environ["CHARLIEBOT_HOME"] = str(home)

import server  # noqa: E402  (the real app stack: _CharlieBotGZipMiddleware + AuthMiddleware)

SCOPE = {
    "type": "http", "asgi": {"version": "3.0", "spec_version": "2.3"},
    "http_version": "1.1", "method": "GET", "scheme": "http",
    "path": f"/api/sessions/{SID}/events.jsonl",
    "raw_path": f"/api/sessions/{SID}/events.jsonl".encode(),
    "query_string": b"", "root_path": "",
    "headers": [(b"host", b"test"), (b"accept-encoding", b"gzip")],
    "client": ("test", 123), "server": ("test", 80),
}


async def drive():
    body = b""

    async def receive():
        return {"type": "http.request", "body": b"", "more_body": False}

    async def send(msg):
        nonlocal body
        if msg["type"] == "http.response.body":
            body += msg.get("body", b"")

    t0 = time.perf_counter()
    await server.app(SCOPE, receive, send)
    return time.perf_counter() - t0, body


async def main():
    await drive()  # first view: the cold read+compress a fresh open pays; not timed
    worst, walls, body = [], [], b""
    for _ in range(9):
        stop = False
        gaps = []

        async def ticker():
            prev = time.perf_counter()
            while not stop:
                await asyncio.sleep(0.005)
                now = time.perf_counter()
                gaps.append(now - prev)
                prev = now

        t = asyncio.create_task(ticker())
        dt, body = await drive()
        stop = True
        await t
        worst.append(max(gaps) if gaps else dt)
        walls.append(dt)
    worst.sort()
    walls.sort()
    print(f"{best_n / 1e6:.1f} MB file, {len(body) / 1e6:.1f} MB gzip wire; "
          f"loop-lag median {worst[4] * 1000:.2f} ms, max {worst[-1] * 1000:.2f} ms; "
          f"steady-state wall median {walls[4] * 1000:.1f} ms, max {walls[-1] * 1000:.1f} ms over 9")
    shutil.rmtree(home)


asyncio.run(main())
EOF
```

M102 — artifact-CLI command wall, wrap verb. Every plan page the master ships is a
`charliebot artifact wrap` invocation — a fresh process whose import chain used to drag the
probe's registry stack (artifact_check → backends registry → fastapi, sessions) and the
KaTeX fetch's HTTP client for module-scope imports the wrap verb never exercises (genre plan
pre-renders no math, and the vendored-KaTeX steady state never fetches). The collector times the
real assembly command from the checkout under test resolved cwd-first (the same shape as the M97
protocol): a scratch fragment and scratch output under /tmp, no live-home write:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import os, subprocess, sys, tempfile, time
from pathlib import Path

CHECKOUT = os.environ["CHECKOUT"]
work = Path(tempfile.mkdtemp(prefix="m102-wrap-"))
fragment = work / "fragment.html"
fragment.write_text("<section><h2>Probe</h2><p>latency-perf M102 wrap-wall probe paragraph.</p></section>", encoding="utf-8")
output = work / "page.html"

CODE = "import sys; from src.cli.artifact import main; sys.exit(main())"

def wall():
    t0 = time.perf_counter()
    subprocess.run([sys.executable, "-c", CODE, "wrap", str(fragment), "--genre", "plan",
                    "--output", str(output)], cwd=CHECKOUT,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True)
    return time.perf_counter() - t0

wall()  # warm the interpreter's own page cache; not timed
times = sorted(wall() for _ in range(7))
print(f"checkout {os.path.basename(CHECKOUT)}: artifact wrap --genre plan "
      f"median {times[3]:.3f} s, max {times[-1]:.3f} s over 7")
EOF
```

M103 — config-dependency resolution on the routes that kept the sync `Depends(get_config)`: a
sync dependency is one FastAPI threadpool round-trip per request (the M34/M52/M82 rows' 67-104 µs
no-op hop floor, queueing-amplified under load), which `get_config_on_loop` — the awaited form the
polled routes already take — removes; the collector drives the three read shapes raw-ASGI over a
scratch empty corpus (the routes' DI + render floor, no live-corpus variance), and the zero-sync-sites
half is the route-walk guard test's job:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, os, shutil, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from fastapi import FastAPI
import src.api.deps as deps
from src.api.git import router as git_router
from src.api.pages import router as pages_router
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager

home = Path(tempfile.mkdtemp(prefix="m103-cfgdep-", dir="/tmp"))
(home / "credentials.yaml").write_text("charliebot:\n  access_key: testkey\n")
cfg = CharlieBotConfig(charliebot_home=home, paths={"workspace_dirs": [str(home)]})
app = FastAPI()
app.include_router(pages_router)
app.include_router(git_router, prefix="/api/git")
app.dependency_overrides[deps.get_session_manager] = lambda: SessionManager(cfg)
# The config dependency must serve the scratch cfg too: without this override
# the routes resolve the live config's workspace_dirs and code-server probe,
# and the drive reads the live corpus its isolation declares away.
app.dependency_overrides[deps.get_config_on_loop] = lambda: cfg

async def drive(path, n=120):
    status = None
    scope = {"type": "http", "asgi": {"version": "3.0"}, "http_version": "1.1", "method": "GET",
             "path": path, "raw_path": path.encode(), "query_string": b"", "headers": [],
             "server": ("127.0.0.1", 80), "client": ("127.0.0.1", 1), "scheme": "http"}
    async def receive():
        return {"type": "http.request", "body": b"", "more_body": False}
    async def send(message):
        nonlocal status
        if message["type"] == "http.response.start":
            status = message["status"]
    await app(scope, receive, send)
    assert status == 200, (path, status)
    times = []
    for _ in range(n):
        t0 = time.perf_counter()
        await app(scope, receive, send)
        times.append(time.perf_counter() - t0)
    times.sort()
    return times[len(times) // 2] * 1e6, times[int(n * 0.9)] * 1e6

async def main():
    rows = []
    for label, path in [("GET /diff", "/diff"), ("GET /", "/"), ("GET /api/git/repos", "/api/git/repos")]:
        p50, p90 = await drive(path)
        rows.append(f"{label} median {p50:.0f} us, p90 {p90:.0f} us")
    shutil.rmtree(home)
    print("; ".join(rows))

asyncio.run(main())
EOF
```

M104 — backend tail-follow cursor checkpoint, per line. Every consumed line of the
tail-follow loop checkpoints the byte offset to the mount's ``agent.raw.cursor`` so a
server restart re-attaches without replaying delivered lines; the per-line write is
invisible to the standing parse metric (M84's replay disables the cursor) and its cost
is storage-shaped, so the collector drives the real ``tail_follow_events`` over a
scripted 2000-line scratch stream with a real cursor file under /tmp (live home
untouched), one warm pass, as at a first mount, then five timed drains, asserting the
recorded offset equals the consumed bytes. Evidence points the same collector at the
before and after checkouts (``CHECKOUT`` at each root), the same shape as the M89
protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, os, sys, tempfile, time
sys.path.insert(0, os.environ["CHECKOUT"])
from pathlib import Path
from src.agents.backends.base import tail_follow_events
from src.core.runs import CURSOR_NAME, read_raw_cursor

LINES = 2000

work = Path(tempfile.mkdtemp(prefix="m104-cursor-", dir="/tmp"))
raw = work / "agent.raw.ndjson"
raw.write_bytes(b"".join(
    b'{"type": "assistant", "seq": %d, "pad": "%s"}\n' % (i, b"y" * 60) for i in range(LINES)
))
cursor = work / CURSOR_NAME
total_bytes = raw.stat().st_size


async def drain():
    count = 0

    def translate(event):
        nonlocal count
        count += 1
        return [event]

    async for _ in tail_follow_events(
        raw, translate=translate, is_alive=lambda: False,
        cursor=cursor, start_offset=0, post_result_timeout=60.0,
    ):
        pass
    return count


async def main():
    await drain()  # warm, as at a first mount; not timed
    times = []
    count = 0
    for _ in range(5):
        t0 = time.perf_counter()
        count = await drain()
        times.append(time.perf_counter() - t0)
    times.sort()
    assert count == LINES, count
    assert read_raw_cursor(cursor) == total_bytes, (read_raw_cursor(cursor), total_bytes)
    print(f"checkout {os.environ['CHECKOUT'].rsplit('/', 1)[-1]}: {LINES}-line stream, cursor {total_bytes} B "
          f"recorded; per-line cursor checkpoint wall median {times[2] / LINES * 1e6:.1f} us, "
          f"max {times[-1] / LINES * 1e6:.1f} us over 5 drains")

asyncio.run(main())
import shutil
shutil.rmtree(work)
EOF
```

M105 — binary-file transport serve, gzip-accepted. The file server's plain FileResponse arm
serves any host file, and every gzip-accepting client (the browser's image loads and deck
downloads ride it) paid the middleware's inline per-chunk deflate on the event loop for
bodies whose format is already entropy-coded — measured on the served shapes: a 336 KB PNG
at 10.6-11.4 ms per view against 2.5-2.8 ms identity, a 727 KB pptx at ~19 ms against
~3.7 ms, the deflate buying 1.7-2.1 % of wire (random-data bodies only grow). The fix skips
transport compression for that media-type prefix list; text formats (html, json, svg, csv)
keep compressing and SSE stays excluded. The cost is per-view serve time invisible to the standing HTTP
probes, so the collector drives the real app stack raw-ASGI (`import server`, the production
middleware chain over the file server's FileResponse arm) with the auth middleware's gate
at its no-op — the scratch home pins an empty `charliebot_access_key`, because an
uncredentialed request reads 401 before the route and the empty key serves the credentialed
view's exact bytes — over the worst on-disk `.png` and `.pptx` under the
sessions tree plus the worst artifact page as the witness: one cold pass per corpus, then five
timed requests, reporting the serve wall, the wire bytes, and the transport header each answer
carried. Evidence points the same collector at the before and after checkouts (`CHECKOUT` at
each root; the middleware lives in server.py, so the arms differ exactly by the fix), the same
shape as the M35 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, os, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])

# The auth middleware's gate is a no-op on an empty configured key, while an
# uncredentialed request reads 401 before the file route; the scratch home pins
# the empty key so the drive carries the credentialed view's served shape.
home = tempfile.mkdtemp(prefix="m105-transport-home-", dir="/tmp")
Path(home, "credentials.yaml").write_text("charliebot:\n  access_key: ''\n", encoding="utf-8")
os.environ["CHARLIEBOT_HOME"] = home
import server  # the real app stack: the transport-gzip middleware over the file server

# Worst served corpora per family: the largest .png and .pptx under the live
# sessions tree's artifact dirs, plus the largest artifact page (the
# keep-compressing witness). Read-only; the URL shape is the canonical
# /absolute_filepath mount.
root = Path.home() / ".charliebot" / "sessions"
best = {}
for p in root.glob("*/artifacts/*"):
    if not p.is_file():
        continue
    suffix = p.suffix.lower()
    n = p.stat().st_size
    if suffix in (".png", ".pptx", ".html") and n > best.get(suffix, (0,))[0]:
        best[suffix] = (n, p)
for p in root.glob("*/artifacts/*/*"):
    if not p.is_file():
        continue
    suffix = p.suffix.lower()
    n = p.stat().st_size
    if suffix in (".png", ".pptx") and n > best.get(suffix, (0,))[0]:
        best[suffix] = (n, p)

def scope(url):
    return {
        "type": "http", "asgi": {"version": "3.0", "spec_version": "2.3"},
        "http_version": "1.1", "method": "GET", "scheme": "http",
        "path": url, "raw_path": url.encode(), "query_string": b"", "root_path": "",
        "headers": [(b"host", b"test"), (b"accept-encoding", b"gzip")],
        "client": ("test", 123), "server": ("test", 80),
    }

async def drive(url):
    body = b""
    out = {"status": 0, "encoding": b""}

    async def receive():
        return {"type": "http.request", "body": b"", "more_body": False}

    async def send(msg):
        nonlocal body
        if msg["type"] == "http.response.start":
            out["status"] = msg["status"]
            out["encoding"] = dict(msg.get("headers", [])).get(b"content-encoding", b"")
        elif msg["type"] == "http.response.body":
            body += msg.get("body", b"")

    t0 = time.perf_counter()
    await server.app(scope(url), receive, send)
    return time.perf_counter() - t0, body, out

async def main():
    for suffix in (".png", ".pptx", ".html"):
        entry = best.get(suffix)
        if entry is None:
            print(f"{suffix}: no corpus under the sessions tree; unmeasured")
            continue
        n, p = entry
        url = f"/absolute_filepath{p}"
        _, _, cold_out = await drive(url)  # cold pass; not timed
        assert cold_out["status"] == 200, (url, cold_out["status"])
        times, wire, enc = [], 0, b""
        for _ in range(5):
            dt, body, out = await drive(url)
            assert out["status"] == 200, (url, out["status"])
            times.append(dt)
            wire = len(body)
            enc = out["encoding"]
        times.sort()
        transport = "identity" if not enc else f"gzip ({wire} B wire)"
        print(f"{suffix} {n} B raw: serve median {times[2] * 1000:.2f} ms, max {times[-1] * 1000:.2f} ms, "
              f"wire {wire} B, transport {transport}")

asyncio.run(main())
EOF
```

M106 — switch-during-stream repaint: the synchronous paint a session switch performs on a
mid-stream pending draft. The switch's teardown path hides the streamed bubble and the
render re-shows the server's pending draft; when the re-show lands past the 200 ms
coalesce window the paint runs synchronously inside the switch span, and its cost is the
draft's markdown parse — incremental when the parse state survives the hide, a full
re-parse of the whole accumulated draft when it does not. The collector replays the
shape through the checkout's real usage.js/markdown-renderer.js and the page-pinned
marked build over the largest on-disk assistant draft (the M33 corpus): one standing
turn paint (not timed), then seven hide+re-show rounds, the draft grown one 200 B delta
per round as it is between two switches, each round past the coalesce window; the
painted frame must equal a direct full-draft parse. Evidence points the same collector
at the before and after checkouts (`CHECKOUT` at each root), the same shape as the M33
protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} node /home/chaoli/workspace/charlie-bot/tests/switch_stream_repaint_collector.js
```

M107 — multi-trace merged-trace build wall. The merged view's dir shape
(`GET /perfetto/merged?dir=…`) merges every `*.json` trace of a directory: the
pre-parallel form walked all traces sequentially inside one pool worker, so
the wall was the sum of N traces' parse+remap walks. The fixed form submits
one merge-pool task per trace and streams each member's fragment into the
single gzip run as its task completes — the wall becomes the slowest wave of
members, ids allocate inside per-member strides so parallel members never
collide, and the artifact stays the single-member deterministic gzip run
(`-n` keeps the isal igzip header's mtime 0). A member that parses as JSON but
carries no `traceEvents` array (an analysis sidecar the `*.json` glob
over-matches; the route's first-byte sniff cannot see it) skips with a logged
warning instead of failing the build, and a merge that skips every member
raises. The
cost is the first merged view of a trace dir (repeats serve the cache),
invisible to the standing HTTP probes, so the collector writes only to a
scratch `CHARLIEBOT_HOME` under /tmp (traces read in place, read-only) and
drives `_cached_merge` from the checkout under test: one cold pass, then
three timed builds, each round's cache entry dropped so every round pays the
build; the decompressed event identity (ph, name, pid, ts — the fields the
member form does not re-number) must match across rounds. The harness
materializes itself as a file because the spawn pool's workers re-import
`__main__`, which a stdin heredoc cannot provide.

```bash
mkdir -p /tmp/opencode && cat > /tmp/opencode/m107_collector.py <<'PYEOF'
import asyncio, gzip, hashlib, json, os, shutil, sys, tempfile, time

sys.path.insert(0, os.environ["CHECKOUT"])
from pathlib import Path
from src.api import pages


def find_corpus() -> tuple[Path, int]:
    # Worst trace dir: the directory under the documented roots (~/data, ~/scripts)
    # whose *.json traces carry the most bytes; the merged view's dir-merge shape.
    dirs = [d for root in ("data", "scripts") if (Path.home() / root).is_dir()
            for d in (Path.home() / root).rglob("*") if d.is_dir() and len(list(d.glob("*.json"))) > 1]
    best = max(dirs, key=lambda d: sum(p.stat().st_size for p in d.glob("*.json")))
    return best, sum(p.stat().st_size for p in best.glob("*.json"))


async def main(paths: list[Path], best_n: int, home: str) -> None:
    def event_identity(path: Path) -> tuple[str, int]:
        events = json.loads(gzip.decompress(path.read_bytes()))["traceEvents"]
        ident = [[e.get("ph"), e.get("name"), e.get("pid"), e.get("ts")] for e in events]
        return hashlib.sha1(json.dumps(ident).encode()).hexdigest()[:12], len(events)

    cache_dir = pages._perfetto_merge_cache_dir()
    await pages._cached_merge(paths, slim=False)  # cold pass, as at the first merged view; not timed
    times, digests, count = [], set(), 0
    for _ in range(3):
        for stale in cache_dir.glob("*.json.gz"):
            stale.unlink()
        t0 = time.perf_counter()
        artifact = await pages._cached_merge(paths, slim=False)
        times.append(time.perf_counter() - t0)
        digest, count = event_identity(artifact)
        digests.add(digest)
    times.sort()
    assert len(digests) == 1, f"unstable artifact across rounds: {digests}"
    print(f"{len(paths)} traces {best_n / 1e6:.1f} MB, {count} events; multi-trace merge build "
          f"median {times[1]:.2f} s, max {times[-1]:.2f} s over 3; artifact "
          f"{artifact.stat().st_size / 1e6:.1f} MB, event-identity digest {digests.pop()}")
    shutil.rmtree(home, ignore_errors=True)


if __name__ == "__main__":
    # The spawn pool's workers re-import this file as __main__; everything with
    # side effects stays under the guard so a worker import is defs only.
    home = tempfile.mkdtemp(prefix="m107-home-")
    os.environ["CHARLIEBOT_HOME"] = home  # scratch cache home; the live home read-only
    best_dir, best_n = find_corpus()
    print(f"worst multi-trace dir: {best_dir}, {best_n / 1e6:.1f} MB")
    asyncio.run(main(sorted(best_dir.glob("*.json")), best_n, home))
PYEOF
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python /tmp/opencode/m107_collector.py
```

M108 — claude-sub launch import+dispatch floor: seven fresh-process runs of the worker binary's
console script with a flag argv parse rejects, so the wall is the import-and-dispatch floor every
subscription-mode worker/reviewer launch pays before the claude CLI starts; the nonzero exit is
the assert that the parse ran and no launch work did. The checkout under test rides PYTHONPATH —
the venv's editable finder pins `src` to the main checkout regardless of cwd, so a worktree probe
without it measures main's code:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import os, subprocess, sys, time

checkout = os.environ["CHECKOUT"]
# The console script resolves src through the editable finder pinned to the main
# checkout, so the checkout under test rides PYTHONPATH (cwd-first import is the
# The console script resolves src through the editable finder pinned to the main
# checkout, so the checkout under test rides PYTHONPATH (cwd-first import is the
# `python -c` shape, not the launch shape); the script itself lives in the venv
# that owns this interpreter — worktree checkouts carry no .venv of their own.
env = {**os.environ, "PYTHONPATH": checkout}
script = os.path.join(os.path.dirname(sys.executable), "claude-sub")

times = []
for _ in range(7):
    t0 = time.perf_counter()
    proc = subprocess.run([sys.executable, script, "--m108-unsupported-probe-flag"],
                          capture_output=True, env=env)
    times.append(time.perf_counter() - t0)
    # The expected exit is the argv-parse rejection; a crash before the parse
    # also exits non-zero, so the message is the pass condition.
    if proc.returncode == 0 or b"unsupported claude-sub flag" not in proc.stderr:
        raise SystemExit(f"probe did not parse-and-reject: rc={proc.returncode} "
                         f"stderr={proc.stderr.decode(errors='replace')[:200]!r}")
times.sort()
print(f"claude-sub launch floor median {times[3]:.3f} s, max {times[-1]:.3f} s over 7 (checkout {checkout})")
EOF
```

M110 — remote ssh probe, warm-master steady state. The remote-probe family (the schedule-trigger
verify-on-create, the remote-pid waiter's probes, the remote sacct watch's probes, the host-auth
standing probe, the remote launch) takes every ssh argv from `ssh_cmd`, and each subprocess paid
one full ssh handshake — TCP + KEX + auth, ~0.85 s to this deployment's SLURM login host. The
probe family now rides one ControlMaster per (local user, host, port): probes multiplex over the
master while it lives, the master exits after 1200 s idle (the remote watch ladder's 600 s
plateau plus its ≤10 s noise stays inside, so a watched host's probes never re-master while the
watch lives), and a stale socket (a master killed uncleanly) costs one re-master on the next
probe. The cost is subprocess latency invisible to the HTTP probes; the verify-on-create probe
sits on every remote-watch trigger creation's request. The collector drives the real probe — a
read-only `sacct` query against the standing watches' SLURM login host (read-only; a probe never
writes) — through both argv shapes from the checkout under test: the plain pre-fix argv (every
probe a full handshake) and the module's `ssh_cmd` (master reuse; one timed re-master round after
a clean `ssh -O exit`, then five timed warm rounds), three interleaved rounds, asserting rc 0 on
every probe:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import os, subprocess, sys, time

sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.ssh import ssh_cmd
from src.core.timeouts import SSH_CONNECT_TIMEOUT

# The standing remote watches' SLURM login host; the watched job id rides the
# live watch (read-only sacct query — a probe never writes, and the job's state
# moving between arms is the remote's business: the pass condition is rc 0).
HOST = "host2"
JOB = "285547"
SACCT = f"sacct -j {JOB} -X -n -P --format=JobID,State,ExitCode"
# The pre-fix argv: every probe one full handshake (the shape the served code
# ran before the ControlMaster landing; kept verbatim as the reference arm).
# Both arms are full argv prefixes ending in the host; probe() appends the command.
PLAIN_ARGV = ["ssh", "-o", "BatchMode=yes", "-o", f"ConnectTimeout={SSH_CONNECT_TIMEOUT}", HOST]


def probe(argv: list[str]) -> tuple[float, int]:
    t0 = time.perf_counter()
    proc = subprocess.run([*argv, SACCT], capture_output=True)
    return time.perf_counter() - t0, proc.returncode


def mux_cleanup() -> None:
    # -O exit is an option, so it rides before the destination; the mux arm's
    # argv is [options..., HOST, command], and the cleanup drops the command.
    argv = ssh_cmd(HOST)
    subprocess.run([*argv[:-1], "-O", "exit", HOST], capture_output=True)


plain_medians, warm_medians, colds = [], [], []
for _ in range(3):
    plain = []
    for _ in range(5):
        dt, rc = probe(PLAIN_ARGV)
        assert rc == 0, f"plain probe rc {rc}"
        plain.append(dt)
    plain.sort()
    plain_medians.append(plain[2])

    mux_cleanup()
    dt, rc = probe(ssh_cmd(HOST))  # the re-master round: no master exists here
    assert rc == 0, f"re-master probe rc {rc}"
    colds.append(dt)
    warm = []
    for _ in range(5):
        dt, rc = probe(ssh_cmd(HOST))
        assert rc == 0, f"warm probe rc {rc}"
        warm.append(dt)
    warm.sort()
    warm_medians.append(warm[2])
mux_cleanup()
plain_medians.sort(); warm_medians.sort(); colds.sort()
print(f"remote ssh probe to {HOST}: plain median {plain_medians[1]:.3f} s, "
      f"warm-master median {warm_medians[1]:.3f} s, re-master median {colds[1]:.3f} s "
      f"over 3 interleaved rounds")
EOF
```

M111 — review-context chat-log scan, worker completion. Every worker and reviewer completion
runs `_first_delegation_description` over the session's live `chat_events.jsonl` (the reviewer
prompt's user-request line; the improve chain runs the same extract twice more), and the
pre-landing scan parsed every line text-mode from the file start — ~35-39 ms on the 20.1 MB worst
active corpus, the needle's position setting the parse count (the newest thread's delegation sits
at the file's tail). The reader skips by proof: a line whose bytes lack the thread id cannot name
it, so one C-level find rides the mapping and only a hit's enclosing line parses. The collector
times both shapes — the deepest needle (the last task_delegated's thread id) and an absent id —
over the worst active (non-archived) live chat corpus that carries at least one task_delegated
event (the scan's workload; a corpus without one never runs this scan for a thread), five runs
each:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import json, os, statistics, sys, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.review import _first_delegation_description

# Worst active live chat corpus: the non-archived session whose live chat
# file carries the most bytes among the files that hold at least one
# task_delegated event (the scan's workload: sessions whose workers complete
# name a delegation; a corpus without one never runs this scan for a thread).
root = Path.home() / ".charliebot" / "sessions"
best, best_size = None, -1
for d in root.iterdir():
    p = d / "data" / "chat_events.jsonl"
    if not p.is_file():
        continue
    meta = d / "metadata.json"
    if meta.is_file():
        try:
            if json.loads(meta.read_text()).get("status") == "archived":
                continue
        except (OSError, ValueError):
            pass
    has_delegation = False
    with open(p, "rb") as f:
        for line in f:
            if b'"task_delegated"' in line:
                has_delegation = True
                break
    if not has_delegation:
        continue
    n = p.stat().st_size
    if n > best_size:
        best, best_size = p, n

# Needle-at-end: the LAST task_delegated's thread id, the newest-thread
# completion shape whose match sits deepest in the file.
import orjson
tid = None
with open(best, "rb") as f:
    for line in f:
        if b'"task_delegated"' not in line:
            continue
        try:
            ev = orjson.loads(line)
        except ValueError:
            continue
        if ev.get("type") == "task_delegated" and ev.get("thread_id"):
            tid = ev["thread_id"]
ABSENT = "zzq9xneverpresentthread0000000000000000"

def median_scan(needle_id: str) -> float:
    times = []
    for _ in range(5):
        t0 = time.perf_counter()
        _first_delegation_description(best, needle_id)
        times.append(time.perf_counter() - t0)
    return statistics.median(times)

at_end = median_scan(tid)
absent = median_scan(ABSENT)
print(f"checkout {os.environ['CHECKOUT'].rsplit('/', 1)[-1]}: {best_size / 1e6:.1f} MB active live chat file "
      f"({tid[:8]} deepest needle); review-context scan median {at_end * 1000:.2f} ms needle-at-end, "
      f"{absent * 1000:.2f} ms absent-needle over 5 each")
EOF
```

M112 — backup archive build, whole-home corpus. `create_backup` compresses the whole profile
home (sessions `data/`, cache, memory, config.d — the gigabyte-scale sessions corpus included)
into one `.tar.gz` on every `backup` handler fire; the pre-fix form rode tarfile's `w:gz`
stdlib-zlib stream at its default level 9. The cost is an executor-thread wall invisible to
every standing probe (the handler may never fire on a given host), so the collector rebuilds
the scratch synthetic home — the isolation rule's fresh random ids, no `cc_session_id`, one
token threads subtree priced out by the backup's own exclusion — then times `create_backup`
from the checkout under test: one cold pass, as at the handler's first fire on a fresh host,
then three timed builds, each archive deleted after its reading. Corpus built once (the
committed builder rebuilds it from scratch each run):

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python /home/chaoli/workspace/charlie-bot/tests/backup_corpus_builder.py
```

Then run per checkout (`CHECKOUT` at the worktree root; the scratch home persists at
/tmp/opencode/m112/home):

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import os, sys, time
sys.path.insert(0, os.environ["CHECKOUT"])
os.environ["CHARLIEBOT_HOME"] = "/tmp/opencode/m112/home"
from pathlib import Path
from src.core.backup import create_backup

home = Path(os.environ["CHARLIEBOT_HOME"])
corpus = sum(p.stat().st_size for p in home.rglob("*") if p.is_file())

def one():
    t0 = time.perf_counter()
    archive = create_backup()
    dt = time.perf_counter() - t0
    wire = archive.stat().st_size
    archive.unlink()
    return dt, wire

one()  # cold pass, as at the handler's first fire on a fresh host; not timed
times = []
for _ in range(3):
    dt, wire = one()
    times.append(dt)
times.sort()
print(f"checkout {os.environ['CHECKOUT'].rsplit('/', 1)[-1]}: corpus {corpus / 1e9:.2f} GB, "
      f"archive {wire / 1e6:.0f} MB ({corpus / wire:.1f}x); "
      f"create_backup median {times[1]:.1f} s, max {times[-1]:.1f} s over 3, "
      f"{corpus / 1e6 / times[1]:.0f} MB/s effective")
EOF
```

M113 — voice transcription wall: the offline decode behind `POST /api/voice/*` (the
server log's slowest served path). The collector decodes the largest on-disk
recording (read-only) three times quiet and three times under 8 spinner
processes at the turn tree's nice — the contended shape a master turn's CLI and
tool subprocesses produce around a voice request — and decodes twice again to
assert determinism (the persisted transcripts of pre-upgrade recordings render
punctuation differently, so the parity witness is same-process determinism, not
stored-text equality):

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import os, subprocess, sys, time, wave
sys.path.insert(0, os.environ["CHECKOUT"])
from pathlib import Path
from src.core.config import CharlieBotConfig
from src.agents import transcriber
from src.agents.backends.base import TURN_TREE_NICE

best = max((Path.home() / ".charliebot" / "sessions").glob("*/voice/*.wav"), key=lambda p: p.stat().st_size)
with wave.open(str(best), "rb") as reader:
    pcm = reader.readframes(reader.getnframes())
audio_s = len(pcm) / 2 / transcriber.SAMPLE_RATE
cfg = CharlieBotConfig()
transcriber.provision_models(cfg)
bundle = transcriber._get_model_bundle(cfg, transcriber.get_ready_model_paths())

def decode_round() -> tuple[float, str]:
    t0 = time.perf_counter()
    text = transcriber.transcribe_pcm_offline(bundle, pcm)
    return time.perf_counter() - t0, text

def spawn_hogs(nice_value: int) -> list[subprocess.Popen]:
    code = f"import os\nos.nice({nice_value})\nwhile True: pass"
    procs = [subprocess.Popen(["python3", "-c", code]) for _ in range(8)]
    time.sleep(0.5)
    return procs

def stop_hogs(procs: list[subprocess.Popen]) -> None:
    for proc in procs:
        proc.kill()
    for proc in procs:
        proc.wait()

_, cold_text = decode_round()  # cold pass, as at the first voice request after a server start; not timed
_, rerun_text = decode_round()
quiet = []
for _ in range(3):
    dt, _ = decode_round()
    quiet.append(dt)
quiet.sort()
contended = []
for _ in range(3):
    procs = spawn_hogs(TURN_TREE_NICE)
    dt, _ = decode_round()
    stop_hogs(procs)
    time.sleep(1)
    contended.append(dt)
contended.sort()
print(f"{best.name}: {audio_s:.1f} s audio; quiet decode median {quiet[1]:.2f} s "
      f"(RTF {quiet[1] / audio_s:.2f}); contended 8 hogs @ nice {TURN_TREE_NICE} median "
      f"{contended[1]:.2f} s ({contended[1] / quiet[1]:.2f}x quiet); determinism {cold_text == rerun_text}")
EOF
```

M114 — backend-launch spawn loop stall, big-heap shape. Every backend spawn site
(`asyncio.create_subprocess_exec` pre-fix, the off-loop seam after) forks the calling
process synchronously on the event loop, and the fork's page-table copy scales with the
forking process's resident set (~55 us/MB measured on this host) — the multi-GB server
stalls every concurrent request and WebSocket for ~0.1-0.2 s on each master/worker
launch, invisible to the standing HTTP probes. The collector inflates a heap to the
server's standing RSS class, then drives the checkout's spawn seam in both production
shapes under a 5 ms ticker, from the checkout under test: the raw-log shape
(devnull stdin, stdout/stderr to file fds, preexec-free — the claude family's master
turns and every worker launch) and the piped shape (piped stdout/stderr through the
pdeathsig spawn seam, preexec-free — the piped transports and pdeathsig one-shots); one
cold pass, then five timed spawns per shape:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, os, sys, time
sys.path.insert(0, os.environ["CHECKOUT"])
import src.agents.backends.base as base_module

spawn = base_module.spawn_subprocess

GB = 3.5
blob = bytearray(int(GB * 1e9))
for i in range(0, len(blob), 4096):
    blob[i] = 1

raw_log = os.open("/tmp/opencode/m114_raw.log", os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o644)
raw_err = os.open("/tmp/opencode/m114_err.log", os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o644)

async def run_once(shape):
    gaps = []
    stop = False

    async def ticker():
        prev = time.perf_counter()
        while not stop:
            await asyncio.sleep(0.005)
            now = time.perf_counter()
            gaps.append(now - prev)
            prev = now

    t = asyncio.create_task(ticker())
    await asyncio.sleep(0.01)  # the ticker's first slice: a synchronous fork before it would escape the gap list
    t0 = time.perf_counter()
    if shape == "raw-log":
        proc = await spawn(
            "/bin/true",
            stdin=asyncio.subprocess.DEVNULL,
            stdout=raw_log,
            stderr=raw_err,
            env=dict(os.environ),
            limit=1024 * 1024,
            start_new_session=True,
            preexec_fn=None,
        )
    else:
        proc = await spawn(
            "/bin/true",
            stdin=asyncio.subprocess.DEVNULL,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            env=dict(os.environ),
            limit=1024 * 1024,
            start_new_session=True,
            preexec_fn=None,
            pdeathsig=True,
        )
    wall = time.perf_counter() - t0
    code = await proc.wait()
    stop = True
    await t
    assert code == 0
    return (max(gaps) if gaps else wall), wall

async def main():
    for shape in ("raw-log", "piped"):
        await run_once(shape)  # cold pass, as at the first spawn after a server start; not timed
        worst, walls = [], []
        for _ in range(5):
            gap, wall = await run_once(shape)
            worst.append(gap)
            walls.append(wall)
        worst.sort()
        walls.sort()
        print(f"checkout {os.environ['CHECKOUT'].rsplit('/', 1)[-1]}: {GB} GB inflated heap, {shape} spawn shape "
              f"(loop-lag median {worst[2] * 1000:.1f} ms, max {worst[-1] * 1000:.1f} ms, "
              f"wall median {walls[2] * 1000:.1f} ms over 5)")

asyncio.run(main())
EOF
```

M115 — cold config+credentials resolution, fresh process. Every CLI verb invocation pays the
credentials read (`get_credentials`), a verb on a config-cache miss (deploy, config edit, first
run) pays the full `get_config` resolution, every server start pays both, and every
config/credentials edit re-pays the parse through the reload keys (the M53/M58 fingerprint gates
make the steady state free; this prices the change rounds). The collector times the shared
import plus both resolutions in a fresh process, the same shape the M92-family collectors price
their walls with:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import os, subprocess

CHECKOUT = os.environ["CHECKOUT"]
PY = "/home/chaoli/workspace/charlie-bot/.venv/bin/python"
PROBE = '''
import sys, time
sys.path.insert(0, sys.argv[1])
t0 = time.perf_counter()
from src.core.config import get_config
from src.core.credentials import get_credentials
get_config()
get_credentials()
print(f"{time.perf_counter() - t0:.4f}")
'''

times = []
for _ in range(7):
    out = subprocess.run([PY, "-c", PROBE, CHECKOUT], capture_output=True, text=True, check=True)
    times.append(float(out.stdout.strip()))
times.sort()
print(f"checkout {os.path.basename(CHECKOUT)}: cold config+credentials resolution median "
      f"{times[3]:.4f} s, max {times[-1]:.4f} s over 7")
EOF
```

M116 — ndjson whole-file parse, worst live chat file by event count. The events cache, the
catch-up, the projection build, and the usage resolution all funnel their first load through
`parse_ndjson_file`; the worst-by-bytes corpus the M78 collector reads prices orjson's huge-line
work, so the per-line loop needs the event-count shape — every regular session's cold load. The
collector times the funnel over the live chat file carrying the most events (read-only), from the
checkout under test: one cold pass, as at the first events load after a server start, then twelve
timed calls:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import os, sys, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.ndjson import parse_ndjson_file

# Worst per-line-plumbing corpus: the live chat file carrying the most events; the
# by-bytes worst file (M78's corpus) prices huge-line orjson work instead.
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    p = d / "data" / "chat_events.jsonl"
    if p.is_file():
        with open(p, errors="replace") as f:
            n = sum(1 for _ in f)
        if n > best_n:
            best, best_n = d, n
path = best / "data" / "chat_events.jsonl"

parse_ndjson_file(path)  # cold pass, as at the first events load after a server start; not timed
times = []
events = []
for _ in range(12):
    t0 = time.perf_counter()
    events = parse_ndjson_file(path)
    times.append(time.perf_counter() - t0)
times.sort()
print(f"{best_n} events / {path.stat().st_size / 1e6:.1f} MB worst live chat file; "
      f"whole-file parse median {times[5] * 1000:.1f} ms, max {times[-1] * 1000:.1f} ms over 12")
EOF
```

M118 — raw-log tail-follow grown-line round cost. The runaway-write shape: a backend appends
to one line that never closes, so every poll round the drain re-examines the whole unclosed
tail. The writer paces slower than the drain's poll interval (one append per round), the ticker
reads the event loop's worst gap the same way M14/M114 do, and the tail read stays bounded by
the window's scratch file (live home never touched):

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, os, shutil, sys, tempfile, threading, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.agents.backends.base import tail_follow_events

CHUNK = 128 * 1024 * 1024
ROUNDS = 8

work = Path(tempfile.mkdtemp(prefix="m118-tail-", dir="/tmp"))
raw = work / "agent.raw.ndjson"
raw.write_bytes(b'{"type": "context", "blob": "')
writer_done = threading.Event()
stop = threading.Event()
chunk = b"x" * CHUNK  # built once: a per-round build would ride the writer's alloc, not the drain's cost

def writer():
    with raw.open("ab") as f:
        for _ in range(ROUNDS):
            if stop.is_set():
                return
            f.write(chunk)
            time.sleep(0.25)
    writer_done.set()

async def main():
    lag = []
    done = threading.Event()
    last = time.perf_counter()

    def ticker():
        nonlocal last
        while not done.is_set():
            time.sleep(0.005)
            now = time.perf_counter()
            lag.append(now - last)
            last = now

    wt = threading.Thread(target=writer, daemon=True)
    tt = threading.Thread(target=ticker, daemon=True)
    count = 0

    def translate(event):
        nonlocal count
        count += 1
        return [event]

    t0 = time.perf_counter()
    tt.start()
    wt.start()
    async for _ in tail_follow_events(
        raw, translate=translate, is_alive=lambda: not writer_done.is_set(),
        post_result_timeout=60.0,
    ):
        pass
    wall = time.perf_counter() - t0
    done.set()
    tt.join()
    wt.join()
    lag.sort()
    print(f"{ROUNDS} x {CHUNK // (1 << 20)} MB appends to one unclosed line "
          f"({ROUNDS * CHUNK / 1e6:.0f} MB tail): drain wall {wall:.2f} s, "
          f"max loop tick gap {lag[-1] * 1000:.0f} ms, events {count}")

asyncio.run(main())
shutil.rmtree(work, ignore_errors=True)
EOF
```

M119 — sidebar root session-list serve, steady state. The sidebar's "All" pill
fetch (`GET /api/sessions/`) serves every active session plus one projected
worker-leaf row per legacy thread — the projected shape the capped search
serves, at the whole-corpus scale, and the shape every sidebar re-entry
re-reads. The served path keys the whole body on the row identities plus
overlay states (the search route's `_search_whole_body` mechanism), renders
the rows once into pre-dumped bytes — the M34 events-fetch repair's shape,
priced on the response_model jsonable_encoder pass the mapped return ran over
every row — and ships the gzip form from a body-keyed memo, so a repeat of an
unchanged body re-runs zero dumps and zero deflate (the _switch_gzip_memo
mechanism). The cost is per-click latency invisible to the standing probes
(M56 reads the status poll, M8 the absent-needle search), so the collector
snapshots the M71 corpus (the manager's reads; the projection's thread reads
resolve through the process config) and drives the route raw-ASGI behind the
production gzip middleware — the browser's fetch always sends Accept-Encoding:
gzip, so the body's deflate is part of the served shape — one cold pass, then
nine timed requests, with a parsed-body digest so a corpus difference between
arms cannot masquerade as a payload difference:
```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, gzip, hashlib, json, os, shutil, sys, tempfile, time
sys.path.insert(0, os.environ["CHECKOUT"])
from pathlib import Path
from fastapi import FastAPI
from server import _CharlieBotGZipMiddleware
import src.api.deps as deps
from src.api.deps import get_session_manager
from src.api.sessions import router as sessions_router
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager

# Sidebar corpus snapshot (the M71 corpus): every session's metadata.json, the
# active sessions' live chat files, and every session's triggers/. Live home
# read once for the copy, never written; removed on every exit path
root = Path.home() / ".charliebot" / "sessions"
home = Path(tempfile.mkdtemp(prefix="m119-list-home-", dir="/tmp"))
try:
    for d in root.iterdir():
        meta_p = d / "metadata.json"
        if not meta_p.is_file():
            continue
        try:
            raw = json.loads(meta_p.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            continue
        dst = home / "sessions" / d.name
        dst.mkdir(parents=True)
        shutil.copy2(meta_p, dst / "metadata.json")
        if raw.get("status") == "active" and (d / "data" / "chat_events.jsonl").is_file():
            (dst / "data").mkdir()
            shutil.copy2(d / "data" / "chat_events.jsonl", dst / "data" / "chat_events.jsonl")
        if (d / "triggers").is_dir():
            shutil.copytree(d / "triggers", dst / "triggers")

    cfg = CharlieBotConfig(charliebot_home=home)
    mgr = SessionManager(cfg)
    app = FastAPI()
    app.include_router(sessions_router, prefix="/api/sessions")
    app.dependency_overrides[get_session_manager] = lambda: mgr
    app.add_middleware(_CharlieBotGZipMiddleware, minimum_size=1000, compresslevel=1)

    def scope():
        return {"type": "http", "asgi": {"version": "3.0", "spec_version": "2.3"},
                "http_version": "1.1", "method": "GET", "scheme": "http",
                "path": "/api/sessions/", "raw_path": b"/api/sessions/", "query_string": b"", "root_path": "",
                "headers": [(b"host", b"test"), (b"accept-encoding", b"gzip")],
                "client": ("test", 123), "server": ("test", 80)}

    async def drive():
        body = b""
        out = {"status": 0, "enc": b""}

        async def receive():
            return {"type": "http.request", "body": b"", "more_body": False}

        async def send(msg):
            nonlocal body
            if msg["type"] == "http.response.start":
                out["status"] = msg["status"]
                out["enc"] = dict(msg.get("headers", [])).get(b"content-encoding", b"")
            elif msg["type"] == "http.response.body":
                body += msg.get("body", b"")

        t0 = time.perf_counter()
        await app(scope(), receive, send)
        return time.perf_counter() - t0, body, out

    async def main():
        _, _, cold = await drive()  # cold pass, as at the first sidebar render after a server start; not timed
        if cold["status"] != 200: raise SystemExit(f"M119 FAILED, cold status {cold['status']}")
        times, wire_body, out = [], None, None
        for _ in range(9):
            dt, wire_body, out = await drive()
            times.append(dt)
        if out["status"] != 200:
            raise SystemExit(f"M119 FAILED, status {out['status']}")
        times.sort()
        wire, enc = len(wire_body), out["enc"]
        decoded = gzip.decompress(wire_body) if enc == b"gzip" else wire_body
        digest = hashlib.sha256(json.dumps(json.loads(decoded), sort_keys=True).encode()).hexdigest()[:12]
        print(f"checkout {os.environ['CHECKOUT'].rsplit('/', 1)[-1]}: {len(json.loads(decoded))} rows, "
              f"decoded {len(decoded)} B, wire {wire} B, enc {enc.decode() or 'identity'}, digest {digest}; "
              f"list serve median {times[4] * 1000:.2f} ms, max {times[-1] * 1000:.2f} ms over 9")

    asyncio.run(main())
finally:
    shutil.rmtree(home)  # every exit path removes the scratch copy: the hourly cadence leaks one copy per skipped removal
EOF
```

M120 — task-tree page serve, invalidated index. The task-tree panel's page request rebuilds the
tree index whenever the 2 s TTL expired or a metadata write invalidated it — any write between
clicks does that, so the production shape is one build per request. The pre-fix build re-read and
re-parsed every session's metadata.json per rebuild; the fixed build consults the SessionManager's
shared authoritative entries (the per-entry stat-signature check) and reads a file only where no
entry covers the name. The collector snapshots the live sessions tree (every metadata.json, plus
each task node's data/ for the facts revision the build folds) into one scratch home (live home
read once for the copy, never written), warms the shared metadata cache the hours-running server
has, then drops the tree index before each timed call and drives `tree_page` in-process — the same
shape the M7 protocol uses. Evidence points the same collector at the before and after checkouts
(``CHECKOUT`` at each root, shared snapshot home), asserting an identical page body and tree
revision across arms. Snapshot once:

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import json, shutil, tempfile
from pathlib import Path

root = Path.home() / ".charliebot" / "sessions"
home = Path(tempfile.mkdtemp(prefix="m120-tree-home-", dir="/tmp"))
dst = home / "sessions"
dst.mkdir(parents=True)
n_meta = n_data = 0
for d in root.iterdir():
    m = d / "metadata.json"
    if not m.is_file():
        continue
    sd = dst / d.name
    sd.mkdir()
    shutil.copy2(m, sd / "metadata.json")
    n_meta += 1
    try:
        meta = json.loads(m.read_text())
    except ValueError:
        continue
    if meta.get("profile") is not None and (d / "data").is_dir():
        shutil.copytree(d / "data", sd / "data",
                        ignore=shutil.ignore_patterns("master_runs", "traces", "artifacts", "threads", "runs"))
        n_data += 1
print(f"export M120_HOME={home} M120_METAS={n_meta} M120_TASK_NODES={n_data}")
EOF
```

Then run per checkout (``eval`` the snapshot export first):

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, hashlib, json, os, shutil, statistics, sys, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager
from src.core.task_sessions import TaskTreeManager

home = Path(os.environ["M120_HOME"])
cfg = CharlieBotConfig(charliebot_home=home)
session_mgr = SessionManager(cfg)
tree = TaskTreeManager(cfg, session_mgr)

async def main():
    # The hours-running server's shared metadata cache: one authoritative read
    # per session (the same populate path the server's own readers use). The
    # scratch copy takes any migration write the warm-up triggers.
    names = sorted(p.name for p in (home / "sessions").iterdir() if p.is_dir())
    for sid in names:
        await session_mgr.get_session(sid)
    await tree.tree_page(parent_id=None, include_archived=False, limit=100, cursor=None)  # cold pass; not timed
    times = []
    page = None
    for _ in range(7):
        tree._index = None; tree._index_generation += 1  # the write-between-clicks shape
        t0 = time.perf_counter()
        page = await tree.tree_page(parent_id=None, include_archived=False, limit=100, cursor=None)
        times.append(time.perf_counter() - t0)
    times.sort()
    body = json.dumps(page, sort_keys=True, default=str)
    print(f"checkout {os.path.basename(os.environ['CHECKOUT'])}: {os.environ['M120_METAS']} metadata files, "
          f"{os.environ['M120_TASK_NODES']} task-node event corpora; tree-page roots (invalidated index) median "
          f"{statistics.median(times) * 1000:.2f} ms, max {times[-1] * 1000:.2f} ms over 7; rows {len(page['items'])}, "
          f"body sha1 {hashlib.sha1(body.encode()).hexdigest()[:12]}, revision {page['tree_revision'][:12]}")

try:
    asyncio.run(main())
finally:
    shutil.rmtree(home, ignore_errors=True)  # every exit path removes the scratch copy
EOF
```

M121 — task-tree index rebuild burst, invalidated. One structural write (a task
create, a status flip, any `_save_meta`) invalidates the tree index, and the
readers the write touches — the sidebar poll's task probe, the tree page, the
delegation's own create — all arrive inside the same burst. The cost is
per-request latency invisible to the standing HTTP probes (M120 reads the tree
page alone, serially), so the collector snapshots the M120 corpus (every
session's metadata.json plus each task node's `data/` — the facts fold's chat
events and archived chunks; scratch home, live home read once for the copy,
never written) and drives `_get_index` directly: one cold pass, then per round a
solo invalidated rebuild and a 6-reader concurrent burst, seven rounds. The
solo median is the build's own cost (a regression there is a different topic);
the burst median against it is the single-flight property this metric watches,
and the builds-per-burst count the collector prints is its mechanism witness:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, json, os, shutil, sys, tempfile, time
from pathlib import Path

# The M120 corpus shape: every session's metadata.json plus each task node's
# data/ (the facts fold's chat events and archived chunks). Live home read once
# for the copy, never written; this block removes the copy on every exit path.
root = Path.home() / ".charliebot" / "sessions"
home = Path(tempfile.mkdtemp(prefix="m121-tree-home-", dir="/tmp"))
dst = home / "sessions"
dst.mkdir(parents=True)
n_meta = n_nodes = 0
for d in root.iterdir():
    m = d / "metadata.json"
    if not m.is_file():
        continue
    sd = dst / d.name
    sd.mkdir()
    shutil.copy2(m, sd / "metadata.json")
    n_meta += 1
    try:
        meta = json.loads(m.read_text())
    except ValueError:
        continue
    if meta.get("profile") is not None and (d / "data").is_dir():
        shutil.copytree(d / "data", sd / "data",
                        ignore=shutil.ignore_patterns("master_runs", "traces", "artifacts", "threads", "runs"))
        n_nodes += 1

CHECKOUT = os.environ["CHECKOUT"]
sys.path.insert(0, CHECKOUT)
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager
from src.core.task_sessions import TaskTreeManager

cfg = CharlieBotConfig(charliebot_home=home)
session_mgr = SessionManager(cfg)
tree = TaskTreeManager(cfg, session_mgr)

async def main():
    names = sorted(p.name for p in dst.iterdir() if p.is_dir())
    for sid in names:
        await session_mgr.get_session(sid)
    await tree._get_index()  # cold pass, as at a process start; not timed
    orig = tree._build_index_sync
    builds = {"n": 0}

    def counting(cached_metas):
        builds["n"] += 1
        return orig(cached_metas)

    tree._build_index_sync = counting
    solo, burst = [], []
    revision = None
    for _ in range(7):
        tree._invalidate_index()
        t0 = time.perf_counter()
        r = await tree._get_index()
        solo.append(time.perf_counter() - t0)
        revision = r.revision
        tree._invalidate_index()
        builds["n"] = 0
        t0 = time.perf_counter()
        results = await asyncio.gather(*(tree._get_index() for _ in range(6)))
        burst.append(time.perf_counter() - t0)
        assert all(x.revision == revision for x in results), "revision moved across the burst"
    solo.sort()
    burst.sort()
    print(f"checkout {Path(CHECKOUT).name}: {n_meta} metadata files, {n_nodes} task nodes; "
          f"6-reader burst median {burst[3] * 1000:.2f} ms, max {burst[-1] * 1000:.2f} ms over 7 "
          f"({builds['n']} builds in the last burst); solo invalidated rebuild median "
          f"{solo[3] * 1000:.2f} ms; revision {revision[:12]}")

try:
    asyncio.run(main())
finally:
    shutil.rmtree(home, ignore_errors=True)  # every exit path removes the scratch copy
EOF
```

M122 — backend stream event discovery delay. The raw-log tail-follow loop is the single
read loop for every cc-family backend stream (the master's charlie-code turns, every
worker turn, the re-attach path), and its poll interval is the discovery delay it adds
to every event the CLI writes — assistant message, tool call, result alike. The
collector mounts the loop over a scratch raw log, appends 24 paced lines, and times
append-to-yield per line; the idle-round CPU cost rides the same reading (the trade
side of the interval: one fstat per idle wake). Evidence while the live server runs
older code points the same collector at the branch checkout (`CHECKOUT` at the worktree
root), the same shape as the M62 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'PYEOF'
import asyncio, os, sys, time
from pathlib import Path

sys.path.insert(0, os.environ["CHECKOUT"])
from src.agents.backends.base import tail_follow_events, _TAIL_POLL_INTERVAL

SCRATCH = Path("/tmp/lp_m122/probe.jsonl")
IDLE = Path("/tmp/lp_m122/idle.jsonl")
LINES = 24
PACE = 0.25
IDLE_WINDOW = 2.0

def write_line(seq: int) -> None:
    line = f'{{"seq": {seq}, "type": "assistant", "n": {seq}}}\n'.encode()
    with SCRATCH.open("ab") as f:
        f.write(line)
        os.fsync(f.fileno())

async def discovery_round() -> list[float]:
    SCRATCH.write_bytes(b"")
    appended: list[float] = []
    latencies: list[float] = []
    alive = {"v": True}

    async def consume():
        async for ev in tail_follow_events(
            SCRATCH,
            translate=lambda e: [e],
            is_alive=lambda: alive["v"],
            start_offset=0,
            post_result_timeout=1.0,
        ):
            latencies.append(time.perf_counter() - appended[ev["seq"]])

    async def produce():
        for seq in range(LINES):
            await asyncio.sleep(PACE)
            write_line(seq)
            appended.append(time.perf_counter())

    task = asyncio.create_task(consume())
    await produce()
    await asyncio.sleep(0.5)  # one drain window past the last append
    alive["v"] = False
    await asyncio.wait_for(asyncio.shield(task), timeout=5.0)
    if len(latencies) != LINES:
        raise AssertionError(f"discovery rounds: {len(latencies)} of {LINES} lines yielded")
    return latencies

async def idle_cpu() -> float:
    IDLE.write_bytes(b"")
    alive = {"v": True}
    async def idle_follow():
        async for _ in tail_follow_events(
            IDLE,
            translate=lambda e: [e],
            is_alive=lambda: alive["v"],
            start_offset=0,
            post_result_timeout=1.0,
        ):
            pass
    task = asyncio.create_task(idle_follow())
    await asyncio.sleep(0.2)  # let the mount settle before the window opens
    cpu0 = time.process_time()
    await asyncio.sleep(IDLE_WINDOW)
    cpu = time.process_time() - cpu0
    alive["v"] = False
    try:
        await asyncio.wait_for(asyncio.shield(task), timeout=2.0)
    except asyncio.TimeoutError:
        task.cancel()
        raise AssertionError("idle follow did not stop on producer death")
    return cpu

async def main():
    await discovery_round()  # warm pass, not timed
    lat = await discovery_round()
    lat.sort()
    cpu = await idle_cpu()
    wakes = IDLE_WINDOW / _TAIL_POLL_INTERVAL
    print(f"checkout {Path(os.environ['CHECKOUT']).name}: poll interval {_TAIL_POLL_INTERVAL:.2f} s; "
          f"discovery median {lat[len(lat)//2]*1e3:.1f} ms, max {lat[-1]*1e3:.1f} ms over {LINES}; "
          f"idle follow CPU {cpu*1e3:.1f} ms over {IDLE_WINDOW:.0f} s "
          f"({cpu/IDLE_WINDOW*100:.2f}% of one core at {wakes/IDLE_WINDOW:.0f} wakes/s)")

asyncio.run(main())
PYEOF
```

M123 — hook-helper import floor, per hook event. Every Claude Code hook event spawns the
registered helper command once (the gate events — UserPromptSubmit, PreToolUse, PermissionRequest —
sit on the turn's critical path), so the helper's fresh-process import floor is a per-event
latency line. The collector imports the checkout under test's `claude_sub`, writes the plugin
exactly as the launch writes it, and times the registered PreToolUse command verbatim against an
absent socket (`--gate`, so the transport-failure path returns rc 2 without signalling the parent
group); the nonzero exit is the assert that the transport round-trip ran. The collector itself
runs `-S`: the venv's editable finder would pin `src` at the main checkout and read the wrong
tree's registration:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python -S - <<'PYEOF'
import json, os, subprocess, sys, tempfile, time, types
from pathlib import Path

sys.path.insert(0, os.environ["CHECKOUT"])
from src.cli import claude_sub

with tempfile.TemporaryDirectory(prefix="m123-hook-home-") as tmp:
    bridge = types.SimpleNamespace(socket_path=str(Path(tmp) / "bridge.sock"), token="m123-token")
    plugin_dir = claude_sub._write_hook_plugin(Path(tmp), bridge)
    hooks = json.loads((Path(plugin_dir) / "hooks" / "hooks.json").read_text())
    command = hooks["hooks"]["PreToolUse"][0]["hooks"][0]
    argv = [command["command"], *command["args"]]
    argv[argv.index("--socket") + 1] = str(Path(tmp) / "absent.sock")
    times = []
    for _ in range(7):
        t0 = time.perf_counter()
        proc = subprocess.run(argv, input=b"{}", capture_output=True)
        times.append(time.perf_counter() - t0)
        if proc.returncode != 2 or b"transport failure" not in proc.stderr:
            raise SystemExit(f"probe did not fail the transport round-trip: rc={proc.returncode} "
                             f"stderr={proc.stderr.decode(errors='replace')[:200]!r}")
    times.sort()
    print(f"checkout {os.path.basename(os.environ['CHECKOUT'])}: registered head {argv[0] + ' ' + argv[1]!r}; "
          f"hook helper wall median {times[3]*1000:.1f} ms, max {times[-1]*1000:.1f} ms over 7")
PYEOF
```

M124 — config-verb dispatch wall, help path. `charliebot config --help` is the discovery and
script-probing path: build the parser, print, nothing else. The verb module was the one CLI module
still importing `src.core.config` at module level (the family's other readers defer the import into
the one command that reads it — the src.cli.memory shape), so the pydantic model build rode every
`--help` and parser-error dispatch. The collector times the M92 protocol's fresh-process wall over
the verb; the `config get` round rides the same probe as the mechanism witness — it pays the model
build either way, so its band must hold while the help wall drops:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import os, subprocess, sys, time

CHECKOUT = os.environ["CHECKOUT"]
CODE = "import sys; from src.cli.main import main; sys.exit(main())"

def wall(args):
    t0 = time.perf_counter()
    subprocess.run([sys.executable, "-c", CODE, *args], cwd=CHECKOUT,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True)
    return time.perf_counter() - t0

def line(label, args):
    wall(args)  # warm the interpreter's own page cache; not timed
    times = sorted(wall(args) for _ in range(7))
    print(f"checkout {os.path.basename(CHECKOUT)}: {label} median {times[3]:.3f} s, "
          f"max {times[-1]:.3f} s over 7")

line("config --help (import+dispatch floor, no server call)", ["config", "--help"])
line("config get server (the reading command; the model stack rides it either way)",
     ["config", "get", "server"])
EOF
```


M125 — sibling-verb dispatch floor, help path. Five verb modules still import their heavy core
stacks at module level (improve → src.core.improve_sequence, publish → src.core.publish, storage →
src.core.storage_cool, gc-trash → src.core.worktree_trash, remote-launch → src.core.models, all
five → src.core.config), so the M124 round's probe read their --help walls at 0.157-0.386 s while
the deferred-module verbs read 26-57 ms. The collector times the M92 protocol's fresh-process wall
per verb — argparse exits before any request, so the reading is pure import+dispatch:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import os, subprocess, sys, time

CHECKOUT = os.environ["CHECKOUT"]
CODE = "import sys; from src.cli.main import main; sys.exit(main())"
VERBS = ["improve", "publish", "storage", "gc-trash", "remote-launch"]

def wall(verb):
    t0 = time.perf_counter()
    subprocess.run([sys.executable, "-c", CODE, verb, "--help"], cwd=CHECKOUT,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True)
    return time.perf_counter() - t0

for verb in VERBS:
    wall(verb)  # warm the interpreter's own page cache; not timed
    times = sorted(wall(verb) for _ in range(7))
    print(f"checkout {os.path.basename(CHECKOUT)}: {verb} --help (import+dispatch floor, "
          f"no server call) median {times[3]:.3f} s, max {times[-1]:.3f} s over 7")
EOF

## Sampling history

| Date | PR | Before → after | Note |
| 2026-09-26 | this PR | M125 sibling-verb dispatch floor, introduced with this PR: fresh-process `--help` medians improve 0.382/0.392/0.377/0.399 → 0.037/0.038/0.037/0.038 s (−90 %), publish 0.157/0.160/0.159/0.166 → 0.039/0.036/0.037/0.037 s (−77 %), storage 0.235/0.248/0.243/0.255 → 0.031/0.030/0.029/0.030 s (−88 %), gc-trash 0.203/0.197/0.197/0.201 → 0.033/0.032/0.032/0.032 s (−84 %), remote-launch 0.204/0.201/0.207/0.206 → 0.042/0.043/0.042/0.042 s (−79 %), maxima down in step, every paired round faster over three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, arm order alternating, at load 1.89-2.29 one-minute; no-regression witnesses: M92 schedule-trigger --help interleaved ×3 (main 0.038/0.039/0.038 vs branch 0.037/0.036/0.040 s median, band parity — the deferred-module shape the fix joins), tests/test_cli_import_weight.py's five new per-verb import bans (25 passed), the full suite green before push | each verb module imported its heavy core stack at module level, so the M124 round's five sibling probes read 0.157-0.386 s while the deferred-module verbs read 26-57 ms — the core-module import moved into the one command that needs it in all five (the src.cli.config deferral shape), and storage's parser default MIN_IDLE_DAYS single-homed into src/core/constants.py (stdlib-only by charter, the MAX_TRIGGER_MESSAGE_CHARS precedent) so the parser build stops importing src.core.storage_cool; the verbs' real invocations pay the same imports at execution time, so only the discovery paths get faster, and the three get_config patch targets retargeted to src.core.config.get_config where the deferred imports now read |
| 2026-09-26 | this PR | M124 config-verb dispatch wall, introduced with this PR: fresh-process `config --help` median 0.154/0.156/0.151 → 0.032/0.030/0.030 s (−79 % to −81 %), maxima 0.156-0.173 → 0.030-0.034 s, every paired round faster over three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, arm order alternating, at load 0.55-0.59 one-minute; mechanism witness in the same probe: `config get server` band parity both arms (0.156-0.157 vs 0.157-0.159 s median, maxima 0.161-0.166 both — the get path pays the model build either way); no-regression witnesses interleaved ×3: M92 schedule-trigger --help 0.037-0.038 s median both arms, M115-family 14-passed config/main CLI tests | src.cli.config priced the pydantic model build (~30 ms) plus the backend-model stack (~19 ms) into every `--help` and parser-error dispatch; the import moved into _cmd_get beside the model_fields membership check that needs it, the src.cli.memory deferral shape, so the get wall is unchanged and the discovery floor joins the deferred-module verbs' 26-57 ms band. Five sibling verb modules also import get_config at module level (improve, remote-launch, gc-trash, publish, storage), but their deferral moved no wall beyond noise in this round's probe — each floor is pinned by a core module's own eager config/models chain (publish → src.core.publish, improve → src.core.improve_sequence, storage → src.core.storage_cool, gc-trash → src.core.worktree_trash → src.core.models, remote-launch → src.core.models); this round's after walls read publish 0.178, gc-trash 0.180, remote-launch 0.177, storage 0.253, improve 0.386 s — the next run's dispatch-floor hunt starts at those core modules |
| 2026-09-25 | this PR | M92/M97/M102 CLI verb walls, the run-token chain's dataclasses import priced out: M92 schedule-trigger --help 0.049/0.051/0.052 → 0.041/0.042/0.043 s (−16 % to −18 %), M97 plan list 0.072/0.077/0.074 → 0.063/0.066/0.059 s (−13 % to −20 %), M102 artifact wrap --genre plan 0.054/0.053/0.058 → 0.043/0.042/0.044 s (−20 % to −24 %), maxima down in step (0.053-0.055 → 0.045-0.049, 0.080-0.087 → 0.060-0.077, 0.061-0.067 → 0.045-0.050 s), every paired round faster over three interleaved rounds of the verbatim collectors — main checkout before vs branch worktree after back-to-back at load 1.7-3.9 one-minute; M98 memory query unchanged within noise (0.056/0.064/0.054 → 0.056/0.060/0.054 s — src.core.memory keeps its own dataclasses import, so the run-token share the fix removes sits under that wall's spread); component attribution, -X importtime on schedule-trigger --help: src.core.run_token cumulative 12.4 ms → 3.3 ms with the dataclasses→inspect chain (inspect 7.2 ms plus annotationlib/ast/dis/tokenize) gone from the plan/artifact/schedule-trigger import chains; no-regression witnesses: tests/test_run_token.py + tests/test_cli_import_weight.py (34 passed; the parser-build pin now bans dataclasses beside shutil/_colorize), the 6555-passed suite (17 skipped) | the two frozen dataclasses in src.core.run_token rode src.cli.common's module-level ``from src.core.run_token import load_run_token`` into every fresh CLI process — every master turn and worker session runs several ``charliebot`` invocations, each paying the dataclasses→inspect chain for machinery no consumer calls; the plain ``__slots__`` classes mirror the credentials.py precedent (which names the same cost for its own module), keep the value semantics the round-trip test pins (eq + hash), and tests/test_cli_import_weight.py's parser-build ban now holds the chain dataclasses-free |
| 2026-09-25 | this PR | M119 list serve, introduced with this PR: median 2.90/2.85/2.72 → 0.71/0.74/0.61 ms (−73 % to −78 %), maxima 3.20-4.38 → 0.96-1.02 ms, every paired round faster over three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back at load 1.5-4.1 one-minute, 128 projected rows (52 active sessions, 35 legacy parents carrying the leaf fan-out), decoded 248203-248392 B, wire 42347-42461 B, digest stable within every arm; the parsed body stays pinned equal to the response-model render of the stamped-copy path (the test_list_ships_precompressed_body parity witness, jsonable_encoder equality) and the whole-body memo serves an unchanged corpus with zero re-renders (test_list_repeat_serves_whole_body_without_rerender) | the route returned the projected list through response_model — the jsonable_encoder pass over every row plus the stdlib render measured ~1.8-2.2 ms of the ~2.9 ms wall, and the per-request model copies broke every downstream identity memo, so the leaf rows rebuilt and no whole-body memo could serve; the route now reads the shared cached refs through list_sessions_readonly (plan-approval key added), overlays the six derived fields at render, and keys the whole body on the row identities plus overlay states — a repeat of an unchanged corpus runs zero dumps, and the gzip form rides the body-keyed memo beside the search route's |
| 2026-09-25 | this PR | M98 memory query wall median 0.089/0.087/0.088 → 0.057/0.056/0.057 s, −35 % to −36 %, maxima 0.093/0.089/0.092 → 0.068/0.058/0.059 s, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, live store read-only, load 1.50-2.74 one-minute; component attribution, cProfile of a fresh `memory query --index` process on the before arm: src.cli.memory module exec 60 ms of the 71 ms wall, `asyncio/__init__` 42 ms cumulative inside it, the query work itself ~1 ms); no-regression witnesses: the run-token resolution suite (tests/test_memory_run_query.py + tests/test_memory_store.py, 70 passed), the import-weight contract now banning asyncio from the memory chain (20 passed), the three verbs exercised — query against the live store read-only, add and lint against a scratch CHARLIEBOT_HOME — and the 6511-passed suite (17 skipped) | the module-level `import asyncio` served only `_resolve_run_scoped_audience`'s `asyncio.Lock()` — the run-token path's one consumer — while query/add/lint, the verbs the cron instructions and master turns issue on demand, paid its ~40 ms (asyncio plus the concurrent.futures and logging chain it drags) on every invocation; the import moves beside the run-token path's other deferred imports (the file's get_config deferral shape), putting the read wall at the 09-24 verb-wall band the M92 collector prices (0.033-0.047 s floor plus the memory chain's own ~20 ms), and the healthy range keeps its 0.30 s line |
| 2026-09-25 | this PR | M44 served /scheduled median 1.72/1.66/1.65 → 1.00/0.98/1.01 ms over three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back at load 1.43-1.72 one-minute, 54 scheduled rows, wire 5842 B, decoded 76290 B, digest 0c4db57e2401 identical across all six arms, every paired round faster (−0.63 to −0.72 ms, −37 % to −41 %), maxima 1.89-2.79 → 1.30-1.93 ms; no-regression witnesses interleaved ×3, every body digest identical: M63 /view handler 0.52-0.53 → 0.48-0.60 ms (max 3.90 → 1.43-1.94 ms, body 126327 B), M36 full poll 0.55-0.59 → 0.57-0.68 ms (digest afdaf4098821), M71 capped search 2.54-2.64 → 2.48-2.55 ms (digest df1a38cbf819); 6502-passed suite | the poll rebuilt every payload row per request although the projection's row memos pin each leaf object across requests: the parents arrived as per-request model_copy rows, so the projection memo's parent identity check missed every request and the fan-out rebuilt all 43 leaves plus their dumps, and the view-rows sweep walked 510 µs on the calling poll (90 % of the fan-out's wall over 600 gated calls). The route now lists shared cache references (list_sessions_readonly, the derived state riding alongside instead of stamped onto copies) and overlays derived, thinking, and schedule fields onto the dumped dicts — the same keys in the model's field order, byte-identical render — so the leaf payloads serve from the pinned objects; the countdown's insurance sweep runs detached single-flight (the sidebar sweep's semantics, its fresh proof landing for the following polls), off the collector's 10-call window: the quoted cut is the readonly+memo rework alone, the sweep's −0.6 ms amortized share lands on the live continuous polls |
| 2026-09-25 | this PR | M118 grown-line round cost, introduced with this PR: 8 × 128 MB appends to one never-closing raw-log line (1 GB tail) read drain wall 6.16/6.15/7.83 → 2.93/2.94/2.93 s (−52 % to −62 %) and max loop tick gap 987/1017/995 → 79/90/80 ms (−91 % to −92 %) over three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back (load 1.4-2.4 one-minute, a sibling cron sweeping throughout); the M84 standing collector rode along as the no-regression witness: tail-follow replay median 5924.9 ms (main) vs 5999.9 ms (branch), stdout-stream 6609.1 vs 6599.2 ms, parity divergences 0 both arms (the single-round replay's giant-line parse is untouched); 14 stream-parse and silence-recheck tests passed, full suite in the landing run | the drain copied the whole unclosed tail into `carry` and re-scanned it from the line's first byte on every poll round, so a runaway write's per-round cost grew with the tail while the writer appended — the on-disk 2.1 GB single-line raw log documents the shape; writers only append, so the region a previous round's find proved newline-free stays newline-free and the scan resumes at the round watermark, and the torn-tail warning's bytes read once at follow end instead of riding every round |
| 2026-09-25 | this PR | M81 repeat re-render, introduced with this PR: the same page rebuilt into fresh elements — the session re-entry shape — reads repeat median 16.35/16.71/14.10/16.16/16.90 → 3.09/3.26/4.21/2.75/5.46 ms over five interleaved rounds of the verbatim collector (main checkout before vs branch worktree after back-to-back, load 0.43-0.99 one-minute), −68 % to −83 %, every paired round faster, repeat parity (byte-identity against the cold walk) true every round; the standing cold reading moves 25.88-29.23 → 27.83-31.33 ms (the duplicated gated body's walked-bytes swap replaces its warm re-walk plus the pair store's reads — both arms far inside the 60 ms line, and the walked count the gate watches reads 2 both arms; the cold pass's walk count reads 4 → 2 as the duplicate serves the swap); streamed math-free draft arm unchanged 0 walks / 0.00 ms inside its < 0.010 s line; no-regression witnesses interleaved ×2: M33 replay wall median 0.037-0.038 s both arms, parity true every arm; M54 paint-work median 0.086-0.098 → 0.089-0.094 s, parity true; M60 repeat-page 0.01 ms both arms, parity true; M106 switch repaint 0.40-0.42 → 0.39-0.41 ms, parity true; 43-passed node suites including the new 9-case walked-bytes memo suite registered in _NODE_TESTS, 6473-passed Python suite with the 20 vfork/antigravity failures pre-existing in this venv (CI-only), ruff and yapf clean | every session re-entry, page-depth change, and recap rebuild re-ran the KaTeX walk over every unchanged delimiter-bearing body — the parse memo made the marked parse free on re-entry but never the walk, and the 2026-09-16 row had priced the two direct alternatives against the standing contracts (the walked HTML's jsdom innerHTML re-parse costs 23.2-23.8 ms per body — no cheaper than the walk — and baking katex into the parse memo blind breaks M60's settled-bytes-equal-direct-parse parity) and stopped; the walk's output is a pure function of the body's pre-walk HTML, so renderChatMath now keeps the (pre, walked) pair per body source keyed on the same string the parse memo keys on (data-raw decodes back to it) and the walk upgrades the parse entry to the walked bytes — the next render is born walked and skips even the swap, the element-bytes identity check keeps foreign markup out of the parse entry, the flush settle swaps the plain block in both halves so the pair stays settled, and the streamed paint stays off the cache (its HTML grows every delta — the eviction shape); M60's collector never walks, so its parity contract is untouched — re-read parity true both arms |
| 2026-09-24 | this PR | M92/M97/M98/M102 CLI verb walls, argparse's `_colorize` import priced out of the piped arm: M92 `schedule-trigger --help` 46.9/46.7/46.7/48.0/53.9 → 34.1/34.0/34.5/33.4/38.0 ms (−26 % to −30 %), M97 `plan list` 74.4/69.5/69.2/71.2/68.0 → 62.4/59.6/54.2/54.3/54.4 ms (−14 % to −24 %), M98 `memory query` 51.5/51.6/54.0/54.9/55.6 → 48.8/47.5/50.2/50.1/55.6 ms (−5 % to −9 %, one tie), M102 `artifact wrap` 54.1/56.4/54.2/56.4/51.2 → 41.2/39.8/42.5/40.0/36.2 ms (−22 % to −29 %) over five interleaved rounds of the verbatim collectors — main checkout before vs branch worktree after back-to-back, ABBA arm order inside each round, 7-run medians per arm per round; component attribution (importtime, one verb process): `_colorize` 11.7 ms cumulative — its own 4.0 ms plus the dataclasses→inspect chain 7.4 ms it drags — reached through `HelpFormatter._set_color`'s function-level import at the parser's first formatter construction; the M98 residual is `src/core/memory.py`'s own module-scope `dataclass` use (7.5 ms), a real import the memory entry model needs; byte-identity checked across nine env shapes (plain pipe, FORCE_COLOR, NO_COLOR, PYTHON_COLORS=0/1, TERM=dumb, and combinations) × six commands plus a real-TTY `script` round (both arms ride the stock colorized path there) — every digest pair equal; no-regression witnesses interleaved: M99 `import server` 544.6-548.8 → 541.9-548.5 ms (band), M115 fresh config+credentials 146.2-159.2 → 146.1-148.4 ms (band), M108 claude-sub launch floor 76.9-77.6 → 76.1-77.6 ms (band — manual argv, no parser); 6077-passed suite plus 9 skipped with the 21 vfork/antigravity failures pre-existing on branch worktrees in this venv (the compiled _vfkspawn stub is CI-only — the M116 row's documented set); ruff and yapf clean, 2 new tests (the piped-render ban and the stock-theme byte parity) plus the `_colorize` ban riding the parser-build set | argparse's first formatter construction calls `_set_color`, whose function-level `from _colorize import ...` prices every fresh-process verb even when it never renders help; the shared CliHelpFormatter now reproduces the two arms itself — the colorized arm delegates to the stock method (real import, real decision), the piped arm installs the empty theme without importing (the stock no-color theme is every style field set to "", so any attribute read renders empty in both), and the colorization decision mirrors `_colorize.can_colorize` on POSIX with the colorized arm re-deciding through the stock path, so a mirror drift costs only cosmetic color, never bytes |
| 2026-09-24 | this PR | M116 whole-file parse, introduced with this PR: worst live chat file (20534 events / 22.4 MB) parse median 61.08/60.99/61.02/61.60/60.72/62.12/63.12 → 57.32/57.79/58.67/57.13/56.14/57.45/57.01 ms over seven interleaved rounds of the verbatim collector — main checkout before vs branch worktree after, ABBA arm order inside each round so the host's second-position bias cancels where it lands, every paired round faster (−2.35 to −6.11 ms, −3.7 % to −9.7 %, median −4.5 ms / −7.3 %), 12-call medians per arm per round, the parsed events' identity digest b0492b35024f identical in every arm that checked it (17 of the 28 runs); component attribution (in-process, one 20534-event corpus): the middle generator layer ~5.5 ms — iter_ndjson_events's wrapper exists for the early-stop readers, and a read-everything consumer pays one generator resume per line for nothing — and the per-line parse call chain (kwargs plus the str-first isinstance pair on an all-memoryview stream) ~6.6 ms; no-regression re-measures on the branch: M78's own corpora chat 3281.4/3350.2 → 3228.2/3251.2 ms and worker log 38.9/39.3 → 37.1/39.1 ms over two ABBA rounds (the 507-line / 232-line shapes carry their wall in orjson's work, the per-line cut invisible, as the M116 definition states); 6095-passed suite plus 9 skipped with the one load-sensitive fork-parks failure the 2026-09-24 M115 row documents (11/11 passed alone on both checkouts); ruff and yapf clean | the whole-file parse read every line through two generator frames and constructed a fresh `memoryview(mm)` wrapper per line before slicing it — the wrapper's laziness serves only the early-stop readers (tail, range, the M13 window), which keep it; the read-everything consumer now walks the mapping's lines directly with the skip contract still single-homed in `parse_ndjson_line`, and `_iter_mmap_lines` slices one hoisted mapping view — the pattern `iter_ndjson_events_containing` already rode |
| 2026-09-24 | this PR | M17 fork, the collector repaired and the reference stream's two whole-corpus sweeps fused into one pass. The standing sweep read fork median 6.2596 s, max 10.4999 s against the < 2 s range while the code sat at its post-#1696 level; the ramp tracks the five accumulated corpus-sized children's pending writeback, not the fork: children kept, the five timed forks of the 1051.3 MB / 507-event heaviest corpus read 0.584/1.150/6.178/8.186/8.901 s (a rerun read 0.4955/1.5882/6.4706/10.1147/8.3288 s) and five plain 1 GB writes into one directory ride the same ramp 0.303/0.302/0.320/0.404/2.766 s, while children freed at each wall the same forks read 0.680/0.637/0.524/0.553/0.538 s; the repaired collector frees each child as its wall is taken. Repaired-collector A/B, main checkout before vs branch worktree after back-to-back, three interleaved rounds: fork median 0.5501/0.5123/0.4875 → 0.4937/0.4835/0.4759 s, every paired round faster (−2.4 % to −10 %), maxima 0.5033-0.6494 → 0.5051-0.5256 s; component attribution: the reference stream's ASCII sweep plus newline scan, two whole-corpus DRAM passes, fuse into one chunked sweep — standalone scan median 173.8/182.3/174.7 → 135.4/154.3/133.3 ms over three interleaved rounds of five reps (−15 % to −24 %), newline positions (507) and ASCII verdict identical across arms; a copy_file_range window write measured and rejected — 0.68 → 0.38 s on a sync-drained backlog but +40-70 ms in the fork's own dirty-backlog conditions (three counterbalanced rounds of the phase-attributed collector); parent_reference.jsonl sha256 digest b157b7effbdf58b2 identical across every arm and round; load 7.6-14.9 one-minute across the readings | the five timed forks each leave a corpus-sized parent_reference.jsonl whose pending writeback throttles the next fork's reference write into the kernel's dirty-page path, so the standing collector read host IO state and every round since the corpus crossed a gigabyte would have flagged M17; the free-children shape restores the metric's intent (the fork's own wall) and the fused sweep takes the scan from two passes to one |
| 2026-09-24 | this PR | M1 serve CPU, the standing collector's process grep widened to both process shapes the serving path runs: before — the verbatim collector read `0 serve processes, 0.0% cpu total` at 12:45 PDT while the live instance answered every other collector's request (the server: `python3 server.py` under the repo's own `scripts/start-server.sh`, up since 11:09 PDT, listening on 127.0.0.1:18498) — a structurally silent zero, the same ghost the repaired command confirms side by side (`old grep: 0 serve processes, 0.0% cpu total`); after (repaired command, same round, load 7.25/7.03/5.99 one/five/fifteen under sibling crons' suites and reviews) — `2 serve processes, 1.8% cpu total` (the `uv run python3 server.py` wrapper at ~0% plus the child); the seed-era grep counted only `opencode serve` — the opencode backend family's agent daemons, present only while an opencode session runs — so the server itself never matched and the serve-CPU half read zero whenever no opencode daemon was alive | the collector's single pattern dated from a seed-era reading taken when an opencode daemon was the only long-lived process the host's serving path ran; the server's own launcher chain carries neither the opencode name nor any other line that pattern matched, so the serve-CPU half of the regression watch has been measuring the opencode daemons alone — zero on this round's sweep — while the actual server's CPU went uncounted; the repaired grep (`-E '[o]pencode serve|[s]erver\.py'`) counts both shapes, the `tee` and `bash` wrappers carry neither pattern |
| 2026-09-24 | this PR | M99 server import floor, the import chain and the app assembly ride the server's own gc-off bulk-build span and the slash-command stack leaves the chain: import median 0.520-0.524 → 0.499-0.508 s across two eight-round sets (−13 to −21 ms, −2.5 % to −3.9 %), 16/16 paired rounds faster (sets one and two of eight interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, arm order alternating per round to cancel the host's second-position bias (measured +4.5 ms same-checkout), load 1.7-2.3 one-minute with sibling crons' suites live); component attribution: fresh-process `gc.disable()`-from-start on the before checkout reads the GC share at 43.0 ms of the 0.527 s wall (10/10 interleaved pairs, bands disjoint 0.506-0.553 vs 0.471-0.498), and the branch's spans capture it — the branch's own gc-off-from-start probe reads +0.8 ms over its plain import; `src.core.slash_commands` leaves the import (−17.9 ms module self in `-X importtime`, the last module-scope import no import-path reader reads), of which ~15 ms reappears as a GC gen-2 pause relocated into `src.api.chat`'s exec (import-time GC total is roughly constant — allocation thresholds cross mid-body wherever the heap stands — so the module's wall contribution nets −9.6 ms by sum-of-self), the rest of the wall win riding the span; no-regression witnesses interleaved ×3: M92 schedule-trigger --help 0.036/0.036/0.036 → 0.036/0.036/0.036 s (maxima 0.037-0.039 both arms) and M108 claude-sub 0.075/0.076/0.075 → 0.076/0.075/0.078 s (bands both); 5963-passed suite + 9 skipped (the load-sensitive fork-parks pin fails under sibling-suite load and passes in isolation on both arms, the 2026-09-24 M115 row's environmental class), ruff and yapf clean, plus the server ban-set contract test extended (src.core.slash_commands joins SERVER_HEAVY_MODULES); M99 healthy range unchanged (the after reading sits at two-thirds of the 0.75 s line) | every deploy restart paid the import chain's gen-2 GC walks (~43 ms, the largest single repo-owned slice left after the 2026-09-19/21 deferrals) although the span primitive (`src.core.gc_control.gc_off`) already bounded every other bulk build the server runs; the chain and the app assembly now run inside the same bounded span (gc back on before any request can arrive — the deferred cycles collect at the next natural threshold, the `collect=False` shape the span documents for builds whose allocations stay referenced), and the slash-command stack — whose only import-time reader was the chat send handler's module scope — loads at its four request-time call sites like the master-turn chain already does, the `src.api.slash.dispatch_slash_command` patch target kept through the deferred-import loader's globals-first read |
| 2026-09-24 | this PR | M87 abort wall median 2.2/2.1/1.8 → 1.7/1.5/1.5 ms (−14 % to −29 %), maxima 2.3-5.8 → 1.8-1.9 ms, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, stub serve, load 1.65-2.13 one-minute); loop-lag band parity at the 5 ms ticker floor both arms (5.4-5.9 vs 5.4-5.6 ms); component attribution standalone: the per-call `httpx.AsyncClient(base_url, timeout, verify=_SERVE_SSL_CONTEXT)` construction+aclose reads 260 µs, the rest of the wall the localhost POST both arms share | the per-turn cleanup abort constructed a fresh `httpx.AsyncClient` per call; the POST now rides the process-wide shared outbound client (`src.core.http.get_http_client` — the anthropic-proxy/ext-usage/slack/notifications singleton) with its per-request `OPENCODE_ABORT_TIMEOUT` kept, and the serve URL is pinned plain localhost HTTP (`_SERVER_URL_RE`), so no per-client verify choice applies; the run-start attempt client keeps its own construction (it carries the SSE stream's lifetime) |
| 2026-09-24 | this PR | M7 restart-cold, the persisted seed's proof now carries the tail fetch's maxes (the completion #2009's landing named: the in-process gate's four-field proof — count, sum(time_updated), max(time_updated), max(rowid) — persists whole beside the sidecar rows it describes, and the seeded miss tail-fetches the rows written after the stored max instead of re-reading all 221k keys; a legacy two-field document — the prior deploy's shape, what the live server writes until its next deploy — seeds without a max and keeps the full key diff, the contract the new test pins): controlled-corpus paired rounds, fresh-process seed + one turn's moves (ten in-place step-finish upserts plus twenty appends at the production bump shape) + fresh-process timed collect per round over a 223,441-row synthetic corpus (the live db's row count, live db read-only): 0.576/0.622/0.572/0.529 → 0.382/0.384/0.359/0.369 s (−30.2 % to −38.3 %), every paired round faster; component attribution, instrumented fresh process: the seeded miss tail-fetched exactly the 30 moved rows (5,190 B, 80.8 ms) where the before shape's full key scan reads 0.272-0.275 s standalone; rows-digest parity True in every round and arm (the incremental serve matches the cold replay); no-regression witnesses interleaved ×3: M7 changed-round 0.095-0.107 → 0.099-0.100 s, M7 warm-gate changed round 193.9-204.6 → 192.5-200.5 ms (quiet round 48.9-52.2 → 48.7-50.3 ms, full key scans 0, parity True), M80 churn 0.1329-0.1519 → 0.1351-0.1361 s (rows digest 501337fee183 both arms); 5988-passed suite (the 21 antigravity/spawn failures the missing _vfkspawn build artifact produced in the worktree pass with it copied — no compiler on this host, the committed C source identical), ruff and yapf clean, plus the seeded-miss test rewritten to the tail fetch, the reset round's scan witness updated, and 1 new test (the legacy two-field seed keeps the full key diff and the store writes the four-field proof back); M7 restart-cold definition and collector intro updated to the four-field proof | the restart seed gated on the two-field (count, sum) proof #2009's landing left it, so every fresh process under active turns — each server start's first page load, each hourly round's standing collector reading — paid the 221k-key diff once although the tail fetch's mechanism was already in place behind a len(gate) == 4 dispatch the seed never satisfied |
| 2026-09-24 | this PR | M115 cold config+credentials resolution, introduced with this PR: fresh-process shared import + `get_config()` + `get_credentials()` median 0.1575/0.1587/0.1595/0.1670 → 0.1478/0.1487/0.1500/0.1508 s, −9.5 to −16.2 ms per paired round, every paired round faster (four interleaved rounds of 5 fresh processes per arm, main checkout before vs branch worktree after back-to-back); component attribution, the parse itself over the live corpora: config.yaml 6.582 → 0.700 ms, credentials.yaml 1.346 → 0.168 ms, slash_commands.yaml 0.382 → 0.029 ms medians (−88 % to −92 %), parsed output parity across all 13 live yaml documents (config, credentials, 10 cron tasks, slash_commands), byte-identical safe_dump/CSafeDumper output on the live config, the same YAMLError class on a malformed document; no-regression witnesses interleaved ×3: M97 plan list 0.063/0.057/0.061 → 0.059/0.059/0.060 s, M92 schedule-trigger --help 0.041/0.037/0.040 → 0.052/0.037/0.038 s (round 1's branch arm paid the changed modules' first pyc compile; rounds 2-3 clean), M98 memory query 0.082/0.054/0.051 → 0.078/0.055/0.052 s, M102 artifact wrap 0.039/0.041/0.040 → 0.039/0.041/0.039 s; 5959-passed suite + 9 skipped (the 3 pre-existing environmental failures aside — the 2 nice-runner pins plus the load-sensitive fork-parks test, verified failing on stashed origin/main identically; the branch worktree needed the _vfkspawn build artifact copied from the main checkout — no compiler on this host, the committed C source identical), ruff and yapf clean, plus 7 new tests (the C-binding pin, parsed-output parity with safe_load, the missing/empty-document defaults, the YAMLError propagation, the save/load round trip); M115 definition, collector, and healthy range introduced with this PR | every yaml parse in the process rode the pure-Python SafeLoader/SafeDumper — 6.6 ms on the 3.3 KB live config where libyaml's CSafeLoader reads the same document in 0.7 ms; the loader choice single-homes in yaml_utils (load_yaml/load_yaml_text/save_yaml), so the cold config/credentials resolution, the slash-command load (per /slash list and dispatch), the cron-task and config reloads, and the project-config reads all take the C pair; a pyyaml built without libyaml fails the import loud rather than parsing slowly |
| 2026-09-24 | this PR | M105 file-arm repeat gzip serve memoized on the file signature: the bare-file arm (the /absolute_filepath mount's FileResponse fall-through) serves gzip-accepted GETs of gated media types (text/* plus application/json, application/javascript, text/javascript, application/xml, image/svg+xml) under a 16 MB raw-size cap from a StatSignatureMemo keyed on (path, mtime_ns, size) — stat precedes the read, Content-Encoding set upstream makes the middleware skip — while no-gzip clients, Range requests, unlisted media, and over-cap files keep the streaming arm unchanged: html witness repeat serve median 14.83/15.17/14.69 → 0.63/0.62/0.63 ms (−95.7 % to −95.8 %), maxima 15.58-16.20 → 0.73-0.84 ms, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, the 3,994,219 B worst artifact .html, gzip-accepted, scratch credentials home per arm, load 1.42-2.37 one-minute); wire 2974307 → 2977926 B (+0.12 %, the route's isal level-1 one-shot replaces the middleware's per-chunk zlib at the same level — the decompressed body is byte-identical, the JSON gzip memos' precedent); identity arms band parity (png 1.16-1.20 → 1.26-1.30 ms, pptx 0.87-0.91 → 0.95-1.07 ms medians, wire == raw bytes and transport identity unchanged both arms); the before shape's cost was the stock streaming gzip responder's per-request per-chunk inline deflate — 4 × 1 MiB chunks of the 4 MiB page re-deflated on the event loop every serve, ~1.3 ms of loop stall per chunk — and the memo arm moves the one-shot deflate off the loop behind the existing to_thread hop; 5952-passed suite + 9 skipped (the 3 pre-existing environmental failures aside — the 2 nice-runner pins plus the load-sensitive fork-parks test, verified failing on the main checkout identically; the branch worktree needed the _vfkspawn build artifact copied from the main checkout — no compiler on this host, the committed C source identical), ruff and yapf clean, plus 7 new memo-arm tests (repeat serve stored bytes, rewrite re-deflate, unlisted-media identity, Range streaming, no-gzip raw bytes, over-cap streaming, json memo arm) and the three sibling not-injected tests pinned to the explicit non-gzip client their FileResponse discriminator needs; M105 healthy ranges unchanged (the after reading sits at 0.4 % of the html witness line) | the bare-file arm was the one gzip-able response family still paying the middleware's per-request streaming deflate: every JSON route and both artifact-view arms memoize their gzip form, and the file arm re-deflated the same 4 MB page on every serve, in the event loop, per chunk |
| 2026-09-23 | this PR | M7 token-usage changed round under active turns, the warm row gate's proof miss advanced from a max-keyed tail fetch instead of the whole-table key scan: the in-process gate stores max(time_updated) and max(rowid) beside the (count, sum) proof pair, the miss fetches only rows written after that max (every insert and every time_updated bump carries the write's own wall-clock ms — drizzle `$onUpdate`), and the fetched rows' own before/after sums reconstruct the pair's expected movement, a (count, sum, max rowid) residual mismatch — a cascade delete, a clock stepped backward, a backward write — falling back to the full key scan, with every 16th warm miss taking the full scan as the self-heal that bounds the residual's triple-netting dodge class (a delete and an insert landing in the same millisecond with the deleted row holding the max rowid) the way the pre-tail gate's next-miss re-scan did; the persisted entry's probe stays the two-field (count, sum) pair the restart seed gates on, the maxes are restart-local, and the seeded restart's gate-skip fast path is unchanged: live-corpus harness changed-round median 417.6/425.3/444.2/427.5/477.5 → 300.7/303.3/302.6/314.6/307.1 ms (−26.4 % to −35.7 %), every paired round faster (five interleaved rounds, main checkout before vs branch worktree after, each a fresh process seeded from the live document+sidecar over a fresh copy of the 5.7 GB / 221,612-row live db, 30 moved rows per round — ten in-place step-finish upserts plus twenty appends at the production bump shape, load 1.2-2.7 one-minute), rows digest cf540435cc53 identical across every arm and the cold replay parity True; the doc collector's own calibration, fresh synthetic corpus per arm sized to the live row count: 333.0 → 199.9 ms (−40.0 %), full key scans 5 → 0, parity True; component attribution, standalone on the live db: the before shape's probe (count,sum) 63.7 ms + full keys scan 183.0 ms + Python per-id diff ~90 ms vs the after shape's probe (count,sum,max,max-rowid) 69.6 ms + tail fetch 59.0 ms — the maxes ride the scan the probe already ran; the quiet round (probe skip) reads 63.9-64.7 → 78.0-79.9 ms on the live-corpus harness and 37.0 → 52.4 ms on the doc collector's corpus (the two extra max aggregates); production corroboration: the live server log's /token-usage page loads read 636-1420 ms server-side across the hourly cron's loads (30 loads over 6.55 h) — the changed-round shape the standing collector's fresh process cannot see (its document copy was seconds stale at sweep time, reading 0.086 s median); the charlie-code review's two findings (the residual docstring's never-narrows overclaim; the probe comment's next-miss-re-scan lifetime) — both reproduced A/B by the reviewer — answered with the rowid max (closes the plain delete-and-insert dodge in the very round that dodged), the 16-miss self-heal, and the two claims rewritten to the true strength, plus 2 tests pinning the reproduced self-correction and the masked dodge's self-heal bound; 78-passed token-tally suite including 6 new gate tests, 5943-passed full suite + 9 skipped (the 3 pre-existing environmental failures aside — the 2 nice-runner pins plus the load-sensitive fork-parks test, verified failing on the main checkout's main branch identically), ruff and yapf clean; M7 warm-gate sub-reading, collector, and healthy range (changed-round median < max(0.050 s, rows × 0.0000013 s), quiet round < 0.10 s) introduced with this PR | the changed collect's largest slice re-read the whole message table's keys and diffed 221k of them in Python on every hourly page load while an opencode turn was active — a cost that grows linearly with the db (85k → 190k → 221k rows across its landing docs) — where the tail fetch reads only the rows that moved and the residual check keeps the full scan for every move the fetch cannot prove complete |
| 2026-09-23 | this PR | M114 piped-spawn residual eliminated, the child-side-prctl follow-up its landing named: the piped transports (the opencode and antigravity master launches) and the pdeathsig one-shots spawn through a new clone(CLONE_VM|CLONE_VFORK) seam — src/agents/backends/_vfkspawn.c, a compiled stub whose child runs the fixed pre-exec sequence (PR_SET_PDEATHSIG with the getppid race check, setsid, stdio dup2, Popen(restore_signals=True) parity defaults, close-from-3 via close_range, execve) as pure syscalls on a private stack, so the child never allocates in the shared address space; setup and execve failures report the child's errno over a CLOEXEC pipe and the spawn raises it with the filename — piped spawn loop-lag median 109.1/109.4/110.6 → 5.2/5.3/5.4 ms (−95.1% to −95.3%, the after reading is the 5 ms ticker floor; spawn wall 229.3-230.9 → 0.7-1.1 ms), raw-log shape 5.2-5.4 → 5.2-5.4 ms (band parity, the unchanged path), every paired round faster (three interleaved rounds, main checkout before vs branch worktree after back-to-back, the collector's 3.5 GB inflated heap at the server's standing RSS class, load 1.7-2.3 one-minute); the piped preexec's parent-observable effects move parent-side next to the raw-log shape's (the nice raise and the cgroup move through _apply_turn_tree_limits, failures logged as the existing warnings), the child-side pdeathsig guarantee unchanged and race-corrected; 5943-passed suite (the 3 pre-existing environmental failures aside — the 2 nice-runner pins plus the load-sensitive fork-parks test, verified failing on the main checkout's main branch identically), ruff and yapf clean, plus 5 new seam tests (piped wiring with cwd/env, exec-failure errno with the filename, close-fds stdio-only, pdeathsig child death on spawner exit, SIGKILL exit code); M114 healthy range re-tightened (piped < 0.150 → < 0.020 s, the ticker floor the after reading sits on) and the collector's piped arm updated to the seam shape; deploy note: the first piped spawn after a deploy needs the compiled module — run the repo install (uv sync / pip install -e .) so the ext builds, or the spawn fails loud naming the missing module | the M114 landing moved every spawn off the loop but the pdeathsig-carrying piped spawns still paid the full fork's page-table copy (~110 ms of GIL-held stall per opencode/antigravity master launch on the 3.9 GB server); pdeathsig is child-side-only by kernel contract, so the seam moves the fork itself to clone(CLONE_VM|CLONE_VFORK) — no page-table copy — with the fixed child sequence as syscalls, and every piped launch now costs the loop its ~1 ms handshake; the diff runs over the 300-line loop budget (split-series label): the compiled stub plus its contracts are irreducibly ~260 lines |
| --- | --- | --- | --- |
| 2026-09-23 | this PR | M114 backend-launch spawn loop stall, introduced with this PR: every covered backend's subprocess spawn moved off the event loop — the raw-log transport (the claude family's master turns plus every worker launch) spawns preexec-free so the kernel takes the vfork fast path (~1 ms even from the 3.5 GB heap class, ~55 µs/MB for the full fork's page-table copy this shape skips) and the preexec composition's parent-observable effects apply parent-side right after the handshake (the nice raise via setpriority, the cgroup move via cgroup.procs, failures logged as `turn_tree_nice_failed` / `session_cgroup_move_failed` warnings), while the pdeathsig-carrying spawns (the piped transports and the pdeathsig one-shots) park the fork on a worker thread and wire the child's pipes onto the caller's loop afterwards (src/agents/backends/spawn.py, the child-side preexec unchanged); measured with the M114 collector's 3.5 GB inflated heap at the server's standing RSS class — raw-log spawn loop-lag median 148.0/155.7/158.5 → 5.2/5.3/5.4 ms (−96.6% to −96.7%, the after reading is the 5 ms ticker floor; spawn wall 143.1-153.6 → 0.8 ms), piped spawn loop-lag median 157.0/157.6/158.1 → 69.5/72.0/72.7 ms (−54% to −55%, the residual is the fork's GIL/mmap-lock hold the worker thread's page-table copy keeps — full elimination needs a child-side-prctl mechanism off the fork path, a follow-up), every paired round faster (three interleaved rounds, main checkout before vs branch worktree after back-to-back, load 1.8-2.2 one-minute); production corroboration: the live server log reads GET /api/sessions/status at 239/386/464 ms inside opencode master launches (the 3 s status poll's quiet steady state is the M56 band) — each launch a full fork of the 3.9 GB server on the loop, 37 launches in the 1.75 h window; 5940-passed suite + 9 skipped (the 2 pre-existing environmental nice-runner failures aside — this worker's own process tree runs at nice 10 from the M113 spawner, so the child-nice pins read 10 where a clean runner reads 0; verified failing on the main checkout's main branch identically), ruff and yapf clean, plus the 6-test spawn-seam suite (pipe wiring, exited-before-read drain, SIGKILL code, raw-fd wiring, stdin drain, fork-parks-off-loop) and the parent-side limits contracts (the cgroup.procs pid write, the child nice pin through the preexec-free raw-log path); M114 definition, collector, healthy ranges (raw-log shape loop-lag median < 0.020 s — the M75 loop-lag line, the regression canary: a re-added preexec or an on-loop spawn trips it 30×; piped shape < 0.150 s — the documented GIL/mmap-lock residual the thread-fork keeps, load-sensitive, re-tightened when the child-side-prctl follow-up lands), and history row introduced with this PR | every backend launch is a full fork of the server's own page tables on the event loop — the fork cost scales with resident memory, so the multi-GB server's launch moment stalls every concurrent request, the status poll renders 386-464 ms, live chat ticks freeze mid-stream and in-flight turn outputs buffer; the vfork-shaped raw-log spawn cuts the stall 30× and the piped spawn halves it, and the residual piped cost is the fork's own GIL/mmap-lock hold, not the loop |
| 2026-09-23 | this PR | M72 listing lines recalibrated to entries-tracking formulas, docs-only calibration, no code change: the fixed lines priced the corpora their calibrations measured — changed-round 1159 entries at its 2026-09-12 introduction, repeat-view 1165 at its 2026-09-13 repair, last re-read 1234 entries on 2026-09-16 — and the walk scales with the listed entry count; this round's sweep read repeat 5.78 ms / changed-round 7.95 ms at 1295 entries (load 4.10/2.91/1.32 with the sweep's own collectors live), tripping the changed-round line, and the quiet re-reads minutes later (three verbatim collector rounds, load 2.91-6.04 one-minute carrying the same sweep's tail) read changed-round 5.99/6.47/6.61 ms at 1296 entries — inside the old line and 94 % of it, with repeat-view 5.78 ms at 72 % — so the corpus had grown onto the line, the sweep's own load adding the last 20 %; per-entry rates across the file's record: repeat 3.7-4.5 µs/entry (the post-2026-09-13 serve shape — 4.53-5.25 ms @ 1234 on 09-16, 5.78 @ 1295-1296 today), changed-round 4.5-5.5 µs/entry (6.01-6.33 ms @ 1159 at introduction, 5.53-5.89 @ 1234 on 09-16, 5.99-6.61 @ 1296 quiet today plus the 7.95 sweep-hour trip) — the walk's one-stat-per-entry floor at this host's ~3.5 µs stat cost, the same corpus-tracking shape the M61 all-sessions line adopted; new lines repeat < max(0.008 s, entries × 0.000008 s) and changed-round < max(0.007 s, entries × 0.000008 s) — 1.4-2.2x over the measured bands, the small-corpus floors kept verbatim, a corpus reversion re-tightening them automatically | the sessions root grows a few entries a day (1234 → 1296 in the six days since the 2026-09-17 all-sessions calibration), so a fixed 7 ms line false-trips the regression watch every round the corpus crosses it; the entries-tracking line keeps the trip meaning code-regression-or-load only |
| 2026-09-21 | this PR | M113 voice transcription wall, introduced with this PR: the turn process tree (every covered backend's agent CLI plus the tool subprocesses it spawns) now spawns at nice 10 via the shared `_spawn_preexec` composition, backgrounding it against the server's interactive paths; the contended voice decode — this host's slowest served path — reads 44.68 s → 21.44 s median (0.48×) under 8 nice-0 spinner processes vs 8 nice-10 spinners (three interleaved rounds of the collector's contention harness over the 86.4 s worst on-disk recording, quiet band 18.0-19.4 s RTF 0.21-0.22 both shapes, load 1.2-1.9 one-minute with a sibling cron's suite running), and the quiet wall is untouched by construction (the decode path imports nothing the change adds; quiet medians 18.00 vs 18.00 s band). Production corroboration: the server log's 2026-09-21 08:39 voice round — `POST /api/voice/…/confirm` 30.7 s and the full upload 47.1 s for 25.9 s of audio — reproduces at the pre-fix contended shape (44.7 s harness reading, RTF 0.52) while the same file decodes 6.97 s (RTF 0.27) on the quiet box; the 470 sibling requests in that window stayed sub-500 ms, so the wall was CPU contention on the decode's 4 ONNX threads, not the event loop; contention attribution: 8-core CPU-hog ×2.6, 50 GB memory-hog (swap pressure) ×2.4, stacked ≈ the production 6.8×; determinism witness: two fresh decodes byte-identical (393 chars, the July-era stored transcript renders punctuation differently — the parity witness is determinism, not stored-text equality); the fix's cost side is contention-only by mechanism: nice arbitrates only when the box is oversubscribed, an uncontended box schedules identically (quiet band unchanged across the A/B rounds); 55-passed backend + new nice suite (child-of-preexec nice pin through `make_nice_preexec` and the composed `_spawn_preexec`, cgroup off), ruff and yapf clean; M113 definition, collector, healthy ranges (quiet median < audio seconds × 0.4 — the measured RTF band 0.21-0.29 sits ~1.5x inside; contended median < 2× the same round's quiet median — the after reading sits at 1.19×), and history row introduced with this PR — the live server picks the fix up from its next deploy on | every voice request shares the box with whatever the master turn is running — the user dictates while the previous turn's workers, CLI processes, and cron sweeps burn cores, and the decode's 4 ONNX threads paid 2.5-6.8× the quiet wall (the 08:39 round: 47 s for 26 s of audio); the turn tree now spawns at nice 10, so the interactive server paths keep their cores exactly when there is contention and nothing changes when there is not; the web-terminal/tmux PTY spawns (the user's own terminal) and the in-server merge pool stay untouched |
| 2026-09-21 | this PR | M112 backup archive build, introduced with this PR: the tar stream's stdlib level-9 zlib replaced by isal's IGzipFile at the request path's level 1 — build median 43.8/43.1/42.8 → 2.6/2.5/2.5 s (−94 %, ~16.8×), maxima 43.8/43.7/43.0 → 2.6/2.5/2.5 s, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back over the committed builder's 4.71 GB / 67-file scratch synthetic home, each archive deleted after its reading, load 1.26-1.70 one-minute); effective rate 108-110 → 1844-1883 MB/s; component attribution, standalone compressor ladder over the corpus's largest file (577 MB chat-events JSON): zlib-9 (the before shape) 5.25 s / 110 MB/s / 40.0×, zlib-1 1.13 s / 512 MB/s / 30.7×, isal-1 0.30 s / 1913 MB/s / 34.3×, isal-2 0.30 s / 1924 MB/s / 34.3× (isal's level 2 prices as level 1), isal-3 2.06 s / 280 MB/s / 36.4× — isal-1 dominates zlib-1 outright (faster and smaller), and the wire trade is 42.8× → 36.9× (+16 %, 110 → 128 MB on the whole corpus), the price of the level the request path's `gzip_level1` already runs; archive parity: 56 members, name/size/mtime identical, member content digest ebc2644f4392 identical across arms, exclusions hold (threads subtree, credentials); live-home scale note: this host's included corpus is ~20.5 GB (sessions data 20 GB + cache 399 MB + memory 7.6 MB), pricing the pre-fix build at ~3.2 min of one core per handler fire and the after at ~11 s; 5924-passed suite + 9 skipped (the 3-passed backup suite among them: exclusions, secrets omission, plus a new round-trip test pinning the container reads back through `tarfile.open(r:gz)`), ruff and yapf clean; M112 definition, corpus builder (tests/backup_corpus_builder.py), collector, healthy range (median < max(2.0 s, corpus bytes ÷ 1200 MB/s) — the after band sits ~1.6× inside the bytes line), and history row introduced with this PR | the backup was the one compression holdout after the ISA-L landing moved the seven request-path memos, the middleware responder, and the trace-merge subprocess: tarfile's `w:gz` stream rides stdlib zlib at its default compresslevel 9, pricing the state dir's gigabyte-scale sessions corpus at ~110 MB/s — minutes of one pinned core per backup handler fire on every host that enables the built-in `backup` cron handler; the tar stream now rides one isal IGzipFile at level 1, the same level the request path's one-shot deflator runs |
| 2026-09-21 | this PR | M111 review-context chat-log scan, introduced with this PR: needle-at-end median 34.76/34.14/34.47 → 4.34/4.38/4.19 ms (−87 % to −88 %), absent-needle 39.38/34.84/34.35 → 3.44/3.55/3.44 ms (−91 %), every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, the 20.1 MB / 8158-line worst active live chat corpus of session fd80e6e7, load 1.07-1.51 one-minute); component attribution: the before scan parsed every line text-mode from the file start (the match for the newest thread sits at the file's tail, so the worker-completion shape paid a near-whole-file parse per completion, ~4.3 µs/line on this corpus) where the after scan rides one C-level `mm.find` over the mapping — the absent-needle arm is the pure scan, 3.44 ms / 20.1 MB ≈ 5.8 GB/s — and parses only a hit's enclosing line as a zero-copy view (the same provable-skip class the `type_line_filter` parse_filter sanctions, without the per-line Python loop); no-regression witnesses interleaved ×3: M95 newest-first scans review 0.09-0.11 ms / judgment-pair 1.05-1.19 ms (bands both) and M99 import server 0.498-0.543 → 0.499-0.522 s (band parity); 5923-passed suite + 9 skipped, ruff and yapf clean, plus 9 new reader tests (file-order hits, laziness, the unparsed needle-free skip, the hit-line skip contract, missing/empty files, the unterminated tail, multi-hit single yield, the empty-needle raise, mixed-corpus parity with the escaped-needle proof boundary pinned); M111 definition, collector, healthy range (median < max(0.005 s, bytes ÷ 2000 MB/s) both shapes — the line sits ~3x under the measured 5.8 GB/s scan floor and ~4x over the pre-fix 0.51 GB/s shape), and history row introduced with this PR | the review-context extract ran the one remaining whole-parse text-mode scanner on the worker-completion path: every delegation's reviewer and every improve round re-parsed the session's live chat log from byte 0 although the answer names one thread id — the needle proof cuts the scan to the C-level find plus the matching lines, and a corpus that grows re-prices the line automatically through its bytes term |
| 2026-09-21 | this PR | M110 remote ssh probe, connection reuse via ssh ControlMaster (the probe family's argv single-home `ssh_cmd` now carries ControlMaster=auto, a 0700 ControlPath under ~/.ssh/controlmasters, and ControlPersist=1200 s — the remote watch ladder's 600 s plateau plus its ≤10 s noise stays inside the window, so a watched host's probes never re-master while the watch lives): warm-master probe median 0.121/0.124/0.120/0.121 → plain (pre-fix shape) median 0.842/0.837/0.847/0.842 s (−85.6 % to −85.8 %), re-master median 0.841/0.842/0.846/0.853 s (parity with the plain arm — the master setup adds nothing to the cold shape), every paired round faster (four back-to-back invocations of the verbatim collector — branch
worktree after vs main checkout before, each invocation three interleaved rounds of 5 plain +
1 re-master + 5 warm probes against the standing watches' SLURM login host, read-only sacct,
rc 0 asserted on every probe — 33 per invocation, 132 branch probes plus the main invocation's
33, load 1.35-1.41 one-minute; the main checkout's own module arm reads 0.847/0.851/0.849 s — its argv carries no mux options, the before shape from the module); production carriers: the schedule-trigger verify-on-create probe — the hourly cron's remote-watch POST logged 1064-1255 ms server-side across 8/8 creations in the live server log (event loop idle, sibling requests 1-3 ms in the same windows) — and every remote watch-loop probe; stale-socket recovery verified standalone (kill -9 of the [mux] master → one 0.80 s recovery probe, rc 0, warmth after; ssh creates the socket 0600, the parent dir 0700); the argv-shape mocks in the remote watch tests re-pinned on the new layout (host/remote-command at argv[-2]/argv[-1]) and the new test_ssh_cmd.py pins the policy options plus the 0700 dir creation; 5914-passed suite + 9 skipped, ruff and yapf clean; M110 definition, collector, healthy ranges (warm median < 0.3 s, re-master median < 1.5 s), and history row introduced with this PR — the live server picks the fix up from its next deploy on | every remote probe paid one full ssh handshake — TCP + KEX + auth, ~0.85 s to the SLURM login host — on the verify path of every remote-watch trigger creation and again on each watch-loop probe (the backoff ladder's plateau re-pays it every ~600 s); one master per (local user, host, port) amortizes the handshake across the probes inside its idle window, the watched-host probe stream keeps the master warm, and the expire-or-stale cases degrade to at most the old single-handshake shape |
| 2026-09-21 | this PR | M92/M97/M98/M102 CLI verb walls, argparse's parser-build `shutil` import priced out (the shared `CliHelpFormatter`, src/cli/help_formatter.py, passes the terminal width itself under `shutil.get_terminal_size`'s documented precedence — `COLUMNS`, then the stdout terminal, then 80 — so the lazy `import shutil` inside `HelpFormatter.__init__`, whose module body drags `bz2` + `lzma` for archive support no verb uses, never runs; applied at every `ArgumentParser`/`add_parser` site in `src/cli/`): M92 schedule-trigger --help 0.039/0.039/0.039 → 0.036/0.036/0.036 s (−7.7 %), M97 plan list 0.061/0.060/0.060 → 0.058/0.057/0.058 s, M98 memory query 0.052/0.052/0.051 → 0.050/0.049/0.049 s, M102 artifact wrap 0.040/0.040/0.041 → 0.038/0.038/0.039 s (−5 %), every paired round faster (three interleaved rounds of the verbatim collectors — main checkout before vs branch worktree after back-to-back, seven fresh processes per arm per round, load 1.2-1.6 one-minute); component attribution, fresh-process `-X importtime`: the before arm's schedule-trigger parser build carries `shutil` 2.16 ms cumulative (`bz2` 0.87 + `lzma` 0.73 inside), the after arm's carries none; help byte-identity across arms: every covered verb's `--help` and the usage-error path render identical bytes at COLUMNS=40/200/0/abc and unset (the width pin in tests/test_cli_import_weight.py asserts the shutil-precedence readings, and the parser-build probe bans shutil/bz2/lzma for the plan/artifact/schedule-trigger chains); no-regression witnesses interleaved: M99 import server 0.529/0.529/0.517 → 0.530/0.530/0.532 s and M108 claude-sub 0.077/0.077/0.075 → 0.076/0.076/0.074 s medians (bands both, ×3 — neither chain builds a CLI parser); 5913-passed suite + 9 skipped, ruff and yapf clean; M92/M97/M98/M102 healthy ranges unchanged (the after medians sit at 36 %/39 %/16 %/11 % of their lines) | every `charliebot` verb is a fresh process, and the verb's first parser build paid argparse's lazy `import shutil` — the HelpFormatter width resolution — while shutil's module body imports `bz2` + `lzma` for archive support no CLI verb touches; the width is a pure derivation of `COLUMNS` and the stdout terminal, so the shared formatter computes it directly, the memory chain keeps its pure-local property (the leaf is stdlib-only), and the archive chain stays out of every parser build |
| 2026-09-21 | this PR | M97 plan-CLI command wall, the credentials read's `dataclasses` import left the verb chain (the last slice the M97 landing named after the client cut): wall median 0.074/0.069/0.071 → 0.063/0.060/0.061 s (−11 to −14 %), maxima 0.074/0.071/0.071 → 0.064/0.061/0.069 s, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, seven fresh processes per arm per round, load 1.56-1.66 one-minute); component attribution, fresh-process `-X importtime`: `src.core.credentials` cumulative 26.0 → 21.8 ms and the chain's `dataclasses`/`inspect` entries are gone (the standalone `dataclasses` chain measures 8.5 ms cumulative, of which `typing` stays as the module's direct import for `_HotReloadCache`'s `Generic`; the in-chain marginal is ~4-5 ms of inspect/copy plus the parser-build displacement); `Credentials` is a plain `__slots__` class — two keyword construction sites (the loader and the test seed), no dataclass machinery anywhere (no asdict/fields/replace), and the identity/equality pin in tests/test_cli_base_url_cache.py still passes; every internal-API verb (plan, delegate, improve, schedule-trigger with a request) reads credentials in its fresh process and sheds the same slice; no-regression witnesses interleaved: M92 schedule-trigger --help 0.039/0.038/0.039 → 0.039/0.038/0.039 s (bands both, ×3 — the --help chain imports no credentials), M98 memory query 0.053/0.051 → 0.051/0.050 s and M102 artifact wrap 0.040/0.040 → 0.040/0.040 s (bands both, ×2 each — neither chain imports credentials); 5909-passed suite + 9 skipped, ruff and yapf clean; M97 healthy range unchanged (the after medians sit at ~40 % of the 0.15 s line) | the credentials read was the one remaining pydantic-free module whose value object still cost the dataclasses machinery: the verb walls load the secrets file for the request's auth header in a fresh process, so the import is per-invocation, and `dataclasses` drags `inspect` for a two-field record no consumer reflects on |
| 2026-09-21 | this PR | M3 in-server 401 floor, the http_request access line left structlog's dispatch: floor median 47.94/45.49/48.70 → 25.35/24.62/24.48 µs (−45 % to −50 %), p10 46.27-47.31 → 23.49-23.90 µs, p90 54.43-61.95 → 28.17-35.18 µs, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, 1000 raw-ASGI 401 drives per arm, the branch arm installing the renderer its lifespan installs, load 1.56-1.59 one-minute); line attribution in-context: muting the whole line drops the drive 47.6 → 12.1 µs, the per-line proxy resolution (`LazyStructlogLogger.__getattr__` → `get_logger` + getattr) plus BoundLogger dispatch plus the five-processor chain price ~34 µs of it, and the direct render — stamp, the same `_LeanLineRenderer` instance, print — reads ~20; the chain keeps every other line, its TimeStamper replaced by the shared `_LocalStampProcessor` (localtime + f-string, 1.7 vs 4.2 µs standalone) and the value-render quote check moved from a per-value set build to one regex search (0.84 → 0.41 µs on a 55-char path); byte-identity: the access line compares equal to the chain's render from the level column on (pinned per middleware case in tests/test_request_logging.py), the lean-render battery is unchanged, and the stamp processor's output matches `TimeStamper(utc=False)` within one second (pinned in tests/test_log_line_renderer.py); no-regression witnesses interleaved: M92 schedule-trigger --help 0.039-0.042 → 0.040-0.041 s medians (bands both, ×3) and import-server 0.649-0.651 → 0.642-0.650 s (bands both, ×2); 5904-passed suite + 9 skipped, ruff and yapf clean; M3 healthy range unchanged (the after band sits 2.3x inside the 60 µs line) | the lean-renderer landing byte-identified the line but left it on structlog's per-line machinery — proxy resolution, BoundLogger dispatch, and five processor calls per request — 73 % of the floor it had just cut; the access line is a formatting task, so the middleware hands its fields to `log_http_request_line`, which stamps and renders through the same renderer instance the chain ends in; capture-based readers re-pin: the middleware tests capture the fields dict at the new seam and the rendered bytes against the chain |
| 2026-09-21 | #1959 (row recorded in this docs-only follow-up per the #1046 precedent, the landing PR shipped without it) | M102 artifact wrap wall, the assertion stack left the wrap verb's import chain: wall median 0.066/0.064/0.067 → 0.040/0.042/0.041 s (−24 to −26 ms, −36 % to −39 %), maxima 0.068-0.071 → 0.042-0.045 s, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, seven fresh processes per arm per round, load 1.74-1.78 one-minute; the standing sweep's same-hour reading 0.061 s median is the round's before arm); component attribution, fresh-process `-X importtime`: the parser-build chain reads ~8 ms (site excluded) and loads none of artifact_check/dataclasses/inspect/plan_diff, the wrap verb loads only the new stdlib-only `src.core.artifact_shared` — the removed slice was `src.core.artifact_check` at 22.8 ms cumulative (self 4.8, dataclasses 8.1 with inspect 6.8, plan_diff 4.3, html 1.8); the genre vocabulary single-homes in `src.core.constants.ARTIFACT_GENRES` with an import-time equality check against the assertion registry, the check/wrap shared slice (genre template map, byte-integrity rule pair — the single source the checker's gate and the wrap self-check judge identically) single-homes in the leaf, `artifact_check` re-exports, and the check verb imports the stack inside its dispatch (its wall keeps its work, spot run: all nine plan assertions execute); no-regression witness interleaved ×3: M92 schedule-trigger --help 0.041/0.038/0.042 → 0.039/0.041/0.040 s medians (bands both — the schedule-trigger chain imports neither artifact module); 5903-passed suite + 9 skipped, ruff and yapf clean, plus the artifact ban-set contract extended (src.core.artifact_check joins it) and the genres lockstep pin (the plan-vocabulary pattern); the charlie-code review's one finding (the rewritten docstring claimed nothing else enumerates genres while the leaf's GENRE_TEMPLATES is a third enumeration) landed as a same-branch docs commit; M102 healthy range unchanged (the after medians sit ~9x inside the 0.35 s line) | the artifact CLI imported `src.core.artifact_check` at module level for two argparse choices tuples, dragging dataclasses→inspect, plan_diff, and html into every `charliebot artifact wrap` invocation although the wrap verb's only parse-time need is the genre vocabulary; the same deferral shape the M97/M98/M102 landings applied to the config and asyncio stacks |
| 2026-09-21 | this PR | M99 server import floor, the master-turn chain and its two server-side dependencies left `import server` (the master-cc facade with its run/queue/relay/state modules plus `src.core.project_config` via the chat handlers and the trigger wake; the memory store via the worker prompt build; the compaction stack via the worker's relay decision): import median 0.512/0.512/0.511/0.515/0.510/0.516 → 0.501/0.504/0.502/0.505/0.503/0.502 s (−9.0 ms, −1.8 %), every paired round faster, round-median bands disjoint (before 0.509-0.516, after 0.501-0.505; six interleaved rounds × five fresh processes per arm of the verbatim collector — main checkout before vs branch worktree after back-to-back, PYTHONPATH pinning the checkout under test, load 1.09-1.64 one-minute; a second six-round run at load 1.61-1.89 read −25 ms, 12/12 paired rounds faster across both runs); component attribution, in-server marginal (config+models+streaming preloaded, fresh processes): master_cc chain 9.6 ms of which ~5 ms exclusive (memory 2.1, compaction ~1, relay/latex re-home to their other importers), memory 2.1 ms, compaction 2.3 ms standalone; the patch seams ride the established deferred-module pattern (the `src.core.master_trigger.run_message` and `src.api.chat.cancel_master` targets resolve through PEP 562 `__getattr__` + a globals-first loader, the `load_build_backend` contract — an existing binding returned untouched, materialized on the patch target's first attribute read); `run_and_finalize`'s `run_message` and `_build_worker_prompt`'s `assemble_worker` are plain function-level imports (no seam); no-regression witnesses interleaved ×3: M92 schedule-trigger --help 0.038/0.038/0.038 → 0.038/0.039/0.038 s and M108 claude-sub 0.076/0.075/0.075 → 0.075/0.076/0.076 s medians (bands both); 5902-passed suite + 9 skipped, ruff and yapf clean, plus the server ban-set contract test extended (master_cc + run/queue/relay/state, project_config, memory, claude_compaction join the ban set); M99 healthy range unchanged (the after reading sits at two-thirds of the 0.75 s line) | every server process paid the master-turn chain's import at startup although its only server-side callers are two request-time handlers and the trigger fire, and the worker prompt build and the relay-compaction decision each pulled one more module the import path never reads; the seam hooks are the one place a deferral needed design — the module attribute stays the tests' patch target and the loader's globals-first read is what makes a landed stand-in win |
| 2026-09-20 | this PR | M108 claude-sub launch floor, the launch argv/env assembly single-homed in a stdlib-only leaf: launch floor median 0.083/0.083/0.084 → 0.075/0.075/0.076 s (−8 to −9 %), maxima 0.086-0.088 → 0.077 s, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, PYTHONPATH pinning the checkout under test, load 0.64-0.67 one-minute); component attribution, `-X importtime`: the chain reads 55.2 → 48.5 ms — `src.agents.backends.base` (9.2 ms cumulative: runs 4.2 with subprocess+orjson/ndjson, process 2.5 with ctypes, mmap) and `claude_code` (0.4 ms) leave the launch for one argv-constants function and one env-dict builder, plus pty/tty (0.9 ms) lazy at the one PTY fork; the after chain's floor is asyncio (34.3 ms, whose own base_events pulls concurrent.futures/socket/subprocess/ssl) plus the launch's own stdlib imports; no-regression witnesses interleaved ×2 both orders: M92 schedule-trigger --help 0.038-0.039 s medians and M99 import server 0.592-0.599 s (bands both); 5900-passed suite + 9 skipped, ruff and yapf clean, plus the claude-sub ban-set contract test extended (base, claude_code, runs, process, pty join the launch ban set); M108 healthy range unchanged (the after reading sits at half the 0.15 s line) | the 2026-09-19 landing moved the pydantic stacks out but left the chain importing the whole backend ABC for three vendor-fixed flag strings and two pure assembly functions (`build_claude_argv`, `headless_claude_env`) — the ABC drags runs/process/mmap and claude_code the launch never reads; the names single-home in `src.agents.backends.claude_launch` beside the other stdlib-only leaves with base/claude_code re-exporting for their existing readers, the bridge's one event-builder use and the terminate path's `kill_process_group` import at their call sites, and `PtyAttachment.spawn` imports pty where it forks |
| 2026-09-20 | this PR | chat wire tool-input trim completed to the renderer's read set (toolInputSummary reads Bash command / Read+Edit+Write file_path / Glob pattern / Grep pattern+path / every other tool's first value; read fields keep TOOL_PREVIEW_CHARS, the input's other string values — Edit old_string/new_string, Write content — render nowhere and cap at 60; the workers-events projection's tool_use rows, which carried input uncapped, join the same preview): M96 paired A/B, one scratch snapshot of all 46 active sessions (metadata + data, master_runs excluded), one fresh process per arm driving every session's bootstrap raw-ASGI — total 3927416 → 3876835 B (−1.3 %), median 75791 → 75791 B, max 295085 → 272176 B (−7.8 %), worst sessions −7.8 %/−7.1 %/−1.4 %; M35 verbatim collector on the shared snapshot of the 20534-event worst live corpus: events page decoded 293374 → 272562 B (−7.1 %), wire 108293 → 101118 B (−6.6 %), view 119772 → 118895 B, bootstrap 77540 → 76663 B, handler medians 0.89/1.49/1.00 → 0.89/1.39/1.16 ms (noise); worker-events full-fetch body, input-heaviest on-disk log (5d9639e2/10d7d73c, 2.6 MB / 1285 events): decoded 1554619 → 1429076 B (−8.1 %), wire 393812 → 368842 B, handler median 4.66 → 4.70 ms (noise) — the uncapped input was the latent M34 line-tripper (a worker's whole Write content rode every 5 s poll body); M34 worst log (9.8 MB / 232 events) decoded 112732 B both arms and M94's corpus (658ef901) page 0.07 MB / streamed 2.0 MB both arms — their tool inputs are already small (the big rows are outputs, capped since M94); no-regression witnesses interleaved: M91 full-corpus _process_event replay 0.0140 → 0.0139 s, M38 fan-out 5 frames / 186 serialize calls 1 ms both arms with final-frame parity True; 5900-passed pytest suite + 626-passed node suite, ruff and yapf clean, plus the read-set contract tests (dead fields cap at 60 while read fields keep the wire cap, the worker-events tool_use row rides the same bound); M96/M34/M35/M94 healthy ranges unchanged (body ceilings — every after reading sits lower) | the 2026-09-12 trim (M94) bounded tool outputs and capped input values at 500 chars, but the renderer reads only one bounded summary field per tool row: every other input value — the Edit diff payloads, a Write's whole file content — rode the wire shapes (stream delta, committed message, events pages, view, bootstrap, and on the workers panel wholly uncapped) with nothing ever reading it; the wire now carries exactly the renderer's read set, markers still truthy, full text on the persisted events |
| 2026-09-20 | #1918 | M84 backend stream-line parse, the NDJSON line reader's errors="replace" repair round trip gated to orjson's invalid-UTF-8 class (replace rewrites invalid UTF-8 sequences and nothing else, so a structural failure — control character, truncation, bad literal — survives the replaced decode unchanged and its re-parse is provably dead work): tail-follow replay median 15048.2/14453.5/14557.7 → 5869.2/5849.3/5909.3 ms (−59 % to −61 %), maxima 15077.6/14527.8/14793.8 → 5963.8/5877.9/5933.8 ms; stdout-stream replay median 13778.1/13756.0/13769.3 → 6587.1/6464.5/6590.7 ms (−52 % to −53 %), maxima 13850.1/13825.4/14363.0 → 6648.1/6533.5/6794.1 ms, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, scratch copy of the 2147.5 MB / 61-line worst on-disk raw log whose single 2.1 GB line fails 'unexpected end of data' at column 2147479553, parity divergences 0 and 60/60 events in all six arms, load 1.77-2.42 one-minute); component attribution, standalone on the 2.1 GB failed line: orjson scan 5.56 s, tobytes 1.27 s, replace-decode 1.52 s, second orjson scan 5.28 s — the repair round trip was 8.0 of the 13.6 s the line costs per replay; the after readings sit 326-367 MB/s, inside the line's bytes ÷ 250 MB/s = 8.59 s corpus floor (before: 144-148 MB/s, the trip this round's sweep opened with — the corpus's giant failed line re-paid the repair every pass since the 2026-09-18 drain-copy landing); no-regression witnesses interleaved ×2: M78 parse_ndjson_file chat 3304.7/3319.9 → 3362.8/3369.2 ms and worker log 35.7/34.4 → 37.6/35.0 ms (run noise, both inside their corpus-floor lines), M31 events-summary read 0.0005 s both arms both rounds, M95 review 0.09-0.10 ms / judgment-pair 0.87-0.91 ms both arms, M13 read+transform 0.0000 s both arms; 5896-passed suite + 9 skipped, ruff and yapf clean, plus a new test pinning that a structural rejection skips with exactly one orjson attempt (the torn-multibyte rescue tests untouched and green); M84 healthy ranges unchanged (the after readings sit well inside their lines) | the standing sweep tripped the line this round and the trip was repair inflation, not the parse floor the line prices: orjson names its invalid-UTF-8 class with a stable message prefix at every position (encoding validated upfront, column 1 — verified against the lockfile-pinned 3.12.0 across structural positions), so the reader runs the repair only when the failure is that class, the only one the repair ever rescues; every line the old path rescued still rescues byte-identically, every structural skip still skips with its log line, and any future giant failed line — a class every covered backend's funnel can meet — stops paying the copy + decode + re-scan |
| 2026-09-20 | this PR | M7 token-usage changed-round collect, the Claude+Codex corpora walked once per collect and their directory listings memoized on each directory's own (mtime_ns, size) stat pair (the charlie-bot walk's one-pass contract and listing memo, extended to `_iter_jsonl_stats`): changed-round collect median 0.100/0.101/0.102 → 0.092/0.095/0.095 s (−6 to −8 ms, −6 % to −8 %), every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, load 1.8-2.3 one-minute with sibling crons live); component attribution, cProfile matched-shape changed round before vs after in fresh processes: the signature's claude+codex walks plus the serve walks read ~13+9 ms before (2675 claude dirs + 3253 codex dirs, ~15k directory entries re-listed per pass, twice per collect) vs 4 ms after (one pass, one stat per memoized directory); the standing M7 page-load reading moves with it (0.105 s median at the sweep hour, the same fresh-sources shape — the live corpus moves between page loads); M80 changed-round wall unchanged within noise across nine interleaved rounds both orders (before 0.1051-0.1120 s, after 0.1053-0.1130 s — the 2.09 MB churn re-parse dominates that wall, and the row digest 7c35d23a2edb is identical across every arm and the standing sweep, the parity witness); the charliebot walk's own ~70 ms (17,135 candidate + ~2,565 directory stats at this host's ~4 µs stat floor) is untouched and stays the changed round's largest slice — raw stat parallelism measured negative on this host (7,326 warm stats 26.1 ms sequential → 72.1 ms with 4 threads), so no thread pool; 72-passed token-tally suite + pages/accounts/cli-import-weight suites green, ruff and yapf clean; M7 and M80 healthy ranges unchanged (both readings sit well inside their lines) | every fresh collect — each /token-usage page load while logs churn, each changed round — re-listed the claude projects tree (2,675 directories) and the codex sessions tree (3,253 directories) twice, once for the corpus signature and once for the serve walk, ~15k directory entries re-scandir'd per pass; the same one-pass-plus-listing-memo shape the charlie-bot walk has run since its own landing now covers both trees: `_walk_jsonl_logs` walks each tree once into per-account rows that both the signature and `_walk_source` consume, and `_jsonl_listing` remembers each directory's subdirectories and suffix-matching file names on the directory's own stat pair — an entry's create, delete or rename moves that pair, a file append moves only the file's own mtime, which the per-candidate stat still takes every pass; walk order, note wording, and note firing rules are byte-identical (the memo stores names only; the claude replay-key order semantics ride the same scandir sequence as before), so the M80 churn round's row digest is unchanged |
| 2026-09-19 | this PR | M97 plan-CLI command wall, the verb request's http.client client stack replaced by a minimal socket client on the plain-HTTP path: median 0.090/0.090/0.089 → 0.073/0.074/0.073 s (−16 to −17 ms, −18 % to −19 %), maxima 0.093-0.112 → 0.077-0.083 s, every paired round and every paired max faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, a scratch CHARLIEBOT_HOME holding copies of the live config.yaml + credentials.yaml served both arms, the GET read-only against the live server, live home untouched, load 1.7-2.0 one-minute); component attribution, fresh processes per arm, 9 each: the request slice — client-stack import + round trip — reads 18.0-22.5 ms before vs 4.0-4.3 ms after, and `import src.cli.common` stays in band both arms (7.0-9.1 ms); the standalone measurement that named the slice: `import http.client` 16.3 ms in-process vs 2.3 ms for the socket client's socket+urllib.parse stack (urllib.parse already resident behind the editable finder); no-regression witness interleaved ×3: M92 schedule-trigger --help 0.042/0.042/0.041 → 0.042/0.040/0.041 s medians (standing band, --help makes no request); 5856-passed suite + 11 skipped, ruff and yapf clean, plus a fresh-process wire probe pinning the plain-HTTP request never loads http.client, ssl, or email.parser, and the two real-socket stub tests (the GET readback listener and the wire-shape capture: path+query, Content-Type, Authorization, body bytes) passing against the new client; M97 healthy range unchanged (the after medians sit at ~half the 0.15 s line) | the M97 landing had already cut the config model stack out of the verb wall and named the client its remaining slice: every plan/delegate/schedule-trigger/improve verb is a fresh process whose single internal-API call is plain HTTP against the built `http://localhost:{port}` base, yet `_send_request` imported http.client — dragging email.parser and ssl with it, 16.3 ms — for responses the internal API always frames with Content-Length; the plain-HTTP path now sends the same wire shape (Host, Accept-Encoding: identity, the caller's headers, Content-Length on a body) plus Connection: close over a raw socket and parses the status line, head, and body per framing (chunked, Content-Length, read-to-EOF), with any malformed line, framing mismatch, or short body raising into the sent-but-lost class — never a silent mis-parse — and the https branch (config-owned https base URLs) keeps http.client untouched |
| 2026-09-19 | this PR | M107 dir-merge member classification, non-trace sidecars skip instead of failing the build: the sweep's verbatim collector crashed on the worst on-disk dir — `ValueError: Not a Chrome-JSON trace (no traceEvents array): …/traces/align.gaps.json` out of the merge pool, the same rejection the `/perfetto/merged?dir=` route 500s on (every `*.json` member passed the route's first-byte sniff, one sidecar's parse killed the whole build, the merged view of that dir never served, M107 unmeasured) — and the verbatim collector on the branch reads 12 members 961.9 MB, 2,601,903 events, build median 3.88 s, max 3.90 s over 3, artifact 77.7 MB, event-identity digest 69328480354c (8 sidecars skipped, one `perfetto_merge_member_skipped` warning each naming the file; inside the unchanged max(8 s, bytes ÷ 200 MB/s) = 4.81 s line); the survivors' subset harness on main (the dir's real traces passed explicitly, the same collector shape) reads 2 traces 484.2 MB, 1,308,808 events, 3.26 s median — the pre-fix code's own build for the members that could build; no-regression witnesses on the branch: M66 single-trace merge 3.02 s (standing sweep 2.85 s, band), M88 direct-pass 2.32 s (standing 2.12 s, band), M99 import server 0.583 s (standing 0.528 s, band, the module-level lazy logger adds nothing at import), M92 schedule-trigger --help 0.041 s (standing 0.040 s, parity); 5852-passed suite + 11 skipped, ruff and yapf clean, plus 2 new member-form tests (a middle sidecar's survivors carry their own indexes and pid labels, an all-sidecar merge raises instead of shipping an empty artifact) | the dir shape merges every `*.json` beside the traces, and the classification of "is a trace" is only decidable at the parse the pool worker runs anyway — the freeze-repro dir's gaps/rows/summary sidecars passed the 64-byte sniff and one of them took the whole view down; the skip is loud (one warning per rejected file), a merge that skips every member still raises, and every ≥2-path merge rides the member form the same way (an explicit `?trace=a&trace=sidecar` request skips the sidecar with the same warning now); the single-path forms keep failing the build on a non-trace unchanged (the direct-pass validator and `merge_traces`' sequential walk over caller-chosen paths) |
| 2026-09-19 | this PR | M99 server import floor, the cold-storage, tally, spawn-pool, NCU, and trace stacks left `import server`: import median 0.578/0.576/0.574/0.565/0.571/0.584 → 0.548/0.554/0.570/0.555/0.538/0.573 s (−4 to −33 ms), every paired round faster (six interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, five fresh processes per arm per round, load 1.3-2.2 one-minute with sibling crons' collectors live; standing sweep reading the same hour 0.569 s median at load 0.6-1.0); component attribution, `-X importtime` before vs after: 24 modules leave the import and none join — the cold-storage sweep (storage_cool 2.2 + backup 0.1 + tarfile 1.2 ms), the tally stack (token_tally 4.0 + sqlite3/_sqlite3 0.4 ms), the spawn pool (multiprocessing + concurrent.futures.process + queue + _multiprocessing 1.1 ms), NCU report parsing (0.5 ms), the trace-merge stack (0.4 ms), and the wav container (wave 0.5 ms) — ~10.1 ms of module self-time, the rest of the wall win riding the removed dependency edges; the token-usage page's `TokenTally` annotation moves under TYPE_CHECKING and the spawn-pool holder's annotation quotes `concurrent.futures.ProcessPoolExecutor` (the module `__getattr__` whose first read imports .process); ext_usage reads the Claude default dir from src.core.home (the M98 owner) instead of through token_tally; no-regression witnesses interleaved: M92 schedule-trigger --help 0.046 → 0.043 s, M98 memory query 0.060 → 0.058 s, M102 artifact wrap 0.075 → 0.070 s, M108 claude-sub 0.101 → 0.092 s medians (bands), M66 merged build 3.61 s / M88 direct-pass 2.43 s / M107 multi-trace 8.36 s on the branch with the event-identity digest d73f1c00bfd3 identical to the standing sweep's; 5856-passed suite + 11 skipped (the token-usage route tests' collect seam moved with the deferral, the M98 shape), ruff and yapf clean, plus the server ban-set contract test extended (tarfile, backup, sqlite3, token_tally, storage_cool, multiprocessing, ncu_parsing, trace_merge, wave join SERVER_HEAVY_MODULES); M99 healthy range unchanged (the reading moves further inside its < 0.75 s line) | every module-scope import in the changed files serves a use site the import path never touches — the two built-in cron handlers (backup, cool storage), the merge pool and the Perfetto/NCU/token pages, and one wav write — so each now loads at its first call like croniter and jinja2 already do; the ext_usage constant's move also keeps the tally stack off every ext-usage poll process's import floor |
| 2026-09-19 | this PR | M108 claude-sub launch floor, the pydantic model stacks left the worker binary's import: launch floor median 0.219/0.222/0.231 → 0.090/0.094/0.093 s (−57 % to −61 %), maxima 0.230-0.235 → 0.095-0.098 s, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, PYTHONPATH pinning the checkout under test, load 1.64-1.70 one-minute); component attribution: `src.core.models` measured 117 ms cumulative on the before arm's `-X importtime` (pydantic + backend_models + the full session/API model construction) riding `backends.base → src.core.runs` for one enum (`BackendType`) and `claude_accounts` for one filename constant (`CREDENTIALS_FILE`) — both names now single-home in the stdlib-only leaves (`src.core.constants`, `src.core.home`) with the model modules re-exporting, so the launch chain builds no pydantic model; the after arm's `-X importtime` shows no pydantic/models entry (asyncio ~36 ms is the floor); no-regression witnesses interleaved ×2: M92 schedule-trigger --help 0.042-0.043 → 0.042-0.043 s medians and M99 import server 0.576-0.589 → 0.576-0.589 s (overlapping bands both); 5856-passed suite + 11 skipped, ruff and yapf clean, plus the claude-sub ban-set contract test extended (pydantic + src.core.models + src.core.backend_models + src.core.claude_accounts join the launch ban set); M108 healthy range recalibrated < 0.30 s → < 0.15 s with this PR | every cc-claude subscription worker and reviewer launch paid the full pydantic model construction twice over for two stdlib-only names — the backend-type enum riding runs' module scope and the credentials filename riding the account pool's — although the launch reads neither account model; the vocabulary single-homes beside the other CLI-parsed constants, the filename beside the login-dir names, and the account pool loads at its one transcript-read call site (tui's jsonl probe, memo-gated) |
| 2026-09-18 | this PR | M3 in-server 401 floor, the http_request log line's renderer moved from the dev ConsoleRenderer to a byte-identical inline renderer: floor median 57.85/59.36/59.22/59.72/58.24 → 44.35/44.70/43.85/45.09/45.64 µs (−21.6 % to −25.9 %), p10 55.90-57.91 → 41.10-43.60 µs, p90 83.45-94.38 → 63.53-73.90 µs, every paired round faster (five interleaved rounds of the A/B harness — main checkout before vs branch worktree after back-to-back, 1000 raw-ASGI 401 drives per arm, the branch arm installing the renderer its lifespan installs, main arm's drive reading the dev path main serves, load 1.5-1.8 one-minute); the doc collector on the branch reads 42.83 µs median (p10 41.41, p90 63.23); the log line's share measured standalone: muting the http_request line drops the main drive 59.4 → 16.0 µs — the line cost 43.3 µs of the 59.4 µs floor, 73 %; the curl standing collector cannot see a cut this size (client-dominated; the standing reading median 0.001 s at 18:43 stands until deploy); 5838-passed suite + 11 skipped (the byte-identity battery — every render compared against the dev renderer over level and event padding, value repr rules, the fallback shapes, and the configured chain, plus pins on the chain's local-time stamper and the env-aware color decision), ruff and yapf clean; M3 in-server floor healthy range introduced at < 0.000060 s (the after band 42.8-45.6 µs sits 1.3-1.4× inside; the line sits at the pre-fix dev-render floor, so a renderer regression trips it) | structlog's default chain ends in the dev ConsoleRenderer and the server never configured it — every http_request line paid the dev pad/repr machinery on the request path (43.3 µs of the 59.4 µs 401 floor, ~114k requests per 55.8 h of server log); the lean renderer reproduces that non-color line byte for byte for the common shape (timestamp, level, event, sorted key=value fields with the dev quoting rule), keeps the dev renderer for exception/stack/logger-name lines (the traceback formatter is the one shape it does not reproduce) and whenever the dev color decision (NO_COLOR/FORCE_COLOR/tty) selects colors, mirrors the default chain's local-time stamper (TimeStamper's own utc default is True — dropping the default chain's utc=False would shift every served stamp to UTC), and installs from the server lifespan's first startup statement — never at import, where it would tax the CLI floors the M92/M98 collectors measure, and never inside a capture_logs context, whose exit restores the config it entered with; the independent charlie-code review of the PR flagged exactly the two contract drifts (the utc default and the NO_COLOR/FORCE_COLOR mirror) plus a stale module docstring, all fixed before merge |
| 2026-09-19 | this PR | M105 file-arm serve chunking, 64 KiB → 1 MiB (starlette 1.0.0's FileResponse class attribute): png serve median 7.45/6.59/5.19 → 1.88/1.62/2.02 ms (−61 % to −75 %), maxima 9.36-7.18 → 3.86-4.10 ms; pptx 4.64/4.54/3.68 → 1.40/1.25/1.46 ms (−60 % to −70 %); html witness 28.50/29.19/30.59 → 20.44/17.16/19.20 ms (−29 % to −45 %), maxima 37.96-34.51 → 23.91-24.40 ms, every paired round faster (five interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, the worst on-disk artifact corpora of the live sessions tree, scratch credentials home per drive, live files read-only, load 3.8-4.6 one-minute; transport identity kept on png/pptx, gzip kept on the html witness); wire: identity arms byte-identical across arms, html witness 2972245 → 2974307 B (+0.07 %, the middleware's per-chunk deflate now batches 4 × 1 MiB instead of 61 × 64 KiB — different gzip block boundaries over the same level, decompressed body byte-identical); component context: the 64 KiB default prices the page-cache serve at ~250 MB/s (one executor hop + one ASGI send per chunk; 16 chunks per MB, 21 for the png corpus), and the sweep's png reading had sat at its line (5.64 vs max(5.0, bytes ÷ 250 MB/s) = 5.38 ms) with the live log showing a 1.46 GB trace json served at 7.65 s, both per-chunk-bound; 5805-passed suite + 11 skipped (two new tests: the route builds the 1 MiB-chunk subclass, and a >1 MiB binary serves byte-identical with identity transport), ruff and yapf clean; M105 healthy ranges unchanged (every moved reading went further inside its line) | starlette 1.0.0 exposes the read chunk size only as the FileResponse class attribute, so the file arm serves through a one-attribute subclass; the chunking is transport-only — the served bytes are the file's bytes in both arms, and the Range path's min(chunk, remaining) clamp keeps byte ranges exact; the html witness rides the same FileResponse through the compressing responder (the collector's scratch sessions_dir resolves the live-tree artifact to no session, so it never reaches the injected-page memo), which is why the witness moves with the same change |
| 2026-09-18 | this PR | M7 restart-cold seeded row-memo build removed (the sidecar's parsed rows map adopts as the memo in place): restart-cold wall median 1.333/1.282/1.277 → 0.947/0.952/1.003 s (−25 % to −29 %), maxima 1.333 → 1.003 s, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, the live cache document copied per arm with its sidecar and the 23.4 GB live db read in place mode=ro by the collect itself, live home never written, load 1.9-4.1 one-minute with the full test suite running on the host); component attribution standalone: the per-row tuple comprehension over the 190,497-row sidecar measures 348 ms vs 5 ms for the direct `dict.update` of the same map; payload parity on a deterministic scratch corpus (3000-row opencode db + one appended row rebuilt per round, per-arm cache dirs): the opencode row identical across all six arms (oc-m 3001 calls, 15007 in_fresh, 3002 output — the appended row's +7/+2 landing in every arm), the whole-rows digest moving only with the live claude corpora between rounds; 5732-passed suite + 11 skipped (the restart-cold seed tests' exact-record contracts among them), ruff clean; M7 healthy ranges unchanged (the standing collector's reading moves 1.29 s → ~0.98 s medians inside the unchanged max(0.5 s, bytes ÷ 25 MB/s) line) | the seeded restart rebuilt the 190k-entry row memo from the just-parsed sidecar rows one tuple at a time (~0.35 s of the ~1.3 s wall) although the seed's [time_updated, record] lists index positionally exactly like the (time_updated, record) tuples every memo consumer reads — [0]/[1] and two-name unpacking — and a memo value is only ever replaced whole, never mutated, so the parsed lists alias into the memo and the build drops to one C-level dict update; the gate-pass restart shape saves the same build (its memo build ran before the probe check) |
| 2026-09-18 | this PR | M95 worker-log newest-first scans, the from-the-end walk moved from window reads to a mapped backward scan: review-scan median 0.60/0.59/0.83 → 0.09/0.10/0.09 ms (−85 % to −89 %), failed-iteration judgment-pair median 6.17/6.00/6.46 → 0.93/1.04/0.92 ms (−83 % to −86 %), maxima 6.21-6.81 → 1.01-1.23 ms, every paired round faster (three interleaved rounds of the verbatim collectors — main checkout before vs branch worktree after back-to-back, the 9.8 MB / 232-line worst on-disk worker log carrying one 9.5 MB tool_result line, live home read-only, resolved blocker/summary/report identical across all six arms, load 1.71-1.89 one-minute); no-regression witnesses interleaved: M85 verify-finalize report read 0.6-0.8 → 0.2 ms medians (maxima 1.3-1.5 → 0.3 ms), M31 steady-state events-summary read 0.0008-0.0009 → 0.0005 s medians; 5713-passed suite + 11 skipped (one new test: the plain-filter walk's whole-line contract; the two walk early-stop tests re-pinned on the mapped scan's rfind extents), ruff clean; M95 healthy ranges recalibrated review < 0.005 s → < 0.001 s and judgment-pair < 0.012 s → < 0.004 s with this PR | the from-the-end walker memcpy'd its way through every byte between the consumer's answer and the file start one 512 KiB window at a time — cProfile put 4.0 of the pair's 7.2 ms in BufferedReader.read walking the 9.5 MB tool_result line whose head rejects it, the reads finding the line's opening newline; the backward scan now rides a read-only mapping (the parse_ndjson_file mechanism the #1785 zero-copy walk gave the whole-file parse): lines are zero-copy views between mmap.rfind newlines, a head-provable filter rejects a giant line for one bounded 256-byte probe, and an early stop never scans past its answer; 300 randomized trials × 3 filter shapes (none / head-provable / plain) output-identical to the old walker; the same walk serves the M31 summary read (parse_ndjson_tail_parseable), M85 (_resolve_final_report), the reviewer-completion scan (review.py), verify_trailer, and the codex rollout backward scans |
| 2026-09-18 | this PR | M107 multi-trace merge compressor moved from `gzip -1` to the isal igzip CLI (same subprocess shape): build median 11.67/11.87/11.57 → 8.34/8.11/7.35 s (−29 % to −36 %), maxima 12.06-12.09 → 8.82-7.37 s, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, 12 traces / 2089.8 MB / 5,279,591 events of the step002110 dir, scratch cache home per arm, live traces read in place read-only, load 1.7-2.3 one-minute; event-identity digest d73f1c00bfd3 identical across all six arms; artifact 170.3 → 160.5 MB, −5.8 % wire); component attribution standalone: the level-1 `gzip` subprocess reads the members' 131 MB fragments at 201 MB/s single-core while the same level through isal's igzip CLI reads 797 MB/s (−5.7 % wire), and the ordered fragment stream — fragment k copies only after k−1 streams — made the stream's 2.6 s-per-wave drain the wall beside each 2.07 s wave (12 members / 4 workers = 3 waves; stream total 1.57 GB at 201 MB/s = 7.8 s of serialized copy vs 6.2 s of member waves); the isal stream's 2.0 s total returns the wall to the member waves; no-regression witnesses interleaved: M66 single-trace merge 3.87/4.15 → 3.85/3.78 s medians (the walk writes into the live pipe at 63 MB/s, under both compressors' pace — wall unchanged, artifact 21.5 → 20.4 MB) and M88 direct-pass 2.46/2.49 → 2.47/2.45 s (the 2.72 s validation parse is the floor; artifact 23.8 → 23.7 MB); 5712-passed suite + 11 skipped (test_trace_merge's walk-failure kill/reap contract rides the same Popen seam), ruff and yapf clean; M107 healthy range recalibrated max(14 s, bytes ÷ 150 MB/s) → max(8 s, bytes ÷ 200 MB/s) with this PR | the trace merge was the one gzip holdout after the ISA-L landing moved the seven one-shot memos and the middleware to isal — the 2026-09-17 member-parallel landing multiplied the fragments flowing through the ordered stream 4-wide while the subprocess still read 201 MB/s, so every wave's 524 MB burst drained slower than the next wave built |
| 2026-09-18 | this PR | M6 append-round collector repaired, max line prices the served path: max 50.67/50.71/51.61 → 0.37/0.32/0.33 ms (−99.3 % to −99.4 %) over three interleaved rounds of the verbatim collector — old drive vs new drive back-to-back, the 20534-event worst live corpus copied to a scratch `CHARLIEBOT_HOME` per arm, live home read-only, parity True all six arms, load 1.60-1.87 one-minute; medians unchanged 0.058-0.07 ms both arms; component attribution: the first timed resolve's declared-window warning path (`_warn_declared_window_once` → `LazyStructlogLogger.__getattr__`) fired the lazy `import structlog` inside the timed region — 109 modules, 77.8 ms standalone cProfile wall — while the served process imports structlog at startup; the collector imports structlog beside its other imports, one line; healthy range unchanged (the line watches the median, 100x inside) | the append-round max is the metric's jank signal — the 3 s usage poll during a streamed turn — and priced the collector process's own lazy logger import (the vacuous-read class the M34/M55/M65/M70/M72 repairs called out), so a real 50 ms fold stall would have read the same as the harness artifact; collector command only, no product code |
| 2026-09-18 | this PR | M84 tail-follow drain copy removed (mmap line walk): tail-follow replay median 3608.6/3378.1/3353.4 → 3053.1/2863.9/2776.7 ms (−15 % to −17 %), maxima 3730.5-3397.3 → 3079.6-2787.7 ms, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, the 1051.1 MB / 186-line worst on-disk raw master-run log copied to scratch per arm, live home read-only, load 0.64-2.47 one-minute); the untouched stdout-stream funnel rides the same rounds as the in-collector no-regression witness (2937.4/2736.7/2732.8 → 2916.8/2745.2/2689.6 ms medians, band); component attribution, phase-instrumented replay of the pre-fix funnel on the same corpus: parse_ndjson_line 2680 ms of the 3582 ms total, the drain's `f.read()` readall 554 ms, find+slice 54 ms — the whole-backlog bytes copy the mapping removes (the after arm reads 344-378 MB/s, the stdout funnel's own 378 MB/s parse-floor class); cursor-on witness on the branch: recorded offset 1051067581 == file size exactly, and a re-attach at the recorded cursor replays 0 events; 5712-passed suite + 11 skipped (test_backend_stream_parse's staged-partial, multi-MB-carry, torn-tail-drop, and cursor-checkpoint contracts among them, plus test_master_restart_recovery_e2e's real re-attach shape), ruff and yapf clean; M84 tail-follow healthy range recalibrated < max(0.060 s, bytes ÷ 200 MB/s) → < max(0.060 s, bytes ÷ 250 MB/s) with this PR — both funnels now share the parse floor the line tracks | the tail-follow drain read the whole backlog into one bytes object per round before splitting it — a full memcpy of every drained byte riding the live read side of every covered backend's turn and the restart re-attach replay; the drain now maps the file read-only each round and splits its lines from the mapping (parse_ndjson_file's zero-copy walk, the mechanism #1785 gave the whole-file parse), with a scan watermark so the torn tail is re-examined only when the producer adds bytes, and the offset/cursor bookkeeping byte-identical (the cursor-on witness pins it); the writers only ever append — a truncated mapping would SIGBUS, the contract parse_ndjson_file's own docstring states |
| 2026-09-18 | this PR | The request path's level-1 deflate moved from zlib to ISA-L (isal 1.8.0), one deflator across the seven one-shot gzip memos (compare view, clean view, listing, events download, cron snapshot, events page, fast_json bodies via gzip_body_response) and the middleware responder's streaming file: M55 first view 0.2154/0.2207/0.2170 → 0.1317/0.1261/0.1346 s (−38 % to −42 %), M70 first view 0.1729/0.1690/0.1725 → 0.0793/0.0785/0.0804 s (−53 % to −54 %), M101 first view 3848/4000/3929/3831/3986 → 1514/1502/1524/1507/1484 ms (−61 % to −63 %), M105 html witness 117.55/120.97/123.05/120.54/120.64/121.24 → 25.59/22.86/25.21/23.01/22.77/22.03 ms (−78 % to −82 %, transport gzip kept), every paired round faster; M46 cron tasks 0.23-0.29 → 0.20-0.24 ms (the responder's deflate state now builds at the first deflated body and closes through an exit stack, so a request the middleware skips — precompressed, excluded type, small body — constructs no file objects and binds send where IdentityResponder.__call__ did); M72 changed-round 5.80-5.94 → 5.75-6.71 ms, band parity (the walk dominates the rebuild; the listing's deflate slice is ~1 ms at the new floor) — three interleaved rounds of the verbatim collectors (main checkout before vs branch worktree after back-to-back, live corpora read-only, load 0.9-1.4 one-minute); component attribution standalone: `gzip.compress` level-1 reads 39 MB/s on the 4.0 MB base64-bearing worst artifact page and 314 MB/s on a 64 MB chat-NDJSON slice while the same level through ISA-L reads 385/1212 MB/s at −2.2 %/−0.8 % wire; no-regression witnesses interleaved: M34 repeat 1.20 → 1.19 ms with decoded 112732 B identical (cold band 114-122 → 114-117 ms, the 9.8 MB parse dominates), M35 events/view/bootstrap 1.05/1.38/1.01 → 1.02/1.62/1.03 ms band with decoded digests identical and wire +3.4 % on the events page (isal's ratio on chat JSON), M36 full poll 0.65-0.67 → 0.62-0.68 ms with digest identical and wire 15319 → 13127 B (−14 %), M56 /status 0.50 → 0.49 ms (wire −5 %), M59 full row 0.57 → 0.53 ms / attach 0.45 → 0.40 ms, M99 import server 0.691 → 0.682 s, M92 import src.cli.common 0.040 s both arms; 5712-passed suite + 11 skipped, ruff and yapf clean; the gzip container keeps mtime=0 so equal input stays byte-identical across processes, and the seam the repeat-fetch tests patch moved from the shared `gzip.compress` attribute to each module's `gzip_level1` binding | the deflate is data-dependent slow at level 1 on zlib — 103 ms of the M55 first view's 211 ms was the 4 MB base64-bearing page's pass (39 MB/s, the M105 png row's own ratio class), and the events download's 3.8 s first view was 3.1 s of compress at 314-332 MB/s; ISA-L runs the same level-1 container 4-10x faster at −14 % to +3.4 % wire by body, so the healthy ranges stand unrecalibrated (every moved reading went further inside its line) and the wire trade is per-body ±3 % |
| 2026-09-17 | this PR | M89 stderr-pump reading corrected, docs-only calibration, no code change: the collector re-opened the same scratch stderr.log with O_TRUNC for every timed round, and on this ext4 /tmp the open's truncate of the just-written 3.2 MB log costs 3.51-4.09 ms per round (phase-attributed rounds: open 3.51-4.09 ms, tee 14.6-17.1 µs/chunk, close 0.22-0.26 ms) — 34-40 % of the standing reading — while no run issues it: `_stream_stderr` opens the run's log once at pump start and pumps to exit, and `_rotate_stale_transport` moves a prior attempt's log aside instead of truncating it, so the run's fs shape is one open of a never-before-written path. The collector now gives every round (warm pass included) its own fresh scratch path under /tmp — the open-once-then-pump shape the run issues, still through the real `AgentBackend._stream_stderr` (the vacuous-read guard holds). Verbatim interleaved rounds on the pinned main checkout, one warm pass + five timed rounds per reading, load 1.36-1.50 one-minute: old-shape medians 24.6/24.7/25.3 µs per chunk (maxima 25.7-27.7), fresh-path medians 16.0/16.3/16.6 µs (maxima 17.5-19.6), −32 % to −37 %, every paired round faster; the corrected reading sits 3x under the unchanged < 0.00005 s line and matches the pump's sibling shape (the M90 stdout pump's 13.2 µs plus the tail-keep's ~2 µs in-funnel) | the definition's "exactly as the run issues it" was contradicted by the collector's own per-round re-trunc: the round models one run's pump, and a run never re-truncates a log it just wrote — the standing reading priced an ext4 dirty-truncate artifact, not the pump |
| 2026-09-17 | this PR | M34 served-path repair + full-fetch FastJSON/gzip memo, three interleaved rounds of the repaired collector — main checkout before vs branch worktree after back-to-back, the 9.8 MB / 232-event worst on-disk worker log copied per arm, live home read-only, load 1.3-2.3 one-minute: full fetch repeat median 2.00/1.85/1.94 → 1.11/1.17/1.13 ms (−42 % to −45 %), maxima 2.08-2.16 → 1.38-1.48 ms, every paired round faster; after=total median 0.39/0.40/0.40 → 0.44/0.39/0.39 ms (band — the envelope path is byte-identical, only the handler's Request injection added); component attribution: the removed slices are response_model's jsonable_encoder pass over the 232-event list (the ~6x model_dump pass the envelope path's own note names) and the middleware's per-request level-1 deflate of the 112732 B body, replaced by pre-dumped rows through FastJsonResponse and the body-keyed gzip memo (one off-loop deflate per distinct projection, Content-Encoding set upstream); wire 28111-28116 B both arms, decoded 112732 B both arms; cold first fetch unchanged 116.27-121.38 → 117.01-121.04 ms (the process's first request: the 9.8 MB parse the M78 worker-log line prices plus first-request route/serializer setup, both arms); parity every arm: repeat digests single per shape, envelope-at-0 rows == full fetch's parsed rows (collector-asserted; cross-arm digests differ by design — the read path stamps missing timestamps with now() per process); 19-passed events/threads test files, full suite green, ruff and yapf clean; M34 healthy ranges recalibrated < 0.02 s → < 0.002 s (after=total) and the full-fetch repeat line introduced < 0.002 s with this PR | the full fetch (panel open / count-ahead reset) returned the mapped list through response_model — the jsonable_encoder pass the envelope path's after=N repair had already routed around — and its 112732 B body paid the gzip middleware's whole-body level-1 deflate on every request; both fetch shapes now ride the pre-dumped FastJsonResponse rows, and the full fetch's gzip form rides the body-keyed memo beside the plain render (the M59/M71 single-home), so a re-open of an unchanged panel serves stored bytes and a fresh body pays one off-loop deflate; the standing collector rode the TestClient harness and never mounted the middleware — the vacuous-read class — so the repaired raw-ASGI drive (the M35/M71 pattern) reads the served path the middleware and route actually run |
| 2026-09-17 | this PR | M108 standing sweep's ghost trip diagnosed as a stale-default-checkout measurement, not a product regression, and the sweep's checkout pinned by a preflight, docs-only: the sweep's verbatim M108 collector read median 0.400 s, max 0.409 s against the < 0.30 s line 46 minutes after #1761 merged, while the same collector against a worktree at origin/main (20749aca) read 0.204/0.209 s; after restoring the default checkout from the sibling branch it sat on (`code-health/single-home-threads-seeded-session-rig`, its PR #1755 merged ~3 h earlier, tree clean) to origin/main, the verbatim collector on the restored checkout reads median 0.242 s, max 0.259 s (load 2.03-2.31 one-minute across the three readings) — inside the line; the 8-17 ms delta over #1761's after band (0.225-0.234 s at load 2.01-2.23) reads as this round's higher load; the stale tree's `git merge-base --is-ancestor 9e913956 HEAD` is false — it predated #1755-#1765, among them #1757 (M66) and #1761 (M108); M66's stale-vs-fresh readings 3.87 → 3.83 s sit inside the 8 s line either way — the bias is silent where it does not trip; preflight lands at the top of the collector list this PR edits, the cron prompt's measure paragraph points at it | every in-process collector imports the code under test from the repo's local main checkout and nothing pinned that checkout: a sibling cron's leftover branch made the hourly regression watch measure a pre-#1761 tree and report a ghost M108 trip (and reads pre-#1757 M66 code the same way, silently); the preflight fetches (loud on failure — a stale `origin/main` would make the compare pass while the tree sits behind), asserts a clean tree (tracked or untracked dirt exits before any mutation, the carrying that `switch` would otherwise do silently), compares `rev-parse HEAD` with `origin/main`, and restores with `git switch main` + `merge --ff-only` — a diverged checkout fails the merge loud, and the round reports the in-process metrics unmeasured, never measuring a stale tree |
| 2026-09-17 | this PR | M108 claude-sub launch floor, introduced with this PR: fresh-process `claude-sub --<unsupported-probe-flag>` wall median 0.397/0.405/0.404 → 0.225/0.224/0.234 s, −42 % to −45 %, maxima 0.417-0.420 → 0.230-0.240 s, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, PYTHONPATH pinning the checkout under test, load 2.01-2.23 one-minute); component attribution, `-X importtime` fresh-process: fastapi 128 ms rode `src.agents.backends.pty_common`/`tui` although both carry the `WebSocket` name in TYPE_CHECKING-only positions (future-annotations keep the hints unevaluated) and the relay's one runtime consumer (`WebSocketDisconnect`) now imports inside the function; src.core.config 107 ms rode `backends.base → src.core.runs` (the `CharlieBotConfig` hints) and `claude_accounts` (same) — all under TYPE_CHECKING, and `base`'s one runtime `get_config` call site (the cgroup read) imports it there; the login-dir names (`CLAUDE_CONFIG_DIR_ENV_VAR`, `default_claude_dir`) single-home in src.core.home beside the profile home (env/HOME derivations no config key moves; config re-exports for its existing readers), and `claude_sub`/`claude_code`/`tui` import them from home; no-regression witnesses interleaved: M99 import server main 0.676-0.691 vs branch 0.661-0.711 s (overlapping bands at load 2.2-2.3 — parity) and M92 schedule-trigger --help 0.040 s (standing 0.041 s); 5707-passed suite + 11 skipped, ruff clean, plus the claude-sub ban-set contract test (fastapi + src.core.config + yaml + src.core.credentials stay out of the worker binary's import); M108 definition, collector, healthy range, and history row introduced with this PR | every cc-claude subscription worker and reviewer launch paid a ~0.40 s import floor before the claude CLI could start — 128 ms of fastapi (the PTY module's WebSocket hints) and 107 ms of the config model stack (backend-ABC and account-pool annotation imports) although neither serves the worker binary's launch path; the residual floor is the account pool's runtime models (pydantic + backend_models + src.core.models, ~117 ms) which the launch genuinely needs |
| 2026-09-17 | this PR | M61 all-sessions and archived-page sub-readings priced, docs-only calibration, no code change: the collector prints four idle-cold readings but the definition priced only bare listing and single get_session, so the archived tab's page (the real `list_archived_page` shape) and the collector's full-set probe tripped nothing. Readings the lines are set from — all-sessions (the collector's `status=None` probe over the whole cached set; the production routes filter first and copy only their subsets, so no route pays this shape): 5.50-5.78 ms at 1075 cached metas (#810's after arm, 2026-09-05), then 5.95/6.81/6.46 ms at 1238/1237/1241 metas (this round's three sweeps 11:45-14:43, load 1.5-1.9 one-minute) — 4.8-5.5 µs per cached meta across the 1075→1241 growth; archived-page: 2.09-2.13 ms at 1075 metas (#810's after arm), 0.99-1.09 ms today; the priced sub-readings stand inside their lines (bare listing 0.03-0.05 ms, single get_session 0.042-0.055 ms) | all-sessions median < max(0.008 s, cached-metas × 0.000008 s): the probe's per-meta copy+sort floor held at 4.8-5.5 µs across the corpus growth, so the line tracks the cached set (the archived share never expires from it) the same corpus-tracking form the M78/M84/M101/M107 bytes-lines use — the 8 µs figure is 1.5-1.7x over the measured band and its margin carries the fresh-manager shape's cold sidebar-probe term; archived-page median < 0.003 s, 1.4x over #810's after band and 2.8x over today's readings |
| 2026-09-17 | this PR | M107 multi-trace merged-trace build, introduced with this PR: 12 traces / 2089.8 MB / 5,279,591 events (the worst on-disk multi-trace dir, ~/data/hayden_243809_traces/step002110) → merged build median 26.25/25.56/25.37 → 11.62/12.11/11.58 s, −54 % to −57 %, maxima 26.93-26.83 → 11.66-12.34 s, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, scratch cache home per arm, live traces read in place read-only, load 1.73-2.39 one-minute with the full test suite running on the host; artifact 169.7 → 170.3 MB with the event-identity digest d73f1c00bfd3 identical across all six arms — the +0.6 MB is the members' re-numbered tid digits); component attribution on the pre-fix build: the wall is the twelve members' sequential parse+remap walks (~2.6 s per ~174 MB member measured on the after arm's per-member fragments) while the merge pool's second worker idles; no-regression witnesses on the branch: M66 single-trace merged build 3.85 s median (the sweep's main-checkout reading the same hour 4.41 s; the sequential single-stream path is untouched) and M88 direct-pass 2.47 s (standing 2.69 s), both inside their lines; 5666-passed suite + 11 skipped, ruff and yapf clean; M107 definition, collector, healthy range, and history row introduced with this PR | the dir-merge shape walked every trace inside one pool worker — an N-trace merge paid the sum of N parse+remap walks (the gzip run trailing them) while the pool's other workers idled; each trace's walk now runs as its own merge-pool task and the parent streams each member's fragment into the single gzip subprocess the moment its task returns, so the wall is the slowest wave of members (12 traces over 4 workers = 3 waves ≈ 8 s) plus the gzip tail the streaming already overlaps; the members allocate sequencer ids inside per-member strides (16.7M ids, the largest observed trace 1.07M events) with a loud overflow raise, the artifact stays the single-member deterministic gzip it always was, and the round's real click — yesterday's 11.15 s /perfetto/merged request in the server log — is the production shape this collector prices |
| 2026-09-17 | this PR | M106 switch-during-stream repaint, introduced with this PR: 98.0 KB draft (sha1 6e0cb6e8f159, the M33 corpus), +200 B delta per switch, 7 rounds — main checkout before vs branch worktree after, five interleaved back-to-back rounds of the verbatim collector: before medians 12.97/13.47/13.58/15.77/19.29 ms (maxima 19.17-27.07 ms), after medians 0.39/0.40/0.41/0.46/0.56 ms (maxima 0.47-0.69 ms), every paired round 30-40x faster, frame parity true in all ten arms; real-Chrome corroboration, the same hide+re-show probe over a 63 KB mixed-CJK draft driven against the live server's pre-fix assets 62.7-71.4 ms per paint vs the branch tree's served assets 0-2.6 ms; served-path context: the dashboard's diag_switch client telemetry read 119-144 ms hourly medians across yesterday's streamed-turn workload (n=364) and 64-231 ms per switch this morning pre-reload, while instrumented drives of the same sessions on current assets read 13-16 ms; no-regression witnesses on the branch: M33 replay wall median 0.038 s (standing 0.037 s), M54 paint-work median 0.094 s (standing 0.080 s, band), the 21-case stream parse/render suites plus the 619-passed node suite and the 5660-passed python suite + 11 skipped, ruff and yapf clean (no Python files touched); M106 definition, collector, healthy range, and history row introduced with this PR | hideStreaming reset the incremental stream parse state unconditionally, so every switch's re-show of the same pending draft re-parsed the whole accumulated draft (the parse ~13-20 ms of the collector's before reading on the 98 KB corpus, the wrap and DOM the rest) although parseStreamDraft already gates state reuse on the next draft extending the parsed prefix — the gate is the validator, so the state now survives the hide and a mid-stream switch re-parses only the appended tail; a different session's draft fails startsWith and parses fresh (pinned by the new hide+re-show tests) |
| 2026-09-17 | this PR | M97 plan-CLI command wall, three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back at load 2.0-2.1 one-minute: median 0.228/0.227/0.224 → 0.088/0.090/0.091 s, −60 % to −62 %, maxima 0.230-0.241 → 0.091-0.093 s, every paired round faster (one shared scratch `CHARLIEBOT_HOME` holding a copy of the live config.yaml + credentials.yaml served both arms — the 2026-09-16 row's protocol, the pair isolates the code; the GET is read-only against the live server, live home untouched); component attribution: the request path's config import (pydantic + yaml models, ~150 ms fresh-process, measured standalone) left the verb wall whole; no-regression witnesses interleaved: M92 schedule-trigger --help 0.041-0.043 → 0.042-0.045 s (band), M98 memory query 0.054-0.055 → 0.055-0.057 s (band), M102 artifact wrap 67.1-70.1 → 69.0-69.8 ms medians (band), M99 import server 0.684-0.694 → 0.683-0.687 s (parity — the moved credentials module rides config's chain at ~0 marginal cost); 5676-passed suite + 11 skipped, ruff and yapf clean, plus 9 new cache/seam tests (the subprocess hit probe pins `src.core.config` and pydantic staying out of the verb process); M97 healthy range recalibrated < 0.40 s → < 0.15 s with this PR | every `charliebot` verb is a fresh process whose request path imported the config model stack for one field — the server port — and the auth header pulled the same module for credentials.yaml; the port now rides a fingerprint-keyed document under the profile home (config.yaml + config.py mtimes are the key, written only by a full get_config() resolution, so a config edit or deploy re-prices with one full read and a broken config never plants a cache — the loud full path stays the only resolution semantics), and the credentials loader splits into src/core/credentials.py (stdlib + home/log/yaml only, config.py re-exporting every name) so the auth header keeps its hot-reload contract off the heavy import; the cached value is only ever a value the real loader produced — nothing is reimplemented |
| 2026-09-17 | this PR | M80 changed round under append churn, three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back at load 1.89-2.50 one-minute: wall 0.1268/0.1161/0.1228 → 0.1032/0.1074/0.1070 s, −7.5 % to −18.6 %, every paired round faster, rows digest 682aad2534e3 identical across all six arms, scanned 2.06 MB per arm (the worst claude transcript grew ~10 KB between the round-1 arms — the digest pins the payload); component attribution: the corpus walk standalone 78.7 → 67.0 ms median, 92.7 → 77.3 ms max over 7 warm passes with 7267 candidate rows identical — the walk's ~19k stat syscalls (~63 ms at the measured 3.2-3.9 µs/stat on this host) are the corpus floor the trim leaves untouched; no-regression witness: M7 changed-round harness pairs read 0.106/0.173 s branch vs 0.186/0.179 s main with the live opencode db's per-arm churn dominating the pair (0 vs 2 sqlite executes per round, the 61 ms probe+key-diff lands only when the WAL moved since the stored entry) — the branch never slower with the db's contribution equal, and the M80 line keeps < 0.30 s; 5656-passed suite + 11 skipped (worktree code verified under test via the cwd-first import), ruff and yapf clean, the walk's four contract tests (late candidate file, silent absent candidates, the never-listed deep dirs, symlinked entries) pinning the changed memo's behavior | the collect's corpus walk re-built its per-kind (kind, container, name) tuple per session (~1k re-joins of two constants per collect) and os.path.join'ed every candidate path through posixpath's case analysis (~16.5k candidate joins + 2.5k container joins, ~24 ms of profile time, the walk's largest Python slice after the stat syscalls) although os.scandir's entry.path is absolute and never ends in the separator, so the path is entry.path + os.sep + a relative constant; the directory-listing memo now stores subdirectory paths only — its sole consumer descends directories and stats candidates one level down, an entry's dir-ness changes only through a parent rename the stat pair catches, and the per-round iteration over ~21.5k (path, is_dir, is_symlink) tuples with two DirEntry probes each became ~13.2k plain-string entries |
| 2026-09-17 | this PR | M54 stream-draft paint work, three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back at load 1.10-1.73 one-minute: paint-work median 0.122/0.118/0.131 → 0.086/0.086/0.095 s, −27 % to −30 %, maxima 0.173-0.178 → 0.105-0.127 s, −15 % to −41 %, every paired round faster, final-frame parity true all six arms; component attribution, wrapped-stage probes over one replay of the same corpus (11.4 KB draft, 12 paints): wrapWideChars 44.0 ms of the 184.1 ms replay pre-fix — the per-character 291-range binary search re-running over every re-emitted block on every paint, with cachedHighlight's 12 highlightAuto runs (115.6 ms) and the incremental tail re-lex making up the rest; no-regression witnesses interleaved ×3: M33 replay wall median 0.040/0.037/0.040 → 0.038/0.037/0.038 s, every paired round equal or faster, parity true all arms; 5656-passed suite + 11 skipped plus the 57-case frontend sweep, ruff and yapf clean (no Python files touched), plus 3 new wc2ch tests (the U+1100 probe floor's both sides, the memo's byte-identity and served-hit identity, the cap's eviction); healthy range unchanged | the wrap is a pure function of the input string, so a bounded LRU now serves every re-render of a completed block's identical bytes (the flush's marker swap re-uses the settled block's entry), and a text segment carrying no character at or above U+1100 — the lowest W/F range's floor — skips the per-character scan whole (the probe is conservative: a non-W/F char at or above the floor still scans, and astral chars arrive as surrogates the scan already consumes); the tool-preview mounts stay on the direct wrap because their re-renders carry new truncated bodies that would only evict block entries |
| 2026-09-17 | this PR | M105 binary-file transport serve, introduced with this PR: png serve median 34.67/33.05/34.29/33.94 → 4.82/4.94/5.07/5.13 ms (−85 % to −87 %), maxima 34.72-41.85 → 6.26-6.65 ms, every paired round faster; pptx 24.91/23.54/23.66/22.92 → 3.19/3.69/3.65/3.44 ms (−84 % to −87 %), maxima 24.68-27.03 → 4.03-5.26 ms; the transport header flips gzip → identity with wire == raw bytes on both corpora (the before arm's deflate bought 0.03 % wire on the png, 1.2 % on the pptx); html witness unchanged 119.58/121.26/118.87/120.68 → 120.40/124.70/119.56/121.74 ms medians (overlapping bands) with wire 3136064 B byte-identical across all arms and transport gzip kept (three interleaved rounds plus a solo opening reading of the verbatim collector — main checkout before vs branch worktree after back-to-back, the worst on-disk artifact corpora of the live sessions tree (1,344,367 B png of d4fd4549, 968,921 B pptx of e4074308, 4,111,380 B html of e4074308), no-credential raw-ASGI drives of the real app stack, live files read-only, load 1.32-1.46 one-minute); component attribution: the removed slice is the middleware's inline per-chunk deflate of the FileResponse stream — the identity arm's 4.8-5.1 ms is the read+send the serve keeps; no-regression witness interleaved: M56 /status 0.50 → 0.50 ms with wire 1386 B and parsed digest d849e6400063 identical across arms; 5656-passed suite (5654 + 2 new middleware-contract tests) + 11 skipped, ruff and yapf clean; M105 definition, collector, healthy ranges, and history row introduced with this PR | the file server's plain FileResponse arm deflated every gzip-accepting answer per chunk on the event loop although the body's format is already entropy-coded — the browser's image loads and deck downloads paid ~8-31 ms of serve CPU per view to shrink the wire 0.03-1.2 % (and incompressible bodies only grow); the middleware's exclusion is now the skip list's prefix check, text/event-stream stays excluded (the SSE cadence starlette's one-entry constant protected lives in the list now), and text formats keep compressing |
| 2026-09-17 | this PR | M102 artifact wrap wall median 0.230/0.227/0.220 → 0.064/0.066/0.065 s, −70 % to −72 %, maxima 0.238-0.241 → 0.067-0.071 s, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, scratch fragment/output per round, load 1.83-1.94 one-minute); component attribution, fresh-process subprocess medians over 3: `import src.cli.artifact` 0.062-0.066 s wall alone, +`get_config()` 0.206-0.217 s — the config model stack is ~150 ms of the wrap wall, the M98 attribution's 180 ms cumulative chain read against this chain's lighter import floor; no-regression witnesses interleaved ×2 on the branch: M92 schedule-trigger --help 0.040/0.041 s, M98 memory query 0.055/0.056 s, M97 plan list 0.217/0.233 s, M99 import server 0.574/0.581 s (standing main readings the same hour 0.040/0.055/0.215/0.558 s); 5654-passed suite + 11 skipped, ruff and yapf clean, plus 1 new contract test (a fresh-process wrap verb leaves src.core.config, src.core.models, and pydantic unloaded) | the wrap verb's only config read is the profile home for the vendored-KaTeX path — `Field(default_factory=charliebot_home_dir)` and a yaml charliebot_home key is a hard error, so the env derivation (`charliebot_home_dir()`, the M98 owner module imported directly) is the same value without the config stack; the check verb keeps `get_config` (the probe resolves backends from it), and the wrap tests' seam moved with the verb (`cli_katex` patches the module-level `charliebot_home_dir` name, the M98 seam shape); M102 healthy range unchanged |
| 2026-09-17 | this PR | M98 memory-CLI invocation wall median 0.211/0.207/0.209 → 0.053/0.052/0.052 s, −75 %, maxima 0.218-0.211 → 0.056-0.053 s, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, live store read-only, load 1.56-1.60 one-minute); component attribution: `-X importtime` puts src.core.config at 180 ms cumulative of the 221 ms fresh-process wall (pydantic 52 ms + backend_models 34 ms + yaml 14 ms), and the after arm's `import src.cli.memory` measures 20 ms with none of the heavy chains loaded; no-regression witnesses interleaved ×2: M92 schedule-trigger --help 0.039/0.041 → 0.039/0.041 s medians (standing band), M97 plan list 0.215/0.223 → 0.212/0.214 s, M99 import server 0.529/0.568 → 0.526/0.562 s, M102 artifact wrap 0.221/0.226 → 0.225/0.225 s; 5653-passed suite + 11 skipped, ruff and yapf clean, plus the memory chain's import-weight ban set extended with src.core.config + src.core.models + pydantic (the read verbs no longer bind them at import) | the query/add/lint verbs read no config file — the store root is `charliebot_home_dir() / "memory"`, a pure derivation of the env-resolved home that no config key can move (a yaml charliebot_home key is a hard error), so a fresh invocation no longer pays the config model stack to parse a file the verb never reads; the home-resolution block moved verbatim to src/core/home.py (config re-exports the names for its existing import path; the _home_cache reset/patch sites in conftest and the hardening test re-pointed to the owner module; the hardcoded-path guard's exemption follows the owner), and the memory CLI's test seam patched get_config → charliebot_home_dir; a broken config.yaml no longer blocks the store's own verbs — the store is independent infrastructure whose root the env alone determines |
| 2026-09-17 | this PR | M68 marked changed-poll rebuild median 1.70/1.83/1.72 → 1.23/1.24/1.27 ms, −26 % to −32 %, maxima 1.90-2.35 → 1.30-1.65 ms, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, 2628 KB / 299-row worst thread-metadata corpus of session dfe393f7, scratch copy, live state read-only, decoded 95083 B wire 12757 B with digest c3c19bb391cf identical across all six arms, load 0.70-0.81 one-minute); no-regression witnesses interleaved ×2: M36 full poll 0.63-0.66 → 0.61-0.64 ms and conditional 0.57-0.59 → 0.58-0.60 ms (204, 0 B) with digest c3c19bb391cf identical — the unchanged-poll paths serve the body memo and never re-render; M63 /view handler 0.51-0.53 → 0.51 ms with body 116859 B identical (the view rows share the row memo's dicts); 5653-passed suite + 11 skipped, ruff and yapf clean, plus 1 new byte-parity test (the joined fragments equal the whole-array dump they replaced across nested/unicode/None/float row shapes); M68 healthy range recalibrated < 0.003 s → < 0.002 s with this PR | the marked rebuild re-rendered only the moved row but `_list_body` still re-dumped all 299 rows through stdlib json — 0.60 ms of the ~1.7 ms rebuild measured standalone on the 339-row corpus against 0.01 ms of fragment join; the row memo now carries each row's rendered JSON fragment beside its dict (the M72 files-listing row-memo shape) and the body assembles by sorting the (row, fragment) pairs and joining fragments — the encoder's per-element text is context-free, so the join is byte-identical (pinned by the new test and by the collector's digest across all six arms); trigger rows ride the same per-request fragment render |
| 2026-09-17 | this PR | M99 server import floor, `import server` (fresh process) median 0.557/0.556/0.564 → 0.541/0.543/0.542 s, −13 to −22 ms (−2.3 % to −3.9 %), maxima 0.567-0.591 → 0.549-0.563 s, every paired round and every paired max faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, five fresh processes per arm per round, load 1.09-1.17 one-minute, a sibling cron run's collectors live on the host; the pre-suite trio at load 1.1-1.7 read the same direction, −3 to −36 ms, and is excluded from the paired claim); component attribution, fresh-process subprocess medians over 9: fastapi-only 246.4 ms vs fastapi + jinja2 + fastapi.templating 265.7 ms — the template engine's marginal import cost is 19.2 ms; no-regression witness: the 5652-passed suite + 11 skipped on the branch plus the page-render suites (test_pages, test_home_page, test_perfetto_pages, test_ncu_page, test_versioned_static_cache); the import-weight contract's server ban set now pins jinja2 + fastapi.templating | the page template engine loaded at pages.py's module scope although no import-time path renders a template — jinja2 + fastapi.templating (~19.2 ms marginal over the already-loaded fastapi) now build on first render behind a memoized getter, the #1647 plan_diff deferral shape; M99 healthy range unchanged |
| 2026-09-17 | this PR | M7 restart-cold seeded key scan 1.166/1.161/1.163 → 0.911/0.917/0.941 s medians (−21 % to −22 %), maxima 1.188-1.196 → 1.166-1.187 s, every paired round faster (three interleaved rounds of the A/B harness — main checkout before vs branch worktree after back-to-back, identical scratch state per round: the live cache document copied with its opencode entry's signature deterministically staled (the rows-unchanged-since-the-document production state), the live rows sidecar copied beside it, the 23.4 GB live db read in place mode=ro by the collect itself, live home never written, load 1.76-1.78 one-minute; rows digest a1edab0efea3 identical across all six arms, scanned 0.0 MB — one mid-measurement round absorbed a live 474806 B row move with the digest unmoved); the after arm's one-time warm-up collect plants the entry's probe field (the document-shape upgrade the first post-deploy restart pays, the M7 definition's own class) and the timed rounds measure the recurring shape; no-regression witnesses on the branch: M7 changed-round collect 0.127 s median (main standing 0.205 s the same hour) and M80 churn 0.1176 s with rows digest 43882dbf0866 identical to the main standing reading; 5652-passed suite + 11 skipped, ruff and yapf clean, plus 3 new gate tests (the seeded probe match skips the key scan outright, a proof miss sends the restart down the diff fetching only the moved blob, a legacy probe-less entry seeds and scans the old contract and the round's store writes the proof back) and the pinned entry-shape test gaining the probe key | the seeded row memo never took the warm memo's proof-aggregate gate (`probe = None if seeded`, the 2026-09-16 row's documented gap): every signature-stale restart re-read all 176,685 live (id, time_updated) keys (~0.19 s) and ran the per-id diff although the stored (count, sum) pair proves the rows unchanged — the entry now carries that pair beside the rows and the seeded memo takes the same gate, the same weaker-proof trade the warm path's comment documents (a multi-row coincidence netting to zero dodges it; the next real move re-scans); the sidecar format and the matched-signature serve are untouched (the probe rides the main document's entry, absent on pre-deploy documents and the legacy contract the new test pins) |
| 2026-09-17 | this PR | M71 capped search served median 3.13/2.99/3.08 → 1.89/1.66/1.74 ms, −40 % to −46 %, maxima 4.15-4.31 → 2.52-2.93 ms, every paired round faster (three interleaved rounds of the repaired collector — main checkout before vs branch worktree after back-to-back, shared snapshot of 1235 metas + 170.5 MB active live chat files + triggers dirs, 200 rows, decoded 205960 B / wire 32030 B with parsed digest ad2b74df9d9a identical across all six arms, load 0.81-0.87 one-minute); component attribution: the removed per-request slice is the middleware's level-1 deflate of the 205960 B body — 1.36 ms measured standalone (bare-app raw-ASGI repeat 2.05 ms vs middleware-mounted 3.41 ms over the same snapshot at load ~1.0) — replaced by the body-keyed memo read; no-regression witnesses interleaved: M56 /status 0.56 → 0.51 ms with digest deb187c4071e identical and M8 absent-needle manager 0.98 → 1.04 ms (within noise); 5647-passed suite + 11 skipped, ruff and yapf clean, plus 3 new gzip-contract tests (precompressed serve with decompressed parity, repeat zero re-compress, plain request no memo entry) and the route's four direct test calls moved to the _page_request seam; M71 healthy range recalibrated < 0.006 s → < 0.003 s with this PR | the standing collector rode the TestClient harness (~1.5-2 ms) and skipped the gzip middleware whose deflate the browser's search fetch always pays — the vacuous-read class the M36/M56/M59/M70/M72 repairs called out; the repaired raw-ASGI drive (the M35/M72 pattern) reads the served path the middleware and route actually run, and the search response's gzip form now rides the single-homed body-keyed memo (the #1698 serve rule), one off-loop level-1 deflate per distinct body replacing the middleware's per-request pass, Content-Encoding set upstream making the middleware skip |
| 2026-09-16 | this PR | M7 restart-cold standing reading classified as shape drift, not a product regression and not corpus growth: wall 0.961-1.111 s, 21 rows, scanned 0.0-1.1 MB over seven fresh-process runs (verbatim collector, live cache document copied per run, load 1.4-3.1 one-minute across the readings) against the < 0.5 s line the 2026-09-15 landing set on the matched-signature shape (readings 0.29-0.31 s, the landing's pinned contract: db file+WAL signature unchanged since the document was written, the stored partial serves with the sidecar and db both unread); the standing collector's copy of the live document is signature-stale whenever an opencode turn ran since the server's last token-usage collect — instrumented proof: a fresh-process collect reads the sidecar once and runs the per-id key scan once, and an immediate re-collect of a just-synced document (fresh-process semantics, in-process memos cleared) still reads the sidecar and runs the key scan (1.118 s, scanned 0.0 MB), because the db's file+WAL signature moves with every streamed turn's writes and the seeded cold memo never takes the probe gate (`probe = None if seeded`, the scan function's own comment); no-regression proof: interleaved fresh-process A/B against the pre-#1676 checkout 9f6e8526, back-to-back rounds main 1.092/1.022/0.961 vs pre 1.089/0.982/0.952 s at load 1.4-2.0 one-minute — every paired round within 4 %, so neither #1676's parse carry nor #1690's cache-store single-homing moved the restart path; corpus check: document 6.1 MB + sidecar 25.6 MB / 175,227 rows today vs 5.8 MB + 24.8 MB / ~170k rows at the landing — ~4 % growth, and the component floor (cProfile, fresh-process shape) was the same shape at the landing corpus: sidecar orjson parse 0.21 s, the seeded per-id key diff 0.55 s (~3.1 µs/row), the memoized sessions walk 0.17 s, the sidecar rewrite 0.08 s; healthy range recalibrated to median < max(0.5 s, (document + sidecar bytes) ÷ 25 MB/s) with this PR — the churned reading sits 1.15-1.35x inside (31.7 MB ÷ 25 MB/s = 1.27 s) while the matched-signature quiet-db shape keeps the landing's 0.29-0.31 s contract under the 0.5 s floor; the collector now prints the document+sidecar bytes the formula reads | the two restart shapes ride one db file+WAL signature the opencode db's own writes move; the follow-up code lever, once the M7 topic's one-day skip window from #1676 passes: persist the post-scan probe beside the rows map so the seeded restart can take the same aggregate-gate skip the warm memo takes — a churned restart whose rows themselves did not move (WAL-only noise, the steady state the warm gate exists for) would drop the 175k-row key diff to a probe read, the weaker-proof trade the warm path's docstring already documents |
| 2026-09-16 | this PR | M104 per-line tail-follow cursor checkpoint 956.6/959.0/965.1 → 7.2/7.2/7.3 µs (−99.3 %, ~134x), maxima 963.7-968.8 → 7.2-8.0 µs, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, scripted 2000-line scratch stream with a real cursor file under /tmp, live home untouched, cursor offset 210890 B byte-identical across all six arms, load 0.69-0.97 one-minute); real-corpus witness on the branch: the M84 corpus (1051.1 MB / 186-line worst raw master-run log, scratch copy, live home read-only) replayed with the cursor ON records offset 1051067581 = file size exactly, wall median 3410.8 ms — inside the cursor-off band the same hour (interleaved M84 rounds: main 3500.3/3460.3, branch 3505.2/3519.6 ms, parity divergences 0 both arms); component attribution: the per-line open(O_TRUNC)+write+close cycle measured standalone 933.8 µs on this host's storage (a held-fd pwrite of the same payload 1.5 µs, pwrite+ftruncate 10.9 µs); 5644-passed suite + 11 skipped, ruff and yapf clean; M104 definition, collector, healthy range, and history row introduced with this PR | the loop checkpointed the consumed byte offset once per streamed line through a full open+truncate+write+close cycle — ~0.9 ms of synchronous event-loop time per line riding every live master/worker turn (a 2000-line turn ≈ 1.9 s of cumulative loop stall) — invisible to M84, whose replay passes cursor=None; the mount now holds one fd and rewrites a fixed-width zero-padded decimal in place, so a read observes the full old or full new value and the read_raw_cursor replay contract keeps at-most-duplicates-never-loss |
| 2026-09-16 | this PR | M17 fork median 1.1600/1.1633/1.1698 s → 0.4253/0.4794/0.5031 s, −57 % to −63 %, maxima 1.2172-1.2305 → 0.5162-0.5787 s, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, the 1051.3 MB / 307-event heaviest fork corpus of session 489e7c31, scratch CHARLIEBOT_HOME, live home read once per arm for the copy, load 0.88-0.95 one-minute; parent_reference.jsonl sha256 digest fcb5de4df91f identical across all six arms); component attribution on the before wall (cProfile): source read_bytes 0.555 s (the whole-corpus memcpy), isascii 0.085 s, numpy newline scan 0.118 s, window write 0.237 s; 5637-passed suite plus a new chunk-boundary test (a 1600-line / >3-chunk corpus forks byte-identically), ruff clean | the full-corpus fork read each source file into one Python bytes object before streaming it — a whole-corpus memcpy that dominated the fork of the gigabyte-class live files — although the fast frame path only reads the mapping through a uint8 view and one window write; the source now rides an mmap (the corpus never enters the Python heap: the scan's memory bandwidth and the kernel's window copy replace the read's memcpy), with the non-ASCII and undecodable-byte error contracts raising identically through a materialized fallback and the per-frame path's memoryviews released before the mapping closes (BufferError-safe teardown); chat files are append-only between atomic os.replace rewrites (the ChatEventStore's stated rule), so the mapping holds an append-only or already-unlinked inode and never truncates under it |
| 2026-09-16 | this PR | M59 full-row median 1.35/1.37/1.36 → 0.54/0.57/0.52 ms, −60 % to −62 %, maxima 1.60-1.93 → 0.86-1.04 ms, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, the 99.9 KB / 50206 B-decoded worst thread-metadata row, live state read-only, parsed digest 7184f3458354 identical across all six arms, wire 22140 B, load 1.70-1.73 one-minute); attach mode unchanged 0.43-0.45 ms both arms, body 48 B; no-regression witness interleaved ×2: M36 list poll full 0.61-0.71 → 0.64-0.68 ms and conditional 0.58-0.72 → 0.59-0.66 ms (204, 0 B) with parsed digest 8946dac083ec identical — the refactored response half is byte-identical; 5634-passed suite + 11 skipped, ruff and yapf clean, plus 5 new detail-gzip contract tests (precompressed serve with decompressed parity, repeat zero re-compress, changed-body recompress, plain request no memo entry, attach mode stays slim and uncompressed); M59 full-row healthy range recalibrated < 0.003 s → < 0.001 s with this PR | the served full row still paid the gzip middleware's whole-body level-1 deflate on every request — the ~0.85 ms slice the 2026-09-15 collector-repair row attributed (raw-ASGI+gzip 1.27 ms vs bare 0.42 ms on the 50206 B row) — although the rendered bytes are their own invalidation ground; the gzip form now rides the body-keyed memo beside the plain render (a memo hit proves byte equality because the dict key IS the body; the M36 mechanism), one off-loop level-1 deflate per distinct body replaces the middleware's per-request pass, and Content-Encoding set upstream makes the middleware skip (the M72 mechanism); the serve half is single-homed into `gzip_body_response` beside the list poll's |
| 2026-09-16 | this PR | M72 changed-round collector repaired: it crashed on every timed round since the 0c8fb854 rename landed — `AttributeError: module 'src.api.files' has no attribute '_dir_listing_html'` (the #1686 refactor dropped the [0]-view wrapper and renamed the entry to `_dir_listing_page`; the harness's two call sites read the wrapper name) — so the changed-round sub-metric stood unmeasured while the standing repeat-view collector kept reading 5.25 ms (inside its line); the repaired harness calls the renamed entry (the page element is all the harness reads): changed-round rebuild median 5.53/5.89 ms, maxima 6.62/7.14 ms over 9 (two rounds, 1234 entries, live sessions root read-only, load 2.45-2.49 one-minute) — inside the unchanged < 0.007 s line; no-regression witness: the standing served-path repeat collector 4.53 ms median (standing 5.25 ms the same hour, both inside < 0.008 s) | collector command only, no product code; the changed-round half's regression watch (a page-key miss pricing walk + sort + join with the row memo warm) is live again |
| 2026-09-16 | this PR | M81 page re-render standing reading classified as corpus growth, not a product regression: 29.98 ms, 2 walks, parity true, then 32.65 ms at the round's higher load (verbatim collector, 7 bodies — 5 math-free, 7.5 KB, corpus sha1 e14932d4b7ca — of the 1051.3 MB runaway-turn capture that is now the worst live chat file, live home read-only, load 2.0-3.1 one-minute) against the < 0.020 s line the 2026-09-09 landing set on the 36.3 MB corpus's 40 math-free bodies (33.13/33.64/31.08 → 0.87/1.20/0.95 ms, 40 → 0 walks); the gate works as landed — the 5 math-free bodies skip and the streamed math-free arm stays 0 walks / 0.00 ms inside its unchanged < 0.010 s line, while the 2 delimiter-bearing bodies' walks are the page's own math rendering; healthy range recalibrated to < 0.020 s + 0.020 s per delimiter-bearing body with this PR — the reading sits 1.8-2.0x inside | the worst-corpus move is this week's 489e7c31 runaway-turn capture, the corpus the M78/M84 rows re-based on; no code change renders the walk materially cheaper — katex.render is the per-formula floor (probe over the same corpus: 21.99 + 15.50 ms for the two bodies' 3+4 formulas), and the round priced the memo alternatives against the standing collectors' own contracts: the walked HTML's jsdom innerHTML re-parse costs 23.2-23.8 ms per body — no cheaper than the walk — and baking katex into the parse memo breaks M60's settled-bytes-equal-direct-parse parity, so the line tracks the page's own math and the walked count (2) stays the gate's regression watch |
| 2026-09-16 | this PR | M101 first-view standing reading classified as corpus growth, not a product regression: first view 4030 ms, then 3930 ms at the round's higher load (verbatim collector, 1051.3 MB / 307-event worst live chat file of session 489e7c31, scratch home, live home read-only, load 2.5-3.1 one-minute) against the < 1.0 s line the 2026-09-14 landing set on the 36.3 MB / 5519-event corpus (first views 741-811 ms); the unchanged sub-metrics stay inside their lines — loop-lag median 5.38-5.44 ms (< 0.010 s), steady-state wall median 0.6-0.7 ms (< 0.10 s), 99.2 MB gzip wire; healthy range recalibrated to first-view wall < max(1.0 s, bytes ÷ 200 MB/s) with this PR — the reading sits 1.3x inside | the first view is the stat-keyed memo's one executor hop — read + level-1 gzip of the whole corpus — whose cost scales with bytes; measured 261-267 MB/s end-to-end on this corpus, so the line tracks the compress floor the same way the M84 tail-follow line tracks the parse floor; the corpus is the runaway-turn capture, and a corpus reversion re-tightens the line automatically |
| 2026-09-16 | this PR | M55 compare-view repeat, repaired collector + served gzip memo: standing TestClient reading 0.0101 s median (the round-opening trip, decoded body 4017223 B); interleaved rounds main 0.1073/0.1060/0.1061 → branch 0.0013/0.0011/0.0010 s medians, −98 % to −99 %, maxima 0.1078-0.1111 → 0.0013-0.0014 s, every paired round faster (three interleaved rounds of the repaired collector — main checkout before vs branch worktree after back-to-back, shared scratch snapshot of the 6.0 MB worst artifact pair presentation_a2a_deadlock_gallery.html vs test_s4_eval.html of session 5446ddf7, live state read-only, gzip wire 3058673 B identical across all six arms, decoded-body sha16 bf085e6e87e48cab identical across arms, load 3.38-3.51 one-minute; before-arm wire digests move per round — the middleware's dated gzip header — the after arm's mtime=0 form stable at d27d4dff33e9); loop-lag medians 0.0055-0.0061 s both arms at the 5 ms ticker floor, inside the unchanged < 0.010 s line; first view unchanged 0.2214-0.2300 → 0.2233-0.2318 s (the cold annotate dominates both arms; the one-time route compress replaces the middleware's first-view deflate), inside the unchanged < 0.25 s line; component attribution: the removed per-click slice is the middleware's level-1 deflate of the 4017223 B annotated page — the before arm's raw-ASGI drive pays ~106 ms per repeat where the after arm's precompressed serve reads 1.0-1.3 ms, matching the M70 landing's measured ~27 ms per MB of level-1 deflate; the old standing 0.0101 s reading additionally sat on the TestClient/httpx harness the repair drops (the ~9 ms-of-harness class the M70 repair measured on the 0.8 MB body, larger on this 4 MB one); no-regression witnesses on the branch: M70 clean-view repeat 0.0006 s median with wire digest 237de31d3f6f identical to the standing reading, M72 served-path listing repeat 5.34 ms (standing 4.81 ms, both inside the < 0.008 s line at the round's higher load 3.4-3.5); 37-passed route test file (32 + 5 new tests), ruff and yapf clean; M55 repeat-view healthy range recalibrated < 0.010 s → < 0.003 s with this PR | the compare view served its memoized annotate body plain, so the browser's gzip-accepting click paid the middleware's whole-body level-1 deflate on every request — ~105 ms of the 107 ms served repeat on the 4 MB worst pair — although the published annotate is immutable and already memoized; the gzip form rides the same annotate memo key (a memo hit proves byte equality because the key IS the two files' signatures plus the injection flag), one off-loop deflate per distinct body replaces the middleware's per-request pass, and Content-Encoding set upstream makes the middleware skip (the M70 mechanism); the standing collector also rode the TestClient harness and never mounted the middleware — the vacuous-read class the M68/M89/M90/M70/M72 repairs called out — and had lost the loop-lag leg its definition promised, so the repair mounts the production gzip middleware, drives raw-ASGI with the browser's Accept-Encoding shape, and restores the ticker |
| 2026-09-16 | this PR | M78 chat-file leg 5050-5107 → 3634-3734 ms median (−26 % to −29 %), maxima 5090-5148 → 3644-3917 ms, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, the 1051.3 MB / 307-event worst on-disk live chat file of archived session 489e7c31, live home read-only, load 2.53-4.40 one-minute; event counts 307 identical across all six arms); worker-log leg 52.3-54.5 → 38.9-42.3 ms (−23 % to −28 %, counts 232 identical); component attribution: raw orjson.loads over pre-split lines 2.746 s on the same corpus — the 383 MB/s parse floor — vs 282-290 MB/s through the landed funnel; no-regression witnesses re-measured on the branch at load 4.1-4.6: M6 append-round 0.07 ms parity True, M26 advance 0.19 ms parity True digest e94c56635194 identical, M30 steady 0.0002 / append-round 0.0004 s, M20 repeat 0.0000 s / cold per-divider 0.9 ms, M23 steady 0.0004 s, M28 tail+count 0.01 ms, M37 tail 0.01 ms, M77 re-entry 0.09 ms with 0/36 rebuilt, M101 steady 0.8 ms / loop-lag 5.47 ms, M17 fork 1.3257 s (the 1 GB copy dominates, unchanged), M35 events/view/bootstrap medians 1.27/2.13/1.31 ms with wire bytes and decoded digests identical (2b451eddaa4c / 52e7ee739e38 / 6622b6f73143 — the warm pages never parse; the medians ride the load the M56 history documents); 5614-passed suite (5608 + 6 new tests, 11 skipped), ruff and yapf clean; healthy range recalibrated chat file median < max(0.080 s, bytes ÷ 250 MB/s) with this PR | the whole-file funnel decoded every line to str through text-mode iteration and re-encoded it inside orjson before the parse — 128,331 incremental decode calls plus one full re-encode per line on the 1 GB corpus, ~1.7 s of the 5.1 s reading; the parse now maps the file once and hands orjson memoryview slices (zero-copy, the parse_ndjson_line memoryview contract), the line domain unifies on the \n domain the count and tail readers already counted (the text-mode universal-newline split was the one reader that could disagree), and the archive whole-read and parse_ndjson_range share the same funnel; the reading stays corpus-bound — the line now tracks bytes ÷ 250 MB/s like M84's, with the raw parse floor at 383 MB/s |
| 2026-09-16 | this PR | M7 standing reading's max 102.987 s (one of five /token-usage requests, median 0.133 s inside the line) diagnosed and fixed: the parse the changed round runs on a grown master-run raw log — `token_tally._parse_lines`, the function both the full parse and the append-tail round read every source through — re-concatenated a bytes remainder per 4 MB chunk and mapped every marker hit with a from-zero rfind, both O(line² / chunk); the interleaved A/B on the corpus that paid it (three back-to-back rounds, main checkout before vs branch worktree after, 1051.1 MB / 186-line raw master-run capture of session 489e7c31's finished runaway turn, live home read-only, load 1.77-3.32 one-minute): full parse 97.7/97.0/98.3 → 1.4/1.4/1.4 s median (−98.6 %), tail round from a recorded end near the file head 97.3/96.2/98.6 → 1.4/1.4/1.3 s (−98.6 %), maxima 96.4-101.9 → 1.4 s, every paired round faster, objects (61) and consumed offset (1051067581) identical across all twelve arms; no-regression witnesses interleaved ×2: M7 changed-round 0.134/0.136 → 0.143/0.134 s, restart-cold 1.125/1.264 → 1.132/1.081 s, M80 churn 0.1197/0.1240 → 0.1281/0.1232 s (rows digests move with live traffic only), M78 worker-log leg 52.7/50.5 → 41.2/39.8 ms (the dense small-line path unchanged to slightly faster); 300-trial randomized parity fuzz against the old implementation (marker/no-marker/empty/unparseable lines, trailing fragments, multi-chunk giant lines) 0 mismatches plus the 1 GB corpus byte-identical; 5601-passed suite (5599 + 2 new tests, the pre-existing `stream_incremental_parse.test.js` red on main deselected) | the 12:43 hourly load recorded a ~14 MB end for the then-streaming capture; by 13:49 the runaway turn had appended a gigabyte of multi-hundred-MB no-marker observation lines, so the changed round's tail read paid the quadratic on every chunk — the same shape the M84 landing (2026-09-11) fixed in the raw-log funnel, in the tally's own splitter; the carry now holds exactly the current unterminated line, compacted once per round with the newline scans riding the fresh region and the marker pass covering the carried partial only once the line completes |
| 2026-09-16 | this PR | M84 standing reading classified as corpus growth, not a product regression: tail-follow replay median 3455.5 ms, max 3650.2 ms; stdout-stream replay median 2874.2 ms, max 3026.9 ms (150/150 events, parity divergences 0) over 7 (verbatim collector, 1050.9 MB / 150-line worst on-disk raw master-run log — session 489e7c31's live charlie-code-gemini-3.8-flash turn, still streaming at measurement time, scratch copy, live home read-only, load 1.89-2.59 one-minute) against lines < 0.060/0.040 s calibrated on the 10.1 MB / 64-line corpus the 2026-09-11/09-13 landings measured (14.1/13.0 ms); the replay's cost is the per-byte orjson+translate floor — measured throughput 304 MB/s tail-follow / 366 MB/s stdout-stream on this corpus vs 706/777 MB/s on the 10.1 MB single-giant-line corpus — so no code change parses the corpus materially cheaper; the same collector on the prior 16.3 MB / 638-line corpus read 36.4/27.0 ms the same hour (inside the old lines), pairing the corpus move with the reading on one code state; healthy ranges recalibrated to max(0.060 s, bytes ÷ 200 MB/s) tail-follow and max(0.040 s, bytes ÷ 250 MB/s) stdout-stream — the 1 GB reading sits 1.5x inside both, the 16.3 MB reading keeps 2.2x/2.4x, and a corpus reversion re-tightens the line automatically | the collector's worst-log selector is honest — the funnel streams exactly this log live, line by line, the per-line cost M91 prices at its floor — and the whole-file replay shape is the measurement's, not a production pass; the runaway turn itself is a host-state finding, reported in the round summary, not a perf topic |
| 2026-09-16 | this PR | M97 plan-CLI command wall median 0.309/0.308/0.318 → 0.232/0.247/0.231 s, −77 to −86 ms (−25 % to −27 %), maxima 0.334-0.351 → 0.247-0.267 s, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, 15.4 KB worst plans corpus of session a9bb2346, the GET read-only against the live server through a scratch CHARLIEBOT_HOME holding a synthetic valid config plus a credentials copy, because the live config corpus turned invalid mid-round — the host-state finding this round's summary records; the same scratch home served both arms, so the pair isolates the code, load 4.35-4.63 one-minute); the standing same-day reading before the config broke: median 0.370 s, max 0.529 s over 7 at load 3.83; component attribution, fresh-process subprocess medians over 9: `import src.cli.plan` 82.5 → 36.6 ms (the asyncio chain, 37.9 ms cum in `-X importtime` via plans.py's module-scope import and its locks import, now loads inside the async registry methods and _lock_for, and the import-weight contract's plan ban set pins asyncio), and the request-time client: requests' lazy import (145.9 ms standalone, ~60 ms marginal inside the verb) replaced by a phase-separated http.client client whose own imports (http.client+ssl+urllib.parse, 46.6 ms standalone) load inside _send_request only; `import src.cli.common` parity 38.8 → 36.1 ms — the M92 floor untouched; no-regression witnesses: M92 schedule-trigger --help 0.047/0.054 → 0.045/0.046 s medians interleaved ×2 (standing band), and the M98 chain structurally unchanged (post-import sys.modules identical both arms — requests False, src.core.plans False — while its fresh-process reading is unrunnable this round because the same broken live config exits every fresh-process CLI against the live home); 5575-passed suite + 11 skipped, ruff and yapf clean, plus a real-socket POST wire test pinning the request shape the new client owns (Content-Type, body bytes, query string); M97 healthy range unchanged (the after medians sit at 0.23-0.25 s against the 0.40 s line) | the plan verb wall still carried two dead-weight slices after the 09-14 landing named the requests import "the remaining floor of the verb walls": plans.py's module-scope asyncio (its async registry methods are server-side; the sync CLI read path never reaches them) and the requests client's ~60 ms import paid by every verb's single internal-API call; the replacement client phase-separates connect from post-send failures at the source — http.client raises per phase where requests folds both into one ConnectionError class the old code disambiguated by message-sniffing — so the restart-crossing contract's retry-safe/outcome-unknown split reads the phase instead of the message text, and the rejection, bounded-retry, and readback semantics carry over verbatim under the rewritten contract tests plus the real-socket stub listener |
| 2026-09-15 | this PR | M66 standing corpus re-qualified: the size-ranked probe had picked a 373.8 MB analysis manifest (job114279_runtime_targeted_analysis.json — a JSON object of per-rank analysis entries, no traceEvents) as the worst corpus; the merged build parsed it, merged zero events, and shipped a 0.0 MB.gz artifact in 1.77 s — a vacuous reading that would have hidden any real regression on the true corpus. The repaired collector qualifies candidates largest-first through the build's own shape contract and resolves the 307.3 MB hayden profiler trace: merged build median 4.27 s, max 4.32 s over 3, artifact 21.5 MB.gz (load 3.46/3.14/2.63 one-minute) — inside the < 8 s line; first true reading since the corpus moved | the corpus test read only the first 64 bytes (starts with { or [), which every JSON body passes; the same defect sat in the M88 probe and in the served path's gate, where a traceEvents-less object reached merge_traces and silently produced an empty trace — the build now fails loud on the shape and both collectors qualify through it |
| 2026-09-15 | this PR | M88 standing corpus re-qualified to the same 307.3 MB hayden profiler trace the repaired M66 probe resolves (the manifest the old probe picked validated as parseable JSON and compressed to a 24.4 MB.gz artifact in 1.84 s — a first-view build of a file no trace view can render): direct-pass build median 2.72 s, max 2.75 s over 3, artifact 23.8 MB.gz (load 3.93/3.25/2.68 one-minute) — inside the < 3.5 s line | the direct-pass validation parse accepted any parseable JSON; the gate now applies the merge path's own shape contract, so a traceEvents-less object fails the build loudly (a clear 500, no cache entry) instead of serving a gzip the viewer cannot render |
| 2026-09-15 | this PR | M7 restart-cold, fresh-doc interleaved A/B (each arm's cold pass writes its own document shape from the live state into a scratch cache, then a fresh process re-collects against it, timed): main 534/573/646 → branch 289/288/314 ms medians, −46 % to −50 %, every corpus-stable paired round faster; the trio's round 2 pair straddled a live log append (digests 75432d24482e vs 0dc8db190ea4 differ within the pair) and a second trio's rounds 1-2 read the same within-pair drift at the elevated load 1.14-1.44 one-minute — all excluded from the paired claim; the second trio's corpus-stable round 3 pair reads main 1294 → branch 293 ms (digest d99aaa566520 both arms, −77 %); rows digest identical within every paired round claimed; component: the persisted document the collect parses dropped 30.6 → 5.8 MB — the ~170k-row opencode rows map moved to a sidecar document beside the cache (24.8 MB, one stable name per db path), its parse measured standalone 246 → 18 ms, and a zero-movement restart now serves from the stored partial without touching the sidecar or the db (new tests pin: the sidecar and db both unread on a matched-signature restart; a missing sidecar degrades to the full-scan contract with the note); the rows map itself is load-bearing — the unseeded cold scan reads 692 MB of message blobs for 2.25 s standalone, so it persists, only elsewhere; no-regression witnesses interleaved ×3: the verbatim changed-round collector main 133/136/143 → branch 138/134/134 ms medians (par, maxima 275-278 → 146-171 ms — the branch never pays the 30.6 MB dump spike) and the verbatim restart-cold collector against the live stale-format document (the deploy-skew class the 2026-09-14 row documents) main 6.81-7.11 → branch 6.54-6.85 s — the 2.3 GB stale-document re-read dominates both arms; load 1.14-1.44 one-minute across the rounds; 5500-passed suite + 11 skipped, ruff and yapf clean, plus 3 new sidecar-contract tests and the two rows-reading tests moved to the sidecar; M7 restart-cold healthy range recalibrated < 2.0 s → < 0.5 s with this PR | the rows map is the row memo's persisted seed and the document's bulk at once; its only reader is the restart seed, so the bulk now parses only when a signature miss demands a seed, and the document every changed round parses, diffs, and re-dumps carries the Claude+Codex corpus's size alone |
| 2026-09-15 | this PR | M68 marked changed-poll rebuild, repaired collector: standing TestClient reading 3.47 ms median; interleaved rounds old drive 3.50/3.44/3.47 → new drive 1.83/1.94/1.92 ms medians, −44 % to −47 %, maxima 3.81-4.28 → 2.16-2.31 ms, every paired round faster (three interleaved rounds of old TestClient drive vs new raw-ASGI drive back-to-back, same main-checkout code and worst corpus in all arms, load 1.46-1.54 one-minute, 2551 KB thread metadata over 339 rows in session 3b91d606, live state read-only; decoded body 106314 B and parsed digest 8946dac083ec identical across every arm, wire 15077 B; the probe's content-preserving rewrite makes every rebuilt body byte-identical, so all timed rounds serve the endpoint's body-keyed gzip memo form — a production changed poll whose body genuinely moves pays the endpoint's off-loop deflate instead, the 0.66 ms the M36 row measured); component attribution, same app + overrides, fresh drives at load ~1.5: TestClient repeat 3.47 ms vs raw-ASGI 1.92 ms — the httpx layer is ~1.6 ms of harness per request; M68 healthy range recalibrated < 0.005 s → < 0.003 s with this PR | the standing collector timed the harness, not the served path — the vacuous-read class the M36/M56/M57/M59/M70/M72 repairs called out; the raw-ASGI drive (the M101/M36 pattern) reads the served path the middleware and route actually run, with the production middleware mounted so the drive tracks the serve chain |
| 2026-09-15 | this PR | M94 streamed-replay dumps wall 71/74 ms medians over two runs of the verbatim collector (16.8 MB serialized, 1553 deltas, largest delta 64456 B, page body 0.24 MB, build 4.6-5.1 ms — every other sub-metric inside its line) against the < 0.060 s line; classified as corpus growth, not a product regression: the dumps wall is the collector's own stdlib json.dumps re-serialization of the emitted deltas — the same bytes the serialized sub-metric counts — and its throughput is unchanged since the landing (16.8 MB / 71 ms ≈ 237 MB/s vs the 2026-09-12 landing's 6.3 MB / 24-25 ms ≈ 250 MB/s), while the corpus's largest tool_result grew 1.11 → 13.77 MB and its event count 693 → 2314; serialized 16.8 MB sits inside its 30 MB line, so the dumps line is recalibrated to that line's own implied floor, median < 0.130 s (30 MB at the measured throughput), with this PR | the two sub-metrics are one measurement — bytes and the time to re-serialize them — and the 60 ms line sat below the serialized line's own 30 MB bound's implied cost, so a corpus growing inside the serialized line could still trip the dumps line; the pair now bound the same quantity |
| 2026-09-15 | this PR | M74 collector repaired: standing reading classified as corpus-gate drift, not a product regression — the selector took the largest on-disk raw log regardless of session family, and the worst log drifted to a charlie-code session (16.3 MB carrying one 15.0 MB observation line; component floor measured standalone over three rounds: read 2.1-9.5 ms, parse 35.7-43.4 ms, project 0.1-0.3 ms), a family the turn-end model-attribution gate (`_CLAUDE_RESUME_FLAG_BACKEND_TYPES`, the live call site's own check) never scans — the standing 0.0363 s loop-lag median, max 0.0455 s (wall 0.0385 s) priced a shape the live turn-end path cannot run; repaired selector gates the corpus to the gate's own session set (the set imported from master_cc_run, so collector and call site cannot drift) and builds the scan's translate from the corpus session's own resolved option; the worst claude-family corpus is the 9.9 MB / 391-line log of session 4fcd4c43 (largest line 1.06 MB) — the 2026-09-09 landing's calibration corpus, a smaller file than the 10.1 MB / 64-line charlie-code log the 2026-09-13 landing's row names as its own corpus (that landing's readings were already on a drifted, mis-shaped corpus): loop-lag median 0.0073/0.0072 s, maxima 0.0110/0.0103 s; wall median 0.0074/0.0073 s over two runs of the repaired collector (load 3.0-3.7 and 2.4-2.8 one-minute) — inside the unchanged < 0.030 s line; collector command only, no product code | the definition's own first sentence scopes the metric to claude-family turns ("every claude-family master turn ends with the model-attribution rescan"); without the gate the worst-corpus resolution tracks whichever backend happens to write the biggest tool output, and the line trips on a path the server does not run |
| 2026-09-15 | #1647 (row recorded in this docs-only follow-up per the #1046 precedent, the landing PR shipped without it) | M99 server import floor, `import server` (fresh process) median 0.643/0.634/0.635/0.635/0.645/0.637 → 0.618/0.612/0.626/0.599/0.597/0.596 s, −9 to −48 ms (−1.4 % to −7.5 %, mean ≈ −30 ms), every paired round faster (six interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, five fresh processes per arm per round, load 2.58-2.69 one-minute; an earlier trio at load 3.56-4.86 — a sibling worktree's full-suite pytest on the host, the cron-collision bias the M56 history documents — read two of three paired rounds faster and is excluded from the paired claim); component attribution: pages.py's module-level `_get_git_version()` ran two git subprocesses measured standalone at 25.0/25.1/25.5 ms over three interleaved pairs, and files.py's top-level `plan_diff` import read cum 7.0 ms in the server's `-X importtime` tree — after the change, pages' import carries zero git children and files' import zero plan_diff children; no-regression witness: the verbatim M103 collector against the branch (the `/` render is the path the lazy git moved onto; its cost lands on the untimed cold pass) reads GET /diff median 684 us, GET / median 1129 us, GET /api/git/repos median 502 us against the standing same-day readings 896/1199/404 us — all inside their healthy ranges; 5458-passed suite, ruff clean | the server import floor still carried two one-time init costs with no startup user: the page-render-only git version subprocesses (the same shape buildinfo already defers to its `init_build_info()` startup call) and a diff-view-only module import; both now load on first use — the version memoizes on the first render that reads it, plan_diff imports inside `_annotated_diff_page` — the #1643 deferral shape, and the < 0.75 s line keeps ~10 % headroom at the after medians (0.596-0.626 s) |
| 2026-09-15 | this PR | M102 artifact wrap wall median 0.268/0.266/0.262 → 0.243/0.246/0.239 s, −7 % to −9 %, maxima 0.273-0.295 → 0.258-0.346 s, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, scratch fragment/output per round, load 1.84-2.42 one-minute; an earlier trio at load 3.1-4.6 read 0.273/0.281/0.285 → 0.260/0.256/0.275 — six of six paired rounds faster across the two trios — and a fourth round at load 4.1-4.6, the sibling run's full-suite pytest on the host, the cron-collision bias the M56 history documents, read 0.279 → 0.300 s and is excluded from the paired claim); component attribution, fresh processes per arm: `import src.cli.artifact` 128.9 → 45.7 ms (`-X importtime`; `sys.modules` after the chain holds neither asyncio, websockets, nor pydantic in the after arm) and the chain marginal (fresh-process subprocess median over 9) 130.8 → 64.0 ms; the wrap wall's remainder is the runtime config load at the verb's get_config call (the M97/M98 shared floor, untouched); no-regression witnesses: the check verb's registry lazy import rides beside the new asyncio one (run_probe unchanged otherwise), the page-height assertion's real lazy path covered by the stub-chrome amend-gate test and conftest's render_height stub, 5449-passed suite + 11 skipped, ruff and yapf clean, plus the import-weight contract's artifact ban set now pinning asyncio, websockets, and src.core.headless_render | the wrap verb's chain still carried two module-scope imports whose only users are probe paths — asyncio for run_probe's event loop (~35 ms; pydantic_core is absent from this chain, so the import is unshared) and src.core.headless_render for the page-height assertion (~60 ms, its websockets stack); both now load at their use sites, the #1574 deferral shape, and the healthy range keeps its 0.35 s line (the after medians sit at the 09-14 landing's band) |
| 2026-09-15 | this PR | M46 cron tasks poll, repaired collector + generation-keyed body cache: served poll median 0.53/0.50/0.50 → 0.28/0.26/0.28 ms, −44 % to −49 %, maxima 0.61-0.78 → 0.35-0.44 ms, every paired round faster (three interleaved rounds of the repaired collector — main checkout before vs branch worktree after back-to-back, live cron corpus read-only, 13 task rows, wire 701 B, decoded 4153 B, parsed digest 2fd420e5a059 identical across all six arms, load 1.49-1.53 one-minute); component attribution, same app + middleware, fresh drives: bare handler 0.37 ms — the 13 model_dumps + orjson render 0.275 ms median standalone (15 rounds), the middleware's inline deflate + responder hop of the 4153 B body ~0.13 ms — both removed from a cache-hit poll, which now pays one identity check and the response construction; no-regression witnesses interleaved ×2: M56 /status 0.53/0.58 → 0.56/0.55 ms with digest 4dce7902d0f3 identical and M44 /scheduled 1.05/0.92 → 0.92/1.16 ms with digest f6b853387918 identical (the body-keyed memo's other tenants, unchanged — the cron route rides its own generation cache beside them); 5443-passed suite + 11 skipped, ruff and yapf clean, plus 4 new cache-contract tests (precompressed serve with decompressed parity, repeat zero re-render, generation change re-renders, plain request uncompressed) | the grouped sidebar render's paired fetch still paid the full body rebuild per request — 13 model_dumps + the orjson render (0.275 ms measured) plus the middleware's whole-body deflate — although the rendered bytes are a pure function of the fingerprint-cached cron snapshot, whose tasks list keeps one identity between config changes; the poll now caches (plain body, gzip body) on that identity, re-rendering and re-compressing once per generation, and Content-Encoding set upstream makes the middleware skip its own pass; the standing collector rode the TestClient harness floor (~1.5 ms) and skipped the middleware, so the repaired drive reads the served raw-ASGI path the M44/M56 repairs standardized |
| 2026-09-15 | this PR | M36 worker list poll, served gzip memo: full poll median 1.30/1.35/1.34 → 0.64/0.62/0.69 ms, −50 % to −54 %, maxima 1.57-1.72 → 1.00-1.02 ms, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, shared snapshot of the 2551 KB / 339-row worst threads corpus of session 3b91d606, decoded 106314 B wire 15077 B with parsed digest 8946dac083ec identical across all six arms, load 0.98-1.07 one-minute; the conditional 204 poll unchanged 0.58-0.60 → 0.57-0.60 ms); no-regression witnesses interleaved ×2: M63 /view handler 0.45/0.51 → 0.49/0.47 ms with body 116873 B both arms, M68 marked changed-poll rebuild 3.19/3.29 → 3.46/3.48 ms — the after arm now deflates each changed body inside the handler where the bare-app harness's missing middleware hid it, the deflate the served path pays in both shapes on a changed body; 5439-passed suite + 11 skipped, ruff and yapf clean, plus 4 new gzip-contract tests (precompressed serve with decompressed parity and the tag unchanged, repeat-poll zero re-compress, changed-body recompress, plain-request no memo entry) | the 3 s workers-panel list poll served the memoized plain body and let the gzip middleware deflate the whole 106 KB inside every served request — the same per-request cost the M44/M56 landing removed from the sidebar poll and scheduled list; the gzip form rides the body-keyed memo beside the plain one (a memo hit proves byte equality because the dict key IS the body), one off-loop level-1 deflate per distinct body replaces the middleware's per-request pass, Content-Encoding set upstream makes the middleware skip (the M72 mechanism), and the ETag conditional stays bodyless; M36 healthy range recalibrated median < 0.004 s → < 0.002 s with this PR |
| 2026-09-15 | this PR | M56 sidebar status poll, repaired collector + served gzip memo: standing TestClient reading 2.00 ms median; repaired-collector interleaved rounds main 0.63/0.64/0.67 → branch 0.49/0.48/0.50 ms, −22 % to −26 %, maxima 1.35-1.49 → 0.93-0.99 ms, every paired round faster (three interleaved rounds of the repaired collector — main checkout before vs branch worktree after back-to-back, 40 sidebar ids over the live corpus read-only, wire 1347 B, decoded 8860 B, parsed digest ee549e4d7b7f identical across all six arms, load 1.85-1.91 one-minute); component attribution, same app + overrides, fresh drives at load ~1.9: TestClient repeat 2.03 ms vs raw-ASGI bare 0.45 ms — the httpx layer is ~1.6 ms of harness per request — and raw-ASGI+gzip 0.78 ms, the 8860 B body's middleware deflate + responder hop 0.33 ms, which the body-keyed memo removes; M56 healthy range recalibrated < 0.004 s → < 0.002 s with this PR; no-regression witnesses interleaved: M46 /api/cron/tasks 1.53 ms on the branch (standing 1.56/1.53) and the M35 switch fetches events 1.15 ms / view 1.61 ms / bootstrap 1.08 ms with parsed digests 2b451eddaa4c / 7b56cc98190e / 4193328a2e6f identical (the memo's other tenants share the widened limit-16 slot set without thrash); ruff and yapf clean | the standing collector timed the harness, not the served path — the vacuous-read class the M36/M57/M59/M70/M72 repairs called out — and skipped the gzip middleware whose deflate the browser's 3 s poll always pays; the raw-ASGI drive (the M101/M72 pattern) reads the served path the middleware and route actually run, and the poll's gzip form now rides the body-keyed memo (the M35 switch-fetch mechanism), Content-Encoding set upstream making the middleware skip its pass |
| 2026-09-15 | this PR | M44 scheduled-list, repaired collector + served gzip memo: standing TestClient reading 2.14 ms median; repaired-collector interleaved rounds main 1.12/1.11/1.00 → branch 0.83/0.87/0.95 ms, −17 % to −26 %, maxima 1.43-1.52 → 1.14-1.31 ms, every paired round faster (three interleaved rounds of the repaired collector — main checkout before vs branch worktree after back-to-back, live session + cron corpus read-only, 13 scheduled rows, wire 2644 B, decoded 14577 B, parsed digest 32d4f658bd98 identical across all six arms, load 1.85-1.91 one-minute); component attribution, same app + overrides, fresh drives at load ~1.9: TestClient repeat 2.00 ms vs raw-ASGI bare 0.68 ms — the httpx layer is ~1.3-1.5 ms of harness per request — and raw-ASGI+gzip 1.33 ms, the 14577 B body's middleware deflate + responder hop 0.65 ms, which the body-keyed memo removes; the route's render moved to the /tasks route's encoder-free shape (model_dump(mode="json") feeding orjson, response_model dropped per the M59 thread-detail precedent, parsed content unchanged — digest identical across arms); M44 healthy range recalibrated < 0.004 s → < 0.002 s with this PR; ruff and yapf clean | the same harness disease as the M56 repair in the same PR, on the grouped sidebar render's paired fetch; the serve now ships the memoized gzip form, and the repaired collector keeps the live-churn guard (repeat bodies must match) the TestClient drive carried |
| 2026-09-15 | this PR | M35 switch fetches, verbatim collector: view median 2.99/2.88/2.83 → 1.54/1.61/1.37 ms, −45 % to −52 %, maxima 3.35/3.25/3.35 → 1.97/2.21/1.83 ms; bootstrap median 2.04/2.18/2.01 → 1.05/1.14/0.99 ms, −44 % to −51 %, maxima 2.38/2.22/2.05 → 1.09/1.18/1.02 ms, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, shared snapshot of the 20534-event worst projection corpus, wire 44035 B / decoded 119795 B view and wire 30178 B / decoded 77563 B bootstrap with parsed digests 7b56cc98190e / 4193328a2e6f identical across all six arms, load 0.97-1.21 one-minute); no-regression witnesses interleaved: events page 0.88/1.10/0.98 → 0.89/1.30/0.88 ms with digest 2b451eddaa4c identical (the untouched handler; round 2's median is the round's load), M63 repaired collector 0.45/0.50/0.47 → 0.47/0.44/0.51 ms medians with body 116873 B both arms (the request-seam repair keeps the row's plain-path meaning), M56 /status 1.90 ms median and M65 loop-lag 0.33 ms / wall 0.33 ms (standing bands), M96 plain-urllib sweep byte-identical median 430819 B (the no-Accept-Encoding shape never enters the memo); 5431-passed suite + 11 skipped, ruff and yapf clean, plus 4 new tests (precompressed serve with decompressed parity and the vary header, repeat-fetch zero re-compress, renamed-body recompress, plain-request no memo entry) | the two switch fetches still paid the gzip middleware's whole-body level-1 deflate on every served request — the M35 events-page landing's own row named both handlers untouched, their middleware deflate remains — although each fetch's body is a pure function of the session state it reads; the rendered body bytes are their own invalidation ground (a memo hit proves byte equality because the dict key IS the body), so one off-loop level-1 deflate per distinct body replaces the middleware's per-request pass, Content-Encoding set upstream makes that middleware skip (the M72 mechanism), and mtime=0 keeps the bytes deterministic (the M101 rule) |
| 2026-09-15 | #1623 (row recorded in this docs-only follow-up per the #1046 precedent, the landing PR #1622 shipped without it) | M75 catch-up loop-lag maxima 0.0797/0.0749/0.0804 → 0.0520/0.0396/0.0479 s, −35 % to −50 %, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, 20534-event worst live chat corpus of session d321b9ad, scratch CHARLIEBOT_HOME per round, live home read-only, load 0.77-1.04 one-minute); loop-lag medians 0.0080-0.0087 → 0.0091-0.0105 s (both at the sliced-feed ticker floor, inside the < 0.020 s line), wall medians 0.1013-0.1037 → 0.1026-0.1039 s unchanged within noise; component attribution interleaved 5×5 (fresh scratch manager per round, load 0.75 one-minute): init wall median 104.0 → 96.0 ms, worst event-loop gap 74.3 → 8.7 ms; no-regression witnesses interleaved ×2: M45 catchup replay digest e9f92b4cfe29 identical with wall 0.0292-0.0301 → 0.0293-0.0301 s, M26 advance parity True digest e94c56635194 0.14-0.21 → 0.16-0.19 ms, M6 append-round parity True 0.05 → 0.06-0.12 ms; 5423-passed suite + 11 skipped, ruff and yapf clean, plus the GC-contract test (the success and drop-rerun paths both re-enable collection) | the 2026-09-09 landing's own attribution measured the init's remaining stall — the generational GC passes its whole-corpus dict churn triggers, up to ~80 ms of event-loop pause at the session's first streamed event after a server start — and left it; the init now runs under a gc.disable boundary spanning the corpus load and the sliced feed with the finally re-enabling on every path (the trace_merge build's shape, the M88 precedent), and the drop path's rerun re-pairs under the contract test |
| 2026-09-15 | this PR | M65 collector repaired: the verbatim collector crashed on every timed round since the M35 gzip-body-cache landing — `ValueError: max() iterable argument is empty` at `max(gaps)` after the cold pass, three runs read, crash each time — because the 200-message drive through the real app stack now finishes in ~0.3-1.2 ms (the served path's own log lines: cold 173 ms, steady 0-1 ms), under the 5 ms ticker interval, so no tick ever fires; repaired collector reads loop-lag median 0.29 ms, max 0.77 ms; wall median 0.29 ms, max 0.77 ms over 9 (20534-event worst live chat corpus of session d321b9ad, 104697 B gzip wire, load 2.4-3.4 one-minute) — both far inside the healthy lines, which stay as-is: a regression back to the pre-M35 ~10 ms shape fires ticks again and reads real loop-lag | the M35 landing removed the middleware's per-request level-1 deflate from the events page — its ~2.9 ms median deflate was the loop stall this metric watched — dropping the whole-app-stack drive under the collector's own ticker interval and leaving `max(gaps)` an empty list; the fallback is the shape M14's definition already states (when the handler never yields, every gap is the handler's whole wall time) and the M25/M74 collectors already carry (`max(gaps) if gaps else wall`); the metric regains its regression watch; collector command only, no product code |
| 2026-09-15 | this PR | M35 events page, repaired collector: 4.27/4.55/4.55/4.51/4.71 → 1.08/0.98/1.12/1.14/1.42 ms medians, −73 % to −79 %, maxima 4.66-5.28 → 1.43-1.72 ms, every paired round faster (five interleaved rounds of the repaired collector — main checkout before vs branch worktree after back-to-back, shared snapshot of the 20534-event worst projection corpus, wire 104697 B and decoded 293374 B with parsed digest 2b451eddaa4c identical across all ten arms, load 2.4-3.0 one-minute); component attribution: the replaced middleware pass's level-1 deflate of the 293374 B page measures 2.94 ms median standalone (wire 104697 B) — the served click's dominant slice, which the middleware re-ran inside the send path on every request; no-regression witnesses on the same snapshot, interleaved ×5: view 3.36/3.11/3.31/3.30/3.22 → 3.17/3.12/3.96/3.59/3.36 ms and bootstrap 2.19/2.36/2.39/2.50/2.33 → 2.27/2.07/2.74/2.20/2.33 ms (both handlers untouched, their middleware deflate remains), digests 7b56cc98190e / 4193328a2e6f identical across all ten arms; M26 projection advance 0.20/0.17/0.16 → 0.21/0.22/0.13 ms with parity True digest e94c56635194 and M63 /view handler 0.54/0.62/0.43 → 0.59/0.41/0.44 ms interleaved ×3; 5422-passed suite + 11 skipped, ruff and yapf clean, plus 3 new tests (precompressed serve with decompressed parity and the vary header, repeat-click zero re-compress, advance-recompress); M35 events-page healthy range recalibrated < 0.03 s → < 0.004 s with this PR | the chat pagination endpoint served its memoized page body plain, so the browser's gzip-accepting fetch paid the middleware's whole-body level-1 deflate on every page click — 2.94 ms of the 4.27-4.71 ms served request on the 293374 B worst page — although the published projection is immutable and the page body is already memoized; the page's gzip form now rides the same projection generation as the plain body (one off-loop deflate per page per generation, mtime=0 deterministic, Content-Encoding set upstream makes the middleware skip its own pass — the M72 listing-serve mechanism, the M101 serve's mtime rule), and the standing collector was repaired to the raw-ASGI+gzip drive in the same PR because the TestClient drive had never seen the middleware's deflate — the vacuous-read class the M36/M59 repairs called out |
| 2026-09-15 | this PR | M59 worker thread-detail poll, repaired collector: full row 1.91/1.81/1.78 → 1.31/1.38/1.52 ms medians, −15 % to −31 %, maxima 2.43/2.25/2.29 → 1.45/1.71/2.86 ms; attach mode 1.72/1.71/1.67 → 0.44/0.45/0.44 ms medians, −74 % to −75 %, maxima 1.98/1.88/1.78 → 0.47/0.51/0.56 ms, body 48 B both arms (three interleaved rounds of old TestClient drive vs new raw-ASGI drive back-to-back, same main-checkout code and worst corpus in all arms, load 3.1-3.7 one-minute, 99.9 KB metadata.json, live state read-only; decoded body 50206 B and parsed digest 7184f3458354 identical across every arm, wire 22140 B under the mounted middleware); component attribution, same app + overrides, fresh drives at load ~2.4: TestClient repeat 1.92 ms vs raw-ASGI bare 0.42 ms (harness ~1.5 ms) and raw-ASGI+gzip 1.27 ms (the 50206 B row's deflate + middleware hop 0.85 ms); M59 healthy ranges recalibrated < 0.005 s → full row < 0.003 s, attach < 0.001 s with this PR | the same harness disease as the M36 repair in the same PR: the TestClient drive paid ~1.5 ms of httpx harness per request and skipped the middleware's deflate, reading the 5 s attach poll at 1.7 ms against a 0.44 ms served truth |
| 2026-09-15 | this PR | M36 worker list poll, repaired collector: full poll 2.02/2.15/2.10 → 1.58/1.35/1.34 ms medians, −22 % to −37 %, maxima 2.79/2.92/2.71 → 1.70/1.85/1.77 ms; conditional poll 1.96/1.96/1.98 → 1.44/0.59/0.61 ms medians, body 0 B (204) both arms (three interleaved rounds of old TestClient drive vs new raw-ASGI drive back-to-back, same main-checkout code and worst corpus in all arms, load 3.1-3.7 one-minute, 2551 KB thread metadata over 339 rows in session 3b91d606, live state read-only; decoded body 106314 B and parsed digest 8946dac083ec identical across every arm — the old drive's body bytes are the new drive's decoded bytes; wire 15077 B under the mounted middleware); component attribution, same app + overrides, fresh drives at load ~2.4: TestClient repeat 2.01 ms vs raw-ASGI bare 0.51 ms — the httpx layer is ~1.5 ms of harness per request — and raw-ASGI+gzip 1.17 ms, the 106314 B body's deflate + middleware hop 0.66 ms; M36 full median healthy range recalibrated < 0.02 s → < 0.004 s with this PR | the standing collector timed the harness, not the served path — the vacuous-read class the M57/M70/M72 repairs called out — and skipped the gzip middleware whose deflate the browser's 3 s poll always pays; the raw-ASGI drive (the M101/M72 pattern) reads the served path the middleware and route actually run |
| 2026-09-15 | this PR | M95 failed-iteration judgment pair median 40.52/40.97/41.17 → 6.65/8.87/6.48 ms, −78 % to −84 %, maxima 44.06-46.03 → 6.61-9.33 ms, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, 9.8 MB / 232-line worst on-disk worker log carrying one 9.5 MB tool_result line, live home read-only, resolved blocker/summary identical across all six arms, load 1.67-2.37 one-minute); component attribution: the prefilter-only draft read 27.7-30.8 ms (the parse gone, the 9.5 MB line's window walk + one 9.5 MB join left — a reject-all walk measures 14.2 ms standalone), the head-fragment join-skip removed the join; review-scan parity witnessed with five paired rounds at the elevated load: 1.07-1.15 → 1.09-1.39 ms medians (the early-stop shape the filter cannot slow; the morning's standing 0.60-0.64 ms read the lower load); no-regression witnesses on the branch: M31 steady-state events-summary read 0.0009 s (standing 0.0008 s) and M78 whole-file parse 51.1 ms chat / 42.6 ms worker log (standing 52.7/43.8 ms) — both ride the changed walk unfiltered; 5420-passed suite + 11 skipped, ruff and yapf clean, plus 3 new tests (the filter's keep/skip matrix over both writer shapes, the from-the-end filtered parity over a multi-window giant line, the `_newest_first_events` prefiltered parity with the judgments); M95 judgment-pair healthy range recalibrated < 0.050 s → < 0.012 s with this PR | the failed-iteration judgment pair's no-match exhaustion parsed the whole worst log — the ~35 ms orjson floor on its one 9.5 MB tool_result line — although both judgments match on five event types alone; the newest-first walk now takes a raw-line parse filter: a line whose head opens `{"type"` with a value outside the candidate set is skipped unparsed, and the walker extends the filter to a multi-window line's head fragment so a rejected head skips the 9.5 MB join too; the filter skips only what it can prove (every writer leads with `type`, one key per line), so any other shape parses — the walk's answers are identical, pinned by parity tests |
| 2026-09-15 | this PR | M71 capped search request median 4.94/4.53/5.19/4.83/4.58 → 3.98/4.50/3.61/4.26/3.88 ms, −1 % to −30 % (five interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, shared snapshot of 1187 metas + 159.3 MB active live chat files + triggers dirs, 200 rows / body 207987 B, parsed-body digest b85403422d40 identical across all ten arms, every paired round faster, load 1.9-3.3 one-minute; maxima 5.82-8.63 → 7.06-7.65 ms); component attribution, in-process route body over the same corpus: steady repeat 2.91-3.19 → 1.92-2.00 ms over 6 (−35 % to −40 %) — the whole-body cache serves the 207987 B body after one identity-and-values compare, and the per-row memo makes a churn rebuild (one row's state moved) re-splice that row and rejoin the rest; no-regression witnesses interleaved ×3: M8 absent-needle manager 1.03/1.68/0.85 → 1.00/1.07/1.02 ms (round 2's before arm read the round's load spike; never slower paired) and M56 /status 1.91/2.16/2.13 → 2.07/1.95/1.92 ms with digest 37ea57ed0807 identical; 5417-passed suite + 11 skipped, ruff and yapf clean, plus 3 new tests (a derived-state move re-renders and un-renders byte-identically, a changed row set rebuilds, the row-body memo cap) | the debounced search box's repeat keystroke re-spliced all 200 rows and re-rendered their 600 derived scalars per request although the 207987 B body is a pure function of the row sequence and each row's five derived values; both now ride the request check — the whole body serves from one slot keyed on (row identities, row states), a moved state re-splices only that row through the per-row memo (identity-pinned objects, the same invalidation ground the fragments memo stands on), and the manager's name scan the earlier rounds memoized is untouched |
| 2026-09-14 | this PR | M99 server import floor, `import server` (fresh process) median 0.674/0.654/0.663/0.719/0.708 → 0.632/0.643/0.636/0.669/0.670 s, −11 to −50 ms (−1.7 % to −7.0 %), maxima 0.680-0.777 → 0.653-0.702 s, every paired round and every paired max faster (five interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back at load 2.09-2.28 one-minute; component attribution, `-X importtime` on the same pair: `server` total cum 729.0 → 627.5 ms, the removed subtree 73.3 ms — src.agents.backends.opencode 26.2 (the session_usage constant carrier), registry 23.7 carrying charlie_code 20.0 (the autonamer and worker carriers) plus tui/codex/gemini_cli/kimi/openai_compatible_claude/antigravity_cli 3.5; `sys.modules` after `import server` holds none of them, and claude_code stays loaded (1.9 ms — its compaction-reserve constants feed session_usage's claude math)); no-regression witnesses interleaved ×3: M92 schedule-trigger --help 0.044-0.045 → 0.042-0.045 s, M97 plan list 0.297-0.310 → 0.295-0.307 s, M98 memory query 0.227-0.248 → 0.230-0.241 s medians, all standing bands; 5414-passed suite + 11 skipped, ruff and yapf clean; the import-weight contract's server ban set now pins the backends stack (registry, opencode, charlie_code) plus two new cases — the sessions chain (src.core.sessions) imports no backend module, and the autonamer/recap imports leave the registry unloaded until the first build resolves it | the server import chain dragged the whole backends stack through three module-scope carriers — autonamer's build_backend (chat → autonamer → registry), worker's build_backend (scheduler → spawner → spawner_finalize → worker), and session_usage's opencode import for one constant (sessions → session_usage → opencode); each build site now loads the registry at its one build through a binding-aware load_build_backend (the load_requests mechanism — the module attribute stays the tests' patch target, an existing binding is returned untouched, the M92-name-lookup lesson applied), and the opencode compaction reserve single-homes in src.core.constants (the #1412 stdlib-only home) whose only reader is session_usage's compact-point math; the first build (a naming round, a recap summarize, a worker run start) pays the ~35 ms import at its use site, the M102 run_probe precedent; M99 healthy range unchanged |
| 2026-09-15 | this PR | M57 plan-registry poll, repaired collector: 2.62/2.16/2.31/2.29 → 0.94/0.89/0.87/0.88 ms medians (standing sweep reading first, then three interleaved rounds of old TestClient drive vs new raw-ASGI drive back-to-back, same main-checkout code and worst plans corpus in all arms, load 2.03-2.75 one-minute, 15.4 KB plans.json of session a9bb2346, live state read-only; parsed-body digest f0098c1aae15 and body 10485 B identical across every arm; maxima 2.75-2.92 → 1.10-1.67 ms across the repair round's eight raw-ASGI runs); component attribution, same app + dependency override, fresh drives at load ~2.4: TestClient repeat 2.24 ms vs raw-ASGI 0.79 ms on the same 10485 B body — the httpx layer is ~1.4-1.5 ms of harness per request, and the standing reading had sat on that floor since the 2026-09-04 landing; M57 healthy range recalibrated < 0.0030 s → < 0.0020 s with this PR | the standing collector timed the harness, not the served path — the vacuous-read class the M68/M89/M90/M70/M72 repairs called out; the raw-ASGI drive (the M101 pattern) reads the served path the route actually runs, whose floor is 0.8-1.2 ms with the round's host load reading the 1.7 ms max |
| 2026-09-15 | this PR | M70 artifact clean-view serve, repaired collector: 0.0079/0.0081/0.0147 → 0.0008/0.0027/0.0007 s medians over three interleaved rounds (old TestClient drive vs new raw-ASGI drive back-to-back, same main-checkout code and snapshot in all six arms, load 3.61-3.92 one-minute; the old collector's "gzip body 1084806 B" was the httpx-decoded size — the wire body the raw drive reads is 809814 B, and the per-collector digests stand stable across all rounds: decoded c3352d6e6277, wire b8ff4b1735f0); component attribution, same app + middleware, fresh drives at load ~3: TestClient repeat 10.16 ms vs raw-ASGI 1.16 ms — the httpx layer is ~9.0 ms of harness per request on the ~0.8 MB wire body, and the old line sat entirely on that floor; M70 healthy range recalibrated < 0.010 s → < 0.003 s with this PR | the standing collector timed the harness, not the served path — the vacuous-read class the M68/M89/M90/M72 repairs called out — and the M70 landing's own −76 % fix had been invisible under that floor since 2026-09-13; the raw-ASGI drive (the M101 pattern) reads the served path the middleware and route actually run, whose served floor is 0.7-1.2 ms with the round's host-load spike reading 2.7 ms (the reading discipline the recalibrated range's note pins) |
| 2026-09-15 | this PR | M72 file-browser directory listing, repaired collector: 8.43/15.86/27.74 → 6.54/8.72/14.48 ms medians over the same three interleaved rounds (same main-checkout code, live sessions root read-only, 1186 entries, decoded body 260291 B with the sha1 identical within every round pair across both collectors — 4ca49b8b75d2 rounds 1-2, bb8b030a7d87 round 3 after a corpus move — so the collectors provably read the same pages); component attribution at load ~3: TestClient repeat 8.29 ms vs raw-ASGI 6.08 ms on the same app + middleware — ~2.2 ms of harness per request, which also amplifies host-load noise ~2x (the round-3 pair 27.74 vs 14.48); the standing 8.13-9.04 ms readings that tripped the < 0.008 s line at this round's start were harness + load bias over a served path whose walk floor is 4.1 ms of the 6.1-6.5 ms steady reading; M72 healthy-range cell gains the load-and-corpus reading note, value unchanged | the same harness disease as the M70 repair in the same PR: the httpx/TestClient layer rode every timed request with 2-3 ms of decode and header work that the served path never pays, and the floor grew with host load faster than the walk itself — the false trip this round opened with (8.13 ms standing vs the 6.1 ms served truth) |
| 2026-09-14 | this PR | M4 docs-only calibration, collector precision: old form "1 running sessions with last event older than 1h" — session 8a7964a3 (659: Redesign CharlieBot Session Tree), chat file 1.2 h stale while its running thread 75891ee8 (charlie-code-kimi-k3) had appended to its own worker log 0.1-0.9 min before the reading (670-672 KB and growing) — the in-flight-delegation shape, live work; new form on the same live state: 0 hung with the turn stats unchanged (174 turns, median 122 s, max 4834 s both rules); scratch shape checks, the same walk under both rules: an active session with a running thread whose chat file AND worker log are both 2 h stale reads hung 1 under both rules, the same session with a fresh worker log reads hung 1 → 0 (old → new), an archived session with a stale running marker stays 0 (the 2026-09-07 rule); no range change (hung = 0) | the 2026-09-07 archived-rule calibration's sibling class: the hung watch read the session chat file alone, and a delegation's chat file goes quiet for the delegation's whole run — the worker appends only its own events log and the summary lands at completion — so every hourly round during a > 1 h delegation read a false hung = 1 (this round's sweep tripped on exactly that shape); the collector now checks the running threads' own worker logs before counting, so the tripwire keeps catching genuinely stuck runs (chat and worker logs both stale) at zero extra scan cost on the common shape |
| 2026-09-14 | this PR | M103 config-dependency resolution, interleaved raw-ASGI drives at load 4.0-6.5 one-minute (main checkout before vs branch worktree after back-to-back, session corpus a scratch home per arm while the config-resolved state — the code-server probe and the repos discovery — was the live config in both arms, the delta arm-differentiated exactly by the dependency form under test; every paired round faster): GET /diff 880.7/933.9 → 764.1/579.5 us (−13 % to −38 %), GET / 1828.9/1860.3 → 1304.0/1298.3 us (−29 % to −30 %), GET /api/git/repos 784.2/778.3 → 646.5/668.0 us (−18 % to −14 %); the committed collector adds the config-dependency override its isolation declares (review finding), and its scratch-corpus band reads /diff 660/679, / 1093/1125, /api/git/repos 367/363 us; component attribution, the isolated dependency-shape drive at load 3.3: one sync `Depends(get_config)` 246.5 us vs a no-dependency route 61.5 us — the per-request threadpool round-trip the sync form pays (the M34/M52/M82 rows' 67-104 us no-op hop floor, queueing-amplified under load) vs the awaited form's dict check; M103 definition, collector, healthy ranges, and the route-walk guard test introduced with this PR | 22 routes across 10 api modules still resolved config through the sync `Depends(get_config)` — one FastAPI threadpool handoff per request on the hottest writes and reads: every user-message POST and chat upload, the OpenAI-compatible proxy POST (every proxied LLM call), the index/home/diff pages and the /diff viewer's three fetches, the delegate/Slack internal POSTs, the slash executor, session create/fork/backend-switch/recap-summarize/tui-stop/elone, cron create/update, and code-server open — although `get_config_on_loop` (the awaited form the polled routes took) already served the same memoized instance; all 22 sites now take the on-loop dependency and the guard test walks `server.app.routes`' dependency trees so no route can reintroduce the hop; the polled routes (sessions/threads status, list, view, events, search, scheduled, cron tasks GET) were already on-loop and their collectors (M35/M44/M46/M56) are untouched |
| 2026-09-14 | this PR | M7 restart-cold standing reading classified as the stale-document deploy-skew shape, not a regression: the verbatim collector read wall 6.840 s, 19 rows, scanned 2138.1 MB, rows digest 91801ae27a4c (load 4.58-6.01 one-minute) — 3.4x the < 2.0 s line — while the same collect against a document the current code had just rewritten read wall 1.341 s, 0.0 MB scanned, byte-identical rows (digest parity 98e7ab9edd27 == 98e7ab9edd27 on a same-window pair, two fresh processes back-to-back) — under the line; component attribution of the stale pass: the running server (started 2026-09-10 12:42) predates both the charlie-bot corpus source (#1352-era) and the opencode rows-map entry format, so its document carries no charlie-bot entries (0 of 6,571 lookups hit, 2,062.6 MB re-read, ~3.5 s) and stores the opencode db contribution as records without the rows map the seed path reads (prev.get("rows") is None, so the unseeded whole-blob scan runs, 76.5 MB / 2.7 s measured standalone) | the restart-cold collect reads the document the live server last wrote; a server predating a document-format landing makes every fresh-process collect re-read the whole corpus until the next deploy's own collect rewrites the document, then the metric returns to its zero-movement floor — the heal-at-deploy class the M96 row documents; until the restart, hourly rounds re-read this shape and should classify it, not chase it |
| 2026-09-14 | this PR | M93 standing count 12 AttributeError 500s on GET /api/threads/{sid}/threads/{tid} in the newest server log (healthy 0), spread 2026-09-10 15:21 through 2026-09-14 13:31 local across the 97 h window — the seed's pre-fix AttributeError class still firing because the running server predates the 2026-09-11 cli-binary fix; no code change exists to make, the count heals at the next deploy | re-confirmed deploy skew, recorded so hourly rounds read the growing count as the undeployed fix, not a new failure mode |
| 2026-09-14 | this PR | M96 standing reading re-confirmed as the 2026-09-13 row's deploy-skew shape: median 402270 B, p90 815424 B, max 1290826 B, total 12378399 B over 29 active sessions (load ~5 one-minute) — above the median < 0.15 MB and max < 0.60 MB lines, median up from 254402 B over 26 sessions the prior day — the undeployed pre-trim body shape serving a corpus that kept growing; the served shape on current code stays inside every line (the 2026-09-13 TestClient sweep's −62 %/−75 % readings) | the growth is the corpus's, the shape's is the deploy skew's; heals at the next restart |
| 2026-09-14 | this PR | M102 artifact wrap wall median 0.550/0.557/0.552 → 0.241/0.242/0.239 s, −56 % to −57 %, maxima 0.570-0.570 → 0.244-0.248 s, every paired round faster (three interleaved rounds of the new collector — main checkout before vs branch worktree after back-to-back, scratch fragment/output per round, load 0.91-1.23 one-minute; a standing first reading on main before the interleaved trio read 0.578 s at load 1.83); component attribution, fresh processes per arm: `import src.core.artifact_check` 0.471 → 0.118 s (the backends registry subtree — fastapi.routing cum 137 ms, src.core.runs cum 137 ms, src.agents.backends.opencode 18 ms, autonamer's sessions+streaming chain — and the KaTeX fetch's requests import leave the floor) and `import src.cli.artifact` 0.50 → 0.21 s; no-regression witnesses interleaved: M97 plan list 0.290 → 0.290 s medians both arms (the plan chain's read path never imports the registry), M92 schedule-trigger --help 0.041/0.043 s and M99 import server 0.724/0.739 s (both standing bands); the check verb's wall keeps its work — run_probe imports the registry stack inside the probe, so a check run pays the same imports at its use site; 5548-passed suite + 11 skipped, ruff and yapf clean; M102 definition, collector, and healthy range introduced with this PR | the artifact chain was the last CLI verb module still importing the server's validation stack at module scope: artifact_check bound build_backend (backends.registry) and iter_light_backends (autonamer → sessions + streaming) for the probe alone, and artifact_wrap bound requests for the one-time KaTeX CDN fetch; both now load at their use sites — run_probe imports the registry stack inside the probe, ensure_vendored_katex imports requests on the CDN-fetch path only (a module __getattr__ keeps the tests' patch targets valid, the src.cli.common mechanism) — and the verb module routes get_config through the common forwarder so the chain honors the M92 floor rule; the genre vocabulary stays single-homed in artifact_check (GENRES keeps its registry-derived name) |
| 2026-09-14 | this PR | M99 server import floor, `import server` (fresh process) median 0.815/0.805/0.792 → 0.781/0.774/0.731 s, −31 to −61 ms (−3.9 % to −7.7 %), every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back at load 1.44-2.00 one-minute; the branch's verbatim-collector median 0.728 s sits back under the 0.75 s line; component attribution: `croniter` cum 19.4 ms (self, carrying dateutil.relativedelta 1.7 ms) via src.api.cron + src.core.scheduler, `websockets.asyncio.client` cum 12.7 ms via src.core.slack_listener — all three absent from the after arm's profile and `sys.modules` after `import server`); the import-weight contract's server ban set now pins croniter, dateutil, and websockets alongside numpy/structlog/httpx; no-regression witnesses on the branch: M44 scheduled-list 2.11 ms median (standing ~2.0 ms) with the lazy first-use croniter path serving the same 13-row shape, M42 scheduler tick loop-lag 0.0000 s (standing 0.0000 s) through `_maybe_run`'s primed global, 5547-passed suite + 11 skipped, ruff and yapf clean; the round's before readings sat 0.763-0.818 s — above the line and ~100 ms above the 2026-09-13 band (0.654-0.694 s) — while a git-dir-clean archive A/B of d9a977c1 (the 09-13 httpx-deferral state) vs this branch read 0.813 vs 0.783 s in identical cold directories, so the elevation is environmental, not a code regression: this deferral restores the floor's headroom against the standing line | every server start and every fresh process importing `server` paid croniter (19.4 ms self with its dateutil subtree — both pure-python next-run expanders) and the websockets asyncio client (12.7 ms) at import although the import path resolves no next fire and opens no Socket Mode connection; the imports now ride their first uses — croniter at the scheduler's first due-task resolution and the /scheduled handler's next-run memo miss (a module-level first-use loader whose bound attribute stays the tests' monkeypatch target), websockets inside the Slack listener's connect loop with the connection type's annotation under TYPE_CHECKING — the same pattern the requests/structlog/httpx/config deferrals landed |
| 2026-09-14 | this PR | M34 full fetch body 251479 → 112732 B, −55 %, byte-identical within each arm (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, 9.8 MB / 232-event worst on-disk worker log whose 36 tool_result rows over 500 chars carry 153.3 KB of content, top sizes 20000/20000/18578 B, scratch CHARLIEBOT_HOME per arm, live home read-only, load 2.00-1.66 one-minute; full fetch handler median 0.0024-0.0026 s and after=total 39 B both arms — the bytes are the moved metric); no-regression witnesses interleaved ×3: M13 steady-state read+transform 0.0000 s both arms, M36 full poll 2.01-2.11 ms body 106314 B and conditional 204 0 B both arms, M68 marked rebuild 3.27-3.39 ms body 106314 B both arms; 5547-passed suite + 11 skipped and the 57-passed frontend suite, ruff and yapf clean, with the three projection-cap tests flipped to the preview bound and the toggle pin flipped to the note shape; M34 full-fetch body bound recalibrated < 300 KB → < 200 KB with this PR | the workers projection was the last wire shape still carrying whole tool rows: each projected tool_result output rode the wire up to TOOL_OUTPUT_RENDER_CAP (20000 chars) so the panel's Show-more toggle could reveal the tail, while the client renders an output's first 500 characters inline and the persisted events log keeps the full text; the projection now trims each output to TOOL_PREVIEW_CHARS (500 — the M94 chat wire's bound, single-homed in the aggregator) with the row's output_truncated marker set, and the workers panel renders the raw-log note instead of a hidden 20 KB span; the body now scales with oversized-row count at the preview size (the corpus's 36 rows read 18 KB of content where they read 153 KB) |
| 2026-09-14 | this PR | M101 raw events download, gzip-accepted: loop-lag median 12.00/11.79/10.49 → 5.37/5.41/5.39 ms (the 5 ms ticker floor), maxima 19.28-21.93 → 5.48-5.54 ms; steady-state wall median 1270.0/1265.7/1288.5 → 0.6/0.6/0.6 ms, maxima 1314.3-1319.9 → 1.0 ms over 9 (three interleaved rounds of the new collector — main checkout before vs branch worktree after back-to-back, 36.3 MB / 5519-event worst live chat file of session aa196b47, wire 22.7 MB gzip identical across all six arms, load 0.70-1.20 one-minute, every paired round faster; the first view a fresh open pays measures 741/753/811 ms off-loop on the branch — the read+compress a thread hop carries — and the live server log reads the same endpoint at 1542/1663/1849 ms server-side, 2026-09-11 11:45-12:10); no-regression witnesses on the branch: M65 big-page gzip loop-lag 5.30 ms / wall 3.51 ms (standing 5.28/3.54 this morning) and M35 events page 2.67 ms with digest 1217561fba10 identical (standing 2.67 ms); 5547-passed suite + 11 skipped, ruff and yapf clean; M101 definition, collector, and healthy ranges introduced with this PR | the events viewer's fetch and its download link ride `GET /api/sessions/{id}/events.jsonl`, a FileResponse whose 64 KiB streaming chunks the gzip middleware compresses inline on the event loop — 576 inline deflate slices of ~11-22 ms worst gap each and 1.3 s of server-side wall per download of the worst corpus, on the loop every concurrent poll and WebSocket shares; the download now reads and deflates in one executor hop (level-1 gzip, 755 ms measured standalone on this corpus) behind a stat-keyed memo serving repeat opens of an unchanged file with zero corpus bytes, and Content-Encoding set upstream is what makes the middleware skip its own pass (the M72 listing-serve mechanism); a client sending no Accept-Encoding: gzip still reads the plain FileResponse stream unchanged |
| 2026-09-14 | this PR | M97 plan-CLI command wall median 0.299/0.302/0.303/0.302 → 0.273/0.275/0.277/0.284 s, −19 to −29 ms (−6.6 % to −8.9 %), maxima 0.307-0.320 → 0.279-0.291 s; M98 memory-CLI invocation wall median 0.236/0.238/0.238/0.240 → 0.212/0.217/0.217/0.221 s, −19 to −24 ms (−8.0 % to −9.9 %) (four interleaved verbatim-collector rounds, 15.4 KB worst plans corpus of session a9bb2346, live state read-only, main checkout before vs branch worktree after back-to-back at load 0.45-0.92, every paired round faster; component attribution, fresh processes per arm: `import src.core.config` 179.8 → 151.5 ms and the chain no longer loads src.core.models at all); no-regression witnesses interleaved ×2: M92 schedule-trigger --help floor 0.039-0.040 s both arms and M99 `import server` 0.637-0.646 s both arms (the server's api modules import src.core.models directly, so its floor is untouched); 5539-passed suite + 11 skipped, ruff and yapf clean | the config chain built all 62 of models.py's pydantic models on every CLI invocation's first `get_config` while the config schema's fields ride three of them — the backend-option discriminated union and the two Claude-account models, which now live in `src/core/backend_models` (imported by config directly) with models.py re-exporting the moved names so every established `src.core.models` import path keeps working; the pydantic import itself and the config module's own exec are load-bearing (CharlieBotConfig validates through pydantic), and the requests import (57 ms, lazy at request time) is the remaining floor of the verb walls; M97/M98 healthy ranges unchanged |
| 2026-09-14 | #1553 (row recorded in this docs-only follow-up per the #1046 precedent, the landing PR shipped without it) | M88 direct-pass build median 2.76/2.84/2.79 → 2.51/2.55/2.42 s, −9 % to −13 %, maxima 2.82-2.85 → 2.43-2.61 s, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, 307.3 MB / 1,068,461-event worst on-disk trace /home/chaoli/data/hayden_243809_traces/step000110/trace_rank008_step000110.json, scratch output under /tmp, live home read-only, load 1.69-2.10 one-minute; artifact 23.8 MB.gz identical across all six arms); component attribution, standalone interleaved parse of the same corpus, fresh process per round: gc-on 2.165/2.061/2.087 s vs gc-off 1.794/1.765/1.734 s — 0.27-0.35 s per parse, the slice off the build's floor; no-regression witness: M66 merged build median 3.93 s on the branch (standing band 3.8-4.0 s), the merge path untouched; 5542-passed suite + 11 skipped plus the new GC-contract test (a build's success and parse-failure paths must both re-enable collection), ruff and yapf clean | the direct-pass build's validation parse allocated ~1M dicts with GC enabled while the merge path's build has run GC-off since the M66 landing for the same measured churn; the parse holds the GIL solid either way, so the #1520 gzip-subprocess overlap leaves the parse the build's floor and the disable trims that floor |
| 2026-09-13 | this PR | M96 standing-collector reading classified as the deploy-skew shape, not a regression: the verbatim collector against the running server read median 254402 B, p90 1066055 B, max 1165935 B, total 10308741 B over 26 active sessions (23:46, load 1.14/1.32/1.01 one-minute) — above the median < 0.15 MB and max < 0.60 MB lines — while the same 26 sessions served through the current code read median 97618 B, p90 214912 B, max 295040 B, total 2776189 B (TestClient on the main checkout @ 35a10fb9, scratch CHARLIEBOT_HOME under /tmp holding the 26 sessions' metadata + data with master_runs excluded, live home read once for the id list and the copy, never written) — −62 % median, −80 % p90, −75 % max, inside every healthy line. The live server (started 2026-09-10 12:42) predates the 2026-09-12 payload trim, so its bootstrap bodies still carry every tail tool's whole input and output; the corpus the untrimmed shape serves also grew ~11 % since the landing day's live-before sweep (25 sessions median 244912 B → 26 sessions median 254402 B) | the M96 healthy ranges were set from the landing day's after numbers on branch code, while the standing collector points at the running server, which carries the trim only from its next deploy on — until then every hourly round reads the pre-trim shape and re-chases a fix that already landed; this row pins the trip as deploy skew that heals at the next server restart, not by code (the same class the M7 restart-cold row documents) |
| 2026-09-13 | this PR | M99 server import floor, `import server` (fresh process) median 0.702/0.713/0.706 → 0.671/0.665/0.654 s, −31 to −52 ms (−4.4 % to −7.4 %), maxima 0.706-0.723 → 0.663-0.675 s, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back at load 1.38-1.54 one-minute; component attribution: `httpx` cum 60.6 ms on the before arm's `-X importtime`, httpx._main 43.5 ms of it carrying rich.console 21 ms, and `httpx` absent from the after arm's `sys.modules` after `import server`); no-regression witnesses on the branch: `import src.cli.plan` 0.039 → 0.038 s interleaved ×2 (the CLI chains never touched httpx), 5541-passed suite + 11 skipped unchanged, ruff clean; the import-weight contract's server ban set now pins httpx's absence | the server import floor paid httpx's import chain although no startup path sends a request — the four importers on the chain (anthropic_proxy's two helper annotations, the shared client singleton, the Slack listener's two annotations, and the opencode backend's three client constructions) now load it at their use sites; the opencode backend keeps the PEP 562 hook serving the tests' `src.agents.backends.opencode.httpx.*` patch target, the M92-requests precedent |
| 2026-09-13 | this PR | M74 turn-end rescan loop-lag median 0.0206/0.0203/0.0210 → 0.0157/0.0146/0.0151 s, −24 % to −30 %, maxima 0.0286-0.0383 → 0.0230-0.0257 s; wall median 0.0212/0.0209/0.0216 → 0.0153/0.0157/0.0163 s, −23 % to −27 %, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back at load 1.04-1.72 one-minute, 10.1 MB / 64-line worst on-disk raw log carrying one 9.99 MB observation line, live home read-only; the before arm's checkout sits parked on pr-1521, whose runs.py is byte-identical to main at this fix's funnel — the pr-1521..main delta there is the #1530 naming refactor only; component attribution, separate processes per arm: parse_raw_lines 27.35 → 13.00 ms median over 5); no-regression witnesses on the branch: M84 tail-follow replay 14.5 ms / stdout-stream 13.0 ms (standing bands) with parser parity 0 divergences, M78 whole-file parse 48.4 ms chat / 40.5 ms worker log (standing band), 5541-passed suite + 11 skipped, ruff clean | the whole-file raw-log parse sliced every line out of the read buffer as a bytes copy before the parse, and the copy is parse inflation at the multi-MB line a tool-result-heavy turn produces — the same floor the M84 row documented on the streaming funnel (a 10 MB line ~5 ms of copy plus ~8 ms of parse inflation from the copy's cold cache), which the whole-file sibling kept; lines now ride zero-copy memoryview slices, valid lines parse straight off the slice, and a rejected line takes parse_ndjson_line's verdict (blank invisible, malformed logged, torn multibyte as U+FFFD) instead of an inline duplicate of the contract; the many-small-lines shape moves within run noise (component-level ~+4 % worst case, ±16 % run noise on the 20k-line bench; M74's typical-turn shape reads the 5.4 ms ticker floor in both forms per its definition) |
| 2026-09-13 | this PR | M89 stderr pump per chunk 89.9/89.2/84.5 → 24.6/23.2/24.1 µs, −72 % to −73 %, maxima 96.9-100.6 → 24.2-26.6 µs (400-chunk pump wall 35.98/35.68/33.81 → 9.84/9.29/9.66 ms); M90 stdout pump per chunk 90.8/87.0/73.0 → 12.3/11.9/12.0 µs, −86 % to −87 %, maxima 97.9-92.9 → 15.1-12.3 µs (400-chunk pump wall 36.31/34.81/29.22 → 4.92/4.76/4.81 ms); every paired round faster (three interleaved rounds of the repaired collector — main checkout before vs branch worktree after back-to-back, 400 scripted 8 KB chunks per round, scratch log per round, load 1.67-1.81 one-minute; the standing collectors timed the flush primitive per chunk — a shape the batching makes per-window — so both were repaired to drive the real pumps, the vacuous-read class the M68/M70 repairs called out); M90 startup line unchanged 80/76/80 → 84/69/68 µs both arms (the per-line write the startup wait issues, by design); healthy ranges recalibrated with this PR: M89 median < 0.0002 s → < 0.00005 s, M90 chunk median < 0.0002 s → < 0.00003 s (line unchanged); no-regression witnesses on the branch: M84 tail-follow replay 15.2 ms / stdout-stream 13.4 ms with parity 0 divergences (standing 15.3/13.3), M82 events-log append 2 µs medians both eras, M38 fan-out 186 serialize calls 1 ms inside the #1524 after band with final-frame parity true; 5539-passed suite plus 4 new batching tests (window parity, sparse-line liveness, tail-stays-per-chunk, stdout parity) | every covered backend's stderr tee and the opencode/antigravity stdout pumps paid one asyncio.to_thread executor hop per 8 KB chunk — 73-91 µs of handoff against a ~2-5 µs page-cache write — so a 10 MB streamed turn's pump cost ~36 ms of pure dispatch (1250 hops) whose executor traffic contended with the request path's own to_thread work; the pumps now batch into a 64 KB window flushed through the existing _write_chunk (one hop per window), the flush firing at window-full, at a short read (the StreamReader's drain signal, so a sparse stream's every line lands immediately and tail -f stays live), and at stream end; the in-memory stderr tail stays per chunk, and a pump cancelled mid-window loses at most one buffered diagnostic log window |
| 2026-09-13 | this PR | M72 served-path repeat view, repaired collector: 9.08/9.51/8.73 → 7.05/7.34/6.73 ms, −22 % to −29 %, maxima 10.24-11.05 → 8.57-9.70 ms, every paired round faster (three interleaved rounds of the repaired collector — main checkout before vs branch worktree after back-to-back, 1165-entry sessions root, live state read-only, decoded body 255692 B sha1 6219a0514d10 identical across all six arms, load 1.00-1.16 one-minute; the standing collector had read this metric 7.09 ms since its landing because its bare FastAPI app mounted no gzip middleware and sent no Accept-Encoding header — the served-path deflate was invisible to it, the vacuous-read class the M68/M89/M90/M70 repairs called out, and the repaired collector's before reading sat above the 0.008 s line); component attribution: the middleware's off-loop level-1 deflate of the 255692 B page measured 1.16 ms standalone plus the responder's to_thread round-trip, both gone from a repeat view that now serves the memoized compressed form; no-regression witnesses on the branch: the bare collector's no-Accept-Encoding arm 7.09 ms with the same sha1 (the identity path untouched), changed-round rebuild 5.33 ms (standing band 5.33-6.67), M70 clean view 7.3 ms (standing band 7.3-7.7), M55 compare view 2.4 ms with digest be3110683106 byte-identical to the standing reading; 5533-passed suite plus 4 new listing-gzip tests (precompressed ship through the middleware, no-gzip-accept plain body, repeat-view zero deflate, corpus-move recompress) | every browser navigation click on the file browser paid the server's whole-body gzip middleware a level-1 deflate of the memoized listing page plus its off-loop thread hop per view — the artifact view's identical pathology received the memoized-gzip fix in the M70 landing; the listing now memoizes the compressed form beside the plain one under the same walked-state key and ships it with Content-Encoding: gzip set upstream, which is what makes the middleware skip its own deflate, and a client sending no Accept-Encoding: gzip still reads the plain body |
| 2026-09-14 | #1524 (row recorded in this docs-only follow-up per the #1046 precedent, the landing PR shipped without it) | M38 wire-serialize total 4/4/4 → 1/1/1 ms per worst-turn replay, −75 % at the collector's whole-ms rounding, every paired round faster (three interleaved rounds of the repaired collector — main checkout before vs branch worktree after back-to-back, worst on-disk stream turn of session 7a966888, instant feed, one subscriber, final-frame parity True every arm, load 1.19-1.75 one-minute); component attribution through the real StreamingManager with perf_counter around the render, three interleaved rounds at load 1.50-1.75: stdlib 185 calls 3.86/4.06/3.78 ms → orjson 185 calls 0.56/0.55/0.56 ms, −85 % to −86 %; the standing collector read this metric 4 ms since its 09-03 landing because its stdlib-json.dumps wrap never counted the after-arm's renderer — the repair wraps both json.dumps and orjson.dumps so the counted cost is the wire render whichever renderer the checkout runs (the #1285 vacuous-read class); no-regression witnesses on the branch: M45 catchup replay digest e9f92b4cfe29 identical with wall 0.0288 s / loop-lag 0.0065 s (standing 0.0299/0.0065), M26 advance 0.12 ms parity True digest e94c56635194, M94 page 0.20 MB / streamed serialized 5.5 MB / dumps wall 21 ms unchanged (its dumps wall is the collector's own fixture render, independent of the product serializer by design); 5536-passed suite plus 3 new wire-render tests (parsed-parity vs the stdlib send_json form on a CJK/emoji frame, the NaN→null boundary, the non-str-key raise) | the broadcast fan-out and the catchup replay still serialized every wire frame with the stdlib C encoder — 3.8 ms of event-loop json.dumps per worst-turn replay, the one wire-serialization boundary the orjson sweep (#1490) never reached — while the shared orjson render spends 0.56 ms on the same 185 frames; both sites now ride responses.py's fast_json_bytes, whose two pinned boundaries (NaN/Infinity as null, non-str dict keys raising) replace the stdlib send_json byte form on the wire, the same boundary move the M35 response-render landing made; parsed content is unchanged for the client |
| 2026-09-13 | this PR | M80 churn changed-round wall median 0.1429/0.1529/0.1502 → 0.1106/0.1121/0.1052 s, −27 % to −31 % (four after rounds 0.1052-0.1121 s including an initial 0.1101 s before the interleaved trio), scanned 1.45 MB and rows digest df92885496c3 identical across all seven arms (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back at load 1.31-3.22 one-minute); M7 changed-round collect median 0.156/0.155/0.147 → 0.112/0.111/0.110 s, −28 % to −29 %, 19 rows and 0.0 MB re-read both arms (three interleaved rounds of the verbatim harness, the branch arm with its sys.path line at the worktree root); component attribution: the warm charlie-bot corpus walk measured standalone 0.1056/0.1070/0.1050 → 0.0679/0.0678/0.0673 s median, −36 %, 6352 rows both arms; whole-corpus cold pass rows digest 100098c6f26a and scanned 2059.4 MB identical, wall 7.223 → 6.877 s; 5529-passed suite plus 4 new walk-contract tests (a late candidate file's discovery, absent-candidate silence, the never-listed deep directories, symlinked-entry skip) | the walk listed and statted every intermediate directory — 1164 session dirs × (threads/ + every thread dir + its data/ + master_runs/ + every run dir) ≈ 33 k stats per collect against 6352 corpus files, the corpus signature's floor paid on every /token-usage page load — while both file names are writer-pinned constants (threads.thread_events_log_path, runs.RAW_LOG_NAME), so the deep listings discover nothing the fresh per-candidate stat does not; the walk now lists only the three discovery levels (the sessions root, each threads/, each data/master_runs/, still memoized on the directory's own stat pair) and stats each candidate directly — a deep file's appearance or disappearance moves only its own containing directory, which the walk never lists, so the fresh stat is the only thing that can see it; one stat per candidate plus one per discovered directory is the walk's floor, and the stat count now scales with candidates (6352 present + 8504 known-absent) instead of with corpus directories |
| 2026-09-13 | this PR | M54 stream-draft paint work, three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back at load 2.41-2.95 one-minute: paint-work median 0.144/0.164/0.153 → 0.123/0.107/0.112 s, −15 % to −35 %, maxima 0.262/0.243/0.242 → 0.152/0.146/0.156 s, −39 % to −40 %, every paired round faster, final-frame parity true all six arms; component attribution: hljs.highlightAuto ran 2 calls for 123.7 ms of the 261.0 ms worst replay pre-fix — the pinned build's first-call grammar compilation — and post-fix paint#1's first real auto reads 21.1 ms (was 113.2 ms), the compile absorbed by the idle warm outside the timed paints; no-regression witnesses interleaved: M33 replay wall median 0.035-0.039 s (standing 0.037 s), M60 cold first paint 0.049 s / highlight flush 0.182 s (standing 0.049/0.190 s), M81 page re-render 1.03 ms with 0 walks (standing 1.12 ms); 607-passed node suite plus the rIC-only warm, 5525-passed suite, ruff clean | highlight.js compiles each grammar on first use and the whole compile (~113 ms across the 36-language common build) landed inside the first paint that highlights code — a streamed turn's paint#1 or a committed render's highlight flush — on every page load's first code-bearing turn; one idle auto-highlight over a prose-plus-fence snippet now compiles the grammars chat content exercises in one pass off the render path, and the stream harness's new idle queue lets the M54 replay charge the warm to page load, where the browser pays it, never to a timed paint (rIC-only scheduling keeps the warm out of the timer queue the deferred highlight flush drives) |
| 2026-09-13 | #1490 (row recorded in this docs-only follow-up per the #1046 precedent, the landing PR shipped without it) | M35 chat message-page responses on the 20534-event worst projection corpus (shared snapshot home), three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back at load 0.6-1.0 one-minute: events page median 2.53/2.52/2.66 → 2.46/2.45/2.41 ms (body 347923 → 293374 B, −16 %), view median 3.20/3.16/3.48 → 3.04/2.83/2.82 ms (body 135521 → 119739 B, −12 %), bootstrap median 2.60/2.55/2.65 → 2.24/2.46/2.29 ms (body 93303 → 77563 B, −17 %), every paired round faster on view/bootstrap; component attribution: the pure render of the 91992 B bootstrap payload median 0.265 → 0.023 ms (~11x), body 76264 B; the events-page timed repeats ride the projection's body cache, so the render saving lands on each slice's cold first render; no-regression witnesses interleaved ×2: M56 status poll 2.06/2.09 → 2.27/1.97 ms with the spliced body digest byte-identical across all four arms (8b79ddc76566), M8 search 0.002-0.003 s both arms, M3 401 path 0.001 s both arms; 5523-passed suite plus the re-pinned render contract | the request-path render rode the stdlib C encoder in ensure_ascii=True mode — 0.265 ms on the 92 KB bootstrap payload where orjson spends 0.023 ms — and the escaped body carried \uXXXX escapes where raw UTF-8 serves the same parsed content 16-17 % smaller on the CJK-bearing corpora; the swap's two serializer boundaries are deliberate and test-pinned (NaN/Infinity renders as null — valid JSON, the stream funnels' boundary; non-str dict keys raise instead of the stdlib's silent coercion) |
| 2026-09-13 | this PR | M99 server import floor, `import server` (fresh process) median 0.717/0.723/0.718 → 0.683/0.694/0.685 s, −4.7 % to −5.3 %, maxima 0.721-0.735 → 0.696-0.716 s, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back at load 1.35-1.60 one-minute; component attribution: `structlog` absent from the after arm's `sys.modules` where the before arm's `-X importtime` shows structlog cum 82 ms — structlog.dev's rich + pygments the bulk — pulled by the first API module binding it; no-regression witnesses on the branch: M58 get_config 5.3 µs median, M13 steady-state read+transform 0.0000 s, M26 projection advance 0.20 ms parity True digest e94c56635194, M53 onset 1 warning + 1 re-parse / steady 0 + 0, M22 and M47 steady 0 warnings through the proxy's log seam, 5522-passed suite plus `structlog` in the server ban set) | the server import floor sat 4 % under its 0.75 s healthy line paying structlog.dev (rich, pygments) at import although the import path emits no log line; server.py and the 62 server-chain modules now bind the same structlog-deferring proxy the M92/M98 landings gave config and memory (single-homed in src.core.log_once by #1434), moving the import to the first log call |
| 2026-09-13 | this PR | M84 tail-follow replay median 36.9/36.7/36.9 → 14.1/14.3/14.3 ms, −61 % to −62 %, maxima 37.1-38.2 → 14.7-15.2 ms (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, 10.1 MB / 64-line worst on-disk raw agent log carrying one 9.99 MB observation line, scratch copy, live home read-only, parser parity 0 divergences both arms, every paired round faster at load 0.39-0.44 one-minute; component attribution on the pre-fix replay, perf_counter instrumentation around each loop operation: orjson parse 22-24 ms in-loop against the 13.8 ms bare big-line parse, the chunked-read walk ~14 ms — bytearray append 4.9 + del 3.2 + memoryview line copy 5.2 + reads 1.0; the post-fix drain reads to EOF once per round and hands orjson zero-copy memoryview line slices — a 10 MB synthetic parsed from a view measured 9.8 ms against 21.7 ms for the pre-copied bytes, the copy itself was the parse inflation, and the remaining read is the C-level readall join ~5 ms); no-regression witnesses interleaved ×2: stdout-stream replay 12.8-12.9 → 12.6-13.2 ms (the untouched sibling funnel), M74 turn-end rescan loop-lag 0.0201-0.0205 → 0.0198-0.0203 s, M78 whole-file parse 48.6-48.7 → 49.1-49.2 ms chat / 41.3-41.6 → 41.2-41.5 ms worker log, M31 events-summary read 0.0008 s medians both arms, M85 verify-final report read 0.6-1.1 ms both arms; 5522-passed suite plus the memoryview skip-contract test and the two staged-append carry tests that replace the retired 64 KB boundary shape | the tail-follow loop's per-64 KB-round bytearray carry re-grew and compacted the whole accumulated buffer and copied each completed line out of it — ~14 ms of churn per 10 MB backlog on the live read side of every covered backend's streamed turn — while a drain-to-EOF read plus zero-copy memoryview line slices pay one C-level readall and parse straight from the read buffer; parse_ndjson_line now accepts bytes-like views under the same skip contract, its replace fallback copying once on the parse-failed path only |
| 2026-09-13 | this PR | M97 plan-CLI command wall, `charliebot plan list --session <sid>` median 0.307/0.309/0.316 → 0.300/0.295/0.307 s, −2 % to −5 %, maxima 0.315-0.327 → 0.305-0.317 s, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back at load 0.90-1.19 one-minute; component attribution: `import src.cli.plan` loaded plan_diff for the diff verb's single `diff_text` call, dragging difflib + html.parser + html — dataclasses/inspect ride back with the pydantic chain at get_config time either way, so the net wall slice is ~10 ms; the import-weight contract's plan-chain case now bans `src.core.plan_diff`, red on the eager import; no-regression witness interleaved ×3: M92 schedule-trigger --help 0.039-0.042 s medians both arms, the standing band; 226 plan tests + the 8-case contract green pre-push) | every plan command — the master's plan-delivery verbs, several per delegation — paid plan_diff's module import although only the diff verb renders diff text; the import now rides `_build_diff`, the only consumer |
| 2026-09-13 | this PR | M62 base-less base-resolution chain median 0.3462/0.3302/0.3313 → 0.1962/0.1987/0.2018 s, −41 % to −43 %, maxima 0.3404-0.3669 → 0.1978-0.2343 s, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back at load 1.29-1.64 one-minute, start_point origin/main round-stable across all arms; component attribution standalone against the same origin: the probe `git ls-remote` at protocol v2 0.30-0.36 s vs protocol v0 0.20-0.21 s — the raw info/refs GET measured 0.147-0.151 s total with TLS done at 0.035 s, so v2's second round trip was the probe's dominant slice; the chain's two local rev-parses are ~10 ms each) | the base-less launch's probe and resolve_base_branch's self-probe both ran `git ls-remote` on the default protocol v2, whose ls-refs command costs a second network round trip per probe although ls-remote consumes only the ref advertisement the first response already carries; both probes now pass `-c protocol.version=0` per invocation, which serves the whole advertisement in the one GET — the same answer, one round trip cheaper — while path/file remotes ignore the version, so the resolution matrix's fixtures and tests are unaffected |
| 2026-09-13 | this PR | M70 repeat-view under the production gzip shape, repaired collector: 0.0343/0.0345/0.0344 → 0.0086/0.0074/0.0073 s, −76 % to −79 %, maxima 0.0355-0.0370 → 0.0085-0.0089 s, every paired round faster (three interleaved rounds of the repaired collector — main checkout before vs branch worktree after back-to-back, 1.08 MB worst on-disk artifact report_frame_anchored_displacement_distribution_v1.html, scratch snapshot home, live home read-only, load 0.66-0.95 one-minute; first views 0.0540-0.0564 → 0.0524-0.0534 s — the cold pass still reads, injects, and deflates once, the shape a first view must pay; decoded bodies 1084806 B both arms, the gzip form decompresses byte-identical to the plain injected body, the cross-checkout digest gap the ?v= cache-bust string the M55 row documents; the standing collector had read this metric 0.0026-0.0028 s since its landing because its bare FastAPI app mounted no gzip middleware — the served-path deflate was invisible to it, the vacuous-read class the M68 and M89/M90 repairs called out); the healthy range stays < 0.010 s — the repaired collector's before reading sat 3.4x above it | every credentialed artifact view paid the server's whole-body gzip middleware a level-1 deflate of the injected page on each response — 27.3 ms standalone on the 1.08 MB worst page, inside a 34 ms repeat view — although the served page is memoized and immutable between writes: the view now ships the memoized gzip form with Content-Encoding: gzip set upstream (the header is what makes the middleware skip its own deflate, content_encoding_set in starlette's responder) and Vary: Accept-Encoding, the compressed form memoized beside the plain one on the same (path, mtime_ns, size) key so a rewrite re-compresses, and a client sending no Accept-Encoding: gzip still reads the plain body |
| 2026-09-13 | this PR | M92 CLI invocation wall, `charliebot schedule-trigger --help` median 0.044/0.043/0.042 → 0.039/0.043/0.039 s across three interleaved verbatim-collector rounds (main checkout before vs branch worktree after back-to-back, load 1.45-1.51 one-minute) and 0.0415-0.0424 → 0.0386-0.0408 s across a five-round sweep at load 1.43-1.68 — never slower in eight paired rounds, −2 to −4 ms typical (−5 to −9 %); component attribution (`-X importtime`, `-m src.cli.main schedule-trigger --help`): src.core.buildinfo cum 3.6-3.9 ms (subprocess 2.8 ms + datetime + importlib machinery) riding src.cli.common on the before arm, absent on the after arm with `sys.modules` confirming neither buildinfo nor subprocess at import; no-regression witnesses: the import-weight contract's buildinfo-deferral case (red on the eager import), the version-skew suite (the call-site import still binds monkeypatched `buildinfo.read_repo_head_sha`), and the 5515-passed suite | every `charliebot` invocation — the master's and workers' several per turn — paid buildinfo's subprocess chain at import although only the version-skew failure path reads the local SHA; the import now rides that call site, the same slice comes off every verb wall that imports src.cli.common (M97/M98 read it as noise at their scales), and the import-weight contract pins the absence |
| 2026-09-12 | this PR | M100 broadcast frames per signal 51 → 0; cold read+transform of the 51-signal scratch log: raw rows 51 → 0, wall 3.92/4.02/3.14 → 0.15/0.11/0.15 ms, −96 %, maxima 4.02-4.02 → 0.15-0.15 ms; worker append median 6.1/6.7/7.8 → 4.8/4.2/4.1 us (the broadcast hop gone); chat marker lines 6 → 6 both arms with captured session id 'oc-attach-probe' identical across all six arms — the master funnel's durable append is the stable-history projection's run-start marker, load-bearing and unchanged by design (three interleaved rounds of the new collector — main checkout before vs branch worktree after back-to-back, scratch CHARLIEBOT_HOME per arm, live home read-only, load 1.62 one-minute); M100 definition, healthy range, and collector introduced with this PR; the corpus's standing residue, read-only counts: 11,910 type-less lines across the sessions' chat files and 4,071 across the worker events logs (one per opencode/codex/gemini/charlie-code/antigravity run since inception), each still failing WorkerEvent validation on every cold read+transform of its log until the log ages out | every covered backend opened its run with a bare `{"session_id": …}` adopt signal — the chat history's run-start marker (the stable-history projection's interval key, load-bearing since the ordering repair) and the worker log's session-id record (the token tally's codex reconciliation reads the id from the raw line) — whose missing type failed WorkerEvent validation on every cold read+transform of the log (~61 us of pydantic error construction + debug emit per line) and rendered a `type='raw'` row in the workers panel, beside a broadcast frame no subscriber reads; the signal now carries `ET.SESSION_ATTACHED`, the worker projection skips it before row construction, the worker funnel drops its broadcast, and the readers' interval/id keys accept both shapes so old corpora keep ordering and reconciling |
| 2026-09-12 | this PR | M99 server import floor, `import server` (fresh process) median 0.779/0.800/0.799 → 0.737/0.712/0.717 s, −5 % to −11 %, maxima 0.824-0.864 → 0.738-0.771 s, every paired round faster (three interleaved rounds of the new collector — main checkout before vs branch worktree after back-to-back, five timed imports per arm per round, load 0.98-1.51 one-minute; component attribution (`-X importtime`): numpy cum 71.4 ms + src.agents.transcriber cum 139.9 ms on the before arm, both absent on the after arm, src.core.ndjson 249 → 218 µs — the lazy import line is free; the wall delta (~60-85 ms) reads under the transcriber subtree's 140 ms because its src.core.config child is shared with the deps chain the server still pays); M99 definition, healthy range, and collector introduced with this PR; the speech stack's absence pinned by the import-weight contract's new server case | every server start imported the speech stack at module scope — `server.py` imported `src.agents.transcriber` for one background provisioning call and the voice router imported its four names for handlers — paying numpy (~90 ms with its transcriber host) plus the module's ndjson SIMD import on the event loop's startup path, although provisioning runs on a worker thread and transcription only runs when a voice socket opens; the provisioning machinery is now a sync `provision_models` on the thread, the voice handlers import transcriber at their use sites, and the two numpy SIMD scanners (ndjson's line count, sessions' parent-reference frames) import numpy inside their functions |
| 2026-09-12 | this PR | M66 merged build median 4.26/4.31/4.31 → 3.78/3.83/3.88 s, −11 % to −12 %, maxima 4.26-4.34 → 3.80-4.00 s (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, 307.3 MB worst on-disk trace /home/chaoli/data/hayden_243809_traces/step000110/trace_rank008_step000110.json, scratch output under /tmp, live home read-only, every paired round faster at load 1.10-1.25 one-minute; artifact 21.5 MB.gz same size both arms, decompressed-bytes sha256 parity 397ba886c7c4eacf identical; component attribution at load ~1.7: parse alone 2.75 s, walk+pipe-sink 3.81 s — the overlap's floor; a first draft without the pipe resize measured only 4.00-4.24 s, the blocking cost hiding in the pipe not the compress; no-regression witness: M88 direct-pass 2.82 s median on the branch, standing band 2.79-2.85 s, the direct-pass path untouched; 5510-passed suite plus the compressor-reap contract test) | the merged build compressed on the walk's own thread — `gzip.GzipFile.write` deflates inline between batch renders, serializing the 0.3-0.6 s compress behind the GIL-bound walk; the compress now runs in a `gzip -1` subprocess (the M88 direct-pass mechanism) fed over a 1 MB stdin pipe (F_SETPIPE_SZ — the default 64 KB pipe blocked every ~150 KB batch flush on the compressor's drain latency, which is what the first draft paid), so the compress hides under the walk and the build lands on the walk+pipe floor |
| 2026-09-12 | this PR | M4 healthy range median < 300 s → < 600 s (docs-only calibration, no code change). Standing collector, verbatim: `195 user->master_done turns in last 24h: median 326s, max 6822s; 0 running sessions with last event older than 1h` (load 1.14/0.87/0.78) — the first 24 h window to cross the 300 s line. The nine prior rolling 24 h windows (one per day, oldest first): medians 234/178/144/181/220/33/147/182/187 s over 149/243/101/64/27/10/97/115/131 turns, all under the line; the climb tracks turn count (10–243/day) not a code change. Decomposition: a cron iteration's master turn walls include its delegation's worker+review+merge wait, and the code-side share of the turn wall already sits at its measured floors (M31 finalize read 0.8 ms, M52 append at the fdatasync floor, M74 turn-end rescan at the orjson parse floor) | the seed day's 53 s median priced a human-driven workload; the bot's own cron loops (latency-perf, code-health, improve) now generate most turns and their wait-heavy shape moves the median, so the line tripped on legitimate work. < 600 s clears the heaviest observed window (328 s) with 1.8x headroom while the p90 (2147 s) and the hung = 0 count stay the sharp tripwires a stuck loop or a finalize regression cannot pass |
| 2026-09-12 | this PR | M98 memory-CLI invocation wall, `charliebot memory query --topic charliebot --index` median 0.376/0.372/0.379 → 0.249/0.245/0.244 s, −34 % to −36 %, maxima 0.382-0.384 → 0.249-0.259 s, every paired round faster (three interleaved rounds of the new collector — main checkout before vs branch worktree after back-to-back at load 1.45-1.57 one-minute, checkout resolved cwd-first per arm, live store read-only; component attribution (`-X importtime`) on the before arm: src.cli.memory cum 291 ms, of which src.core.memory_replay 53 ms + memory_replay.compare 52 ms — the replay-curation stack the replay/experiment/compare verbs run, imported eagerly for the parser's mode choices — and structlog 100 ms (structlog.dev — rich.traceback, structlog.tracebacks, rich.pretty) for src.core.memory's module logger, touched only on the memory-dir-missing error path; the after arm's src.cli.memory cum 181 ms with structlog and memory_replay absent, constants 0.3 ms carrying the REPLAY_MODES tuple, config 161 ms (models 51 ms) kept by the get_config module-attribute contract and every verb's config read; M92 no-regression witness on the branch: schedule-trigger --help 0.045 s median (standing band 0.044-0.047 s); M98 definition, healthy range, and collector introduced with this PR) | every memory query the cron instructions and master turns issue on demand paid the replay-curation stack and structlog.dev at import although the read verbs (query/add/lint) touch neither; the parser's mode choices now single-home in stdlib-only src.core.constants (the #1412 constants pattern), the replay/experiment/compare verbs import their stack inside the verb path, and src.core.memory's module logger is the same forwarding proxy the M92 structlog landing gave config — the resolved shape, identity, and monkeypatch surface unchanged |
| 2026-09-12 | this PR | M55 first compare-view 0.1751/0.1774/0.1775 → 0.1496/0.1483/0.1468 s, −15 % to −17 %, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, 1.5 MB worst artifact pair understanding_packed-batch-cost-balance_v10.html vs _v9.html of session 8e0ba3ee, shared scratch snapshot home, live home read-only, load 2.24 one-minute; served bodies 150609 B byte-identical across all six arms, digest be3110683106; repeat-view unchanged within noise 0.0023-0.0025 s both arms — the memo hit path untouched by design; component attribution, plan_diff.annotate standalone on the same pair: 165.6/168.4/168.1 → 132.7/146.3/144.0 ms, −13 % to −21 %, byte-identical output); the first-view sub-metric definition and healthy range introduced with this PR | the annotate's final step re-parsed the whole spliced page with the full DOM-building parser — ~26-30 ms of the 165 ms wall, the second full parse of the new page in one compare (base and new in _analyse, then the anchor-only walk and this full parse of the spliced page) — only to locate the .wrap/.main header anchor; the render passes only ever insert bytes (attribute additions inside start tags, synthetic tags and marks at boundaries), so an element's tag bytes stay contiguous under the splice and its spliced-page position is the pre-splice one shifted by the inserted length before it, computed from the insertions bookkeeping the render passes already built (_offset_after_insertions); no pass synthesizes a wrap class (a ghost stamps only cbd-del), so the pre-splice DOM answers the wrap lookup, while the main-tag fallback can diverge from the replaced re-parse when a deleted bare main or body becomes a ghost carrying that tag — unreachable from the artifact pages the route serves, whose shared wrap chrome answers the lookup first, and there the header lands outside the deleted ghost (pinned by test); the head/body anchors keep their re-parse — the render can rewrite the body start tag itself — on the anchor-only parser; 5505-passed suite plus the reparse-oracle parity test (fixture pair + 300 wrap/main-chromed fuzz documents, computed offset == re-parsed start_end on every capture reaching the anchor) |
| 2026-09-12 | this PR | M97 plan-CLI command wall, `charliebot plan list --session <sid>` median 0.715/0.720/0.715 → 0.307/0.317/0.358 s, −50 % to −57 %, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back at load 1.37 one-minute, checkout resolved cwd-first per arm, the 15.4 KB worst plans.json of session d321b9ad, live GET read-only); component attribution (`import src.cli.plan` standalone): 0.548 → 0.052 s — plans.py dragged src.core.artifact_check (522 ms: backends.registry 486 ms incl. numpy via runs and fastapi via tui) for one `require_plan` import, plus src.core.models (148 ms) for the two argparse choices tuples and sessions/config/json_utils for annotations and the verb-only writer; the chain now loads stdlib-only modules plus memo/plan_paths/sidebar_state, the artifact-check import rides a same-name lazy delegate executed only inside the validation path's to_thread hop (the server's verb shape unchanged — its first amend after a process start pays the import inside the worker thread, the M73 cold pass, untimed), the verb-only utc_now and write_json_atomically lazy-import at their call sites, and the two vocabularies single-home in stdlib-only src.core.constants (the #1412 constants pattern) with the models Literals the type home and a parity pin; M92 no-regression witness: schedule-trigger --help 0.042 s median on the branch (standing band 0.044-0.047 s); 5500-passed suite plus the plan-chain import contract and the parity pin | every plan command — the master's plan-delivery verbs and every registry inspection — paid the server's whole validation stack at import although only the server-side validation path runs it; the chain now imports the constants home the M92 landing created and defers the heavy stacks to the sites that execute them |
| 2026-09-12 | this PR | M94 streamed replay serialized median 33.7/33.7/33.7 → 6.3/6.3/6.3 MB, −81 %, dumps wall median 65-67 → 24-25 ms, −63 %, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, 693-event worst active live corpus of session 9313ed43 with the 1.11 MB tool output, live home read-only, load 1.65 one-minute; page body 0.88 MB and dumps 1.7 ms and build 0.9-1.0 ms identical across arms — the projection's page path untouched); no-regression witnesses on the branch: M38 fan-out final-frame parity True, M45 catchup replay digest 314dfbe9fd89 identical with loop-lag 0.0068 s at the 5 ms ticker floor, M26 advance 0.14 ms parity True digest e94c56635194, M6 append-round 0.06 ms parity True; 5496-passed suite plus 607-passed node suite and 3 new stream-shape tests | the stream delta's draft carried every buffered tool's full render-capped output (20 KB per output, this metric's own cap) plus uncapped input values — 80 % of the replay's serialized bytes (25.8 of 32.2 MB measured on the pre-fix arm's composition check) — re-serialized per delta and shipped per coalesced wire frame although the streaming bubble paints content and thinking only (paintStreamDraft never reads draft.tools); the stream shape now trims each tool row through the renderer's own preview bound (TOOL_PREVIEW_CHARS = 500, the M96 payload trim's cap, single-homed in the aggregator beside TOOL_OUTPUT_RENDER_CAP with the bootstrap payload builder): the renderer displays an output's first 500 characters and reads from an input only a bounded summary (a Bash command 80 chars, other named tools 60, file tools their path or pattern), so string tool content over the bound never renders from a wire shape; the committed message keeps TOOL_OUTPUT_RENDER_CAP and the persisted event the full text |
| 2026-09-12 | #1412 | M92 CLI invocation wall, `charliebot schedule-trigger --help` median 0.251/0.251/0.247 → 0.047/0.047/0.044 s, −81 % to −82 %, maxima 0.260-0.275 → 0.045-0.056 s, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after, back-to-back at load 1.54-2.15 one-minute, checkout resolved cwd-first per arm); `plan list --session <sid>` (a GET against the live server) 0.73/0.74 → 0.74/0.74 s unchanged within noise — plan.py's own module chain (`src.core.models`, `src.core.plans`) still imports the model stack eagerly for its registry work, so the request plus its own imports dominate and the deferral's saving is the argparse floor and the no-config paths the metric defines; component attribution (`-X importtime`): `src.core.config` cum 178 ms of the src.cli.common 189 ms chain (pydantic 43 ms — asyncio 23 ms riding core_schema — yaml 12 ms, src.core.models 57 ms) plus the module's direct `SESSION_ID_ENV_VAR` import re-pulling models; 5488-passed suite plus `src.core.config`/`src.core.models`/`pydantic` in the import-weight contract's HEAVY_MODULES; no-regression witnesses: the CLI restart-contract suite (patches `common.get_config` / `cli_module.get_config` — the forwarders keep both module attributes live patch targets) and the version-skew suite; the server never imports `src.cli` | every `charliebot` invocation — the master's and workers' several per turn — paid config's import chain (pydantic models + yaml) although the argparse and `--help` paths never read config; the same deferral pattern the requests (105 ms), structlog (67 ms), and backends.base (+123 ms) landings applied to their chains now covers the largest remaining slice — config loads on first call through `get_config`/`get_credentials` forwarding defs, and the three constants the argparse layer needs (`SESSION_ID_ENV_VAR`, `MAX_TRIGGER_MESSAGE_CHARS`, `WatchKind`) single-home in a new stdlib-only `src/core/constants` re-exported from `src.core.models`, so every model-layer importer is unchanged; M92 healthy range recalibrated < 0.40 s → < 0.10 s with this PR |
| 2026-09-12 | this PR | M88 direct-pass build median 3.72/3.75/3.68 → 2.85/2.84/2.83 s, −23 % to −24 %, maxima 3.72-3.76 → 2.84-2.87 s (three interleaved rounds of the collector-equivalent harness — main checkout before vs branch worktree after back-to-back, 307.3 MB worst on-disk trace /home/chaoli/data/hayden_243809_traces/step000110/trace_rank008_step000110.json, scratch output under /tmp, live home read-only, every paired round faster at load 1.03-1.55 one-minute; artifact 23.8 MB.gz same size both arms, decompressed-bytes parity True every round; verbatim standing collector on the branch 2.79 s median; component attribution on the before arm: read 0.19 s + orjson validation parse 2.72 s + serial stream-compress 0.91 s — the compress fully hidden under the parse after, the parse the remaining floor; the shared-pool draft was rejected pre-push: the spawn pool broke the stdin-driven standing collector and every caller whose `__main__` is not an importable file (BrokenProcessPool from `python - <<EOF`), while the gzip subprocess imposes no main-module constraint and no pool contention with merge builds); no-regression witness: M66 merged build 4.26 s median / 21.5 MB.gz on the branch (standing band 4.3-4.7 s), the merge path untouched; 5486-passed suite; M88 healthy range recalibrated median < 6 s → < 3.5 s with this PR | the direct-pass build ran its two independent passes serially — a full orjson validation parse whose result is discarded, then a re-read + stream-compress of the original bytes — although the parse holds the GIL for its whole run (measured: a concurrent gzip thread makes no progress), so the compress can only overlap from outside the process; the build now starts a `gzip -1` subprocess over the source and parses while it runs, dropping the wall to the parse's, with the temp-artifact + os.replace contract unchanged, a parse error killing the compress and propagating first, and a gzip failure raising with its stderr |
| 2026-09-12 | this PR | M72 changed-round rebuild median 8.32/8.16/8.06 → 6.33/6.01/6.30 ms, −22 % to −26 %, maxima 8.33-8.77 → 6.64-7.24 ms (three interleaved rounds of the new changed-round collector — main checkout before vs branch worktree after back-to-back, 1159-entry sessions root, live state read-only, every paired round faster at load 1.34-1.69 one-minute; component attribution on the pre-fix rebuild, cProfile over 20 rounds: `_format_mtime` 2.0 ms re-formatting 1159 mostly-unchanged mtimes, the row f-strings + escape fast-path + sort-key lambda ~2.5 ms — the per-entry row memo re-renders only the moved entries and serves the rest as strings); the standing repeat-view reading unchanged within noise 7.09/6.89 → 6.87/7.18 ms with the served body byte-identical across arms, sha1 671ee67e152a — the hit path is untouched by design; M72 definition stale no-memo sentence corrected and the changed-round sub-metric + collector introduced with this PR; 5484-passed suite plus the rebuild-parity test | a rebuild after any corpus move re-rendered all ~1159 rows — sort keys, escape checks, size text, and a civil-from-days format per entry, 2.0 ms of it re-formatting mtimes that had not moved — while the moved corpus differs by the one entry a metadata rename touched; rows now memoize on the entry tuple plus the URL prefix the href embeds (the same walked-state ground the page memo's key stands on), so a rebuild re-renders only the moved entries and joins the rest, byte-identity pinned by the same pure-function-of-the-key contract the reference-walk test pins |
| 2026-09-12 | this PR | M92 CLI invocation wall, `charliebot schedule-trigger --help` median 0.334/0.322/0.300/0.304/0.292 → 0.256/0.237/0.243/0.234/0.227 s, −22 % to −30 %, every paired round faster (five interleaved rounds of the verbatim collector — main checkout before vs branch worktree after, back-to-back at load 1.16-1.82 one-minute, checkout resolved cwd-first per arm); real common-family command corroboration, `plan list` (a GET against the live server): 0.78/0.75 → 0.68/0.71 s wall, both paired rounds faster — the request dominates the remaining wall; component attribution (`-X importtime`): the CLI's own top-level `import requests` cum 105 ms of the src.cli.common 278 ms chain (urllib3 63 ms, charset_normalizer 24 ms), reachable by no code path before the first real request — argparse exits at --help; 5481-passed suite plus `requests` in the import-weight contract's HEAVY_MODULES; the e2e restart-recovery suite caught the first draft (PEP 562 `__getattr__` serves external attribute access only, the module's own global lookups raised NameError) — the shipped form keeps the `__getattr__` for the tests' `src.cli.common.requests.*` patch targets and adds module-local imports at the three runtime use sites | every `charliebot` invocation — the master's and workers' several per turn — paid requests' import chain although only request-path functions touch it; the import now rides those functions (a sys.modules hit per call after the first), the module attribute resolves lazily so the patch-target contract is unchanged |
| 2026-09-12 | this PR | M7 reading validity restored: live `/token-usage` 500ed on every load from 2026-09-11 21:43 to this round — 53 `jinja2.exceptions.UndefinedError: 'dict object' has no attribute 'charlie-bot'` tracebacks in the 41 h server log, 5 per hourly round (each round's own M7 collector timed the 500s; this round's sweep read median 0.014 s, max 0.327 s of that shape) → the same page serves 5/5 status=200, median 2.0 ms, max 2.7 ms through a TestClient on the branch with the old-process context shape (three-clause scale sentence, the charlie-bot clause absent; verbatim before curls against the live server read 5/5 status=500, 61-307 ms, load 0.89-1.02 one-minute); the live page heals at merge without a restart — the serving process's Jinja auto-reload reads the template from the checkout it runs from, and its python regains the fourth clause at the next restart; 5479-passed suite plus the skew-shape route test (red on main's template with the live UndefinedError) and the four-clause pin on the labels test; M7 collector asserts 200 now (curl status+time pair, awk fail-loud on any non-200), so a down page can never again read as healthy latency | 0794f81a added the charlie-bot source and a scale-sentence clause indexing `ctx.per_src["charlie-bot"]`; the live server process (started 2026-09-10 12:42) predates it while the template auto-reloads from disk, so the old python's three-source per_src met the new template's fourth lookup and every render raised — the page was hard-down for every visitor for the window; the sentence now renders one clause per source the serving tally's per_src carries, in the fixed display order, so a template-newer-than-python window degrades to a three-source page instead of a 500 |
| 2026-09-12 | this PR | M96 switch-bootstrap body over the 25 active sessions: median 244912 → 97465 B, −60 %, p90 815424 → 229316 B, −72 %, max 1066055 → 371036 B, −65 %, total 9274862 → 3261927 B, −65 % (live-before GETs against the running server vs scratch-after GETs through a TestClient on the branch checkout, scratch CHARLIEBOT_HOME holding the same 25 sessions' metadata + data with master_runs excluded, live home read-only; 0 mismatches outside the trim contract — every message's non-tools fields byte-identical across arms, every trimmed tool a strict prefix with its marker set); live-log attribution joining each of 869 completed `diag_switch` client reports to its bootstrap server line (40 h server log): server share median 6 % (14 ms of 164 ms), client transfer+parse+mount 94 % — the body's tools arrays read 95 % of the pre-trim body (output 3.31 MB + input 3.42 MB across the 25 bootstraps' 2440 tools against 54 KB of content), while the renderer displays only a bounded preview (output's first 500 chars, input's summary line — 80 chars for Bash, 60 for other named tools, the full file path/pattern for the file tools) inside a block hidden behind the "N tool calls" toggle; switch elapsed median 164 ms, p90 386 ms (the after numbers for the elapsed ride the next deploy's live telemetry — the collector's client-side half cannot move the running server); 5478-passed suite plus 2 new payload contract tests (trim over cap with markers, ≤500 untouched by identity) and the 3-test node note suite; M96 definition and healthy ranges introduced with this PR | the bootstrap payload — the SPA switch's fetch and the index page's embedded SESSION_BOOTSTRAP alike — shipped every tail tool's whole input and output — a 40-message tail weighed 245 KB median / 1.07 MB max — although the renderer displays only a bounded preview inside a block hidden behind the "N tool calls" toggle, inside turns that mostly render folded; the payload now caps each tool's output and each input string field at 500 chars (the renderer's own output split, so a capped output renders plain with the existing truncation note and no dead reveal toggle; a long path/pattern summary the renderer would have shown in full trims behind the input_truncated note), marks output_truncated/input_truncated, and copies only messages that actually trim (the projection memo's dicts stay shared with the events pages and the M26 digest, re-read byte-identical after the payload build); full text stays on the persisted chat event where the raw download and the review scans already read it |
| 2026-09-12 | this PR | M92 CLI invocation wall, `charliebot schedule-trigger --help` median 0.351/0.347/0.357 → 0.285/0.291/0.291 s, −17 % to −19 %, maxima 0.358-0.364 → 0.288-0.296 s, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after, back-to-back at load 0.64-0.74 one-minute, checkout resolved cwd-first per arm); `memory query --index` (config-bound, imports no `src.cli.common`) 0.361/0.366/0.367 → 0.356/0.367/0.368 s unchanged within noise — its structlog import rides another module, outside this diff; component attribution (`-X importtime`): config's top-level `import structlog` cum 67 ms of the src.cli.common 302 ms chain, structlog.dev (rich.traceback 32 ms, structlog.tracebacks 28 ms, pygments 20 ms) the bulk — imported eagerly by structlog itself, unreachable by any config-time skip; 5475-passed suite plus the structlog entry in the import-weight contract and the defer-until-first-log probe; no-regression witnesses on the branch: M53 broken steady state onset 1 warning + 1 re-parse / steady 0 + 0 / fingerprint-move 1 re-parse + 0 new warnings and call wall 0.00 ms (the collector's real warning path through the proxy), M58 per-request config read 5.4 µs median | config's module-level `log = structlog.get_logger()` made every CLI invocation pay structlog's own eager structlog.dev import (rich + pygments + the traceback formatter, ~67 ms) although config logs only on warning paths a CLI command never reaches; the module logger is now a forwarding proxy that imports structlog on first attribute use — the resolved shape, identity, and monkeypatch surface unchanged, the cost paid only when a warning path actually fires |
| 2026-09-12 | this PR | M34 full fetch body 9744203 → 251479 B, −97 %; full fetch median 0.0208/0.0202/0.0215 → 0.0025/0.0029/0.0028 s, −86 % to −88 %, maxima 0.0212-0.0218 → 0.0030-0.0033 s, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, 9.8 MB / 232-event worst on-disk worker log carrying one 9.1 MB top-level tool_result line, scratch CHARLIEBOT_HOME per arm, live home read-only; after=total steady poll unchanged within noise 0.0018-0.0022 s, body 39 B both arms); no-regression witnesses interleaved ×2: M13 steady-state read+transform 0.0000 s both arms, M31 events-summary read 0.0008-0.0009 s both arms, M85 verify-final report read 0.6-0.7 ms medians both arms (one 1.3 ms branch round inside the standing band), M95 review scan 0.60-0.64 ms / failed-iteration judgment pair 39.7-40.9 ms both arms; 5468-passed suite plus 3 new contract tests (message-nested cap, top-level tool_result cap, incremental-append parity with the marker) and the node marker-note test; M34 full-fetch body bound added to the healthy range with this PR | the worker-events projection carried each tool_result's full content, so the worst on-disk log's 9.1 MB top-level tool_result line rode every full panel-open fetch whole — 9.7 MB of serialize+transfer+gzip per panel open against the after= poll's 39 B, and the panel embedded the full tail into the row's innerHTML; the projection now caps rendered output at TOOL_OUTPUT_RENDER_CAP (20000 chars — the bound the chat aggregator has applied since the M94 landing) and marks the row `output_truncated`, which the workers panel surfaces as a truncation note; the persisted events log keeps the full text, and the message-nested and top-level tool_result shapes share one cap constant |
| 2026-09-12 | this PR | M91 worst single event 75.8/79.4/80.1 → 13.0/13.6/14.4 ms, −82 % to −84 %, every paired round faster (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, 9.8 MB / 232-event worst on-disk worker log carrying one 9.5 MB tool_result line, scratch append target, live home read-only, load 1.1-2.4 one-minute; replay wall median 0.0774/0.0791/0.0807 → 0.0144/0.0148/0.0152 s, −81 % to −82 %, replay max 0.0776-0.0820 → 0.0145-0.0163 s; per-event median 5.4-5.6 µs both arms; component attribution on the pre-fix arm, measured standalone on the 9.5 MB event: orjson.dumps 9.1 ms, the `.decode("utf-8")` +24 ms, the `+ "\n"` str concat +26 ms, the `.encode("utf-8")` re-encode +14 ms — ~64 of the 76 ms the str round trip, against the bytes form's 0.42 ms `dumps + b"\n"`; the after arm's residual 13-14 ms is the 9.5 MB line's orjson dumps + page-cache write floor, the same corpus-drift floor the M95 row attributed); no-regression witnesses interleaved ×2: M82 events-log append 2-3 µs medians both arms with the standing collector repaired to build its probe line through the checkout's own `_event_line` (the collector's hand-built str line raised TypeError against the bytes signature — loud failure, the vacuous-read class the #1358 repair called out, avoided); 5466-passed suite; M91 worst-single-event range recalibrated < 1.0 ms → < 0.020 s with this PR | every persisted worker event serialized with orjson to bytes, decoded the whole payload to str, concatenated the newline, and re-encoded the str back to bytes for the write — three full payload copies plus two transcode passes per event, paid by every streamed delta, tool use, tool result, and thinking event on the append the #1327 landing moved on-loop; the line now stays bytes end to end (`orjson.dumps(event) + b"\n"` straight into the shared write-all), byte-identical output since orjson's compact UTF-8 form is exactly what the decode+encode round trip reproduced; M82's collector repair rides this PR (collector command only) |
| 2026-09-11 | this PR | M95 review-completion worker-summary scan median 48.9/49.3/51.2 → 0.61/0.61/0.59 ms, −99 %, maxima 50.2-51.9 → 0.62-0.72 ms; failed-iteration judgment pair median 49.7/50.3/51.9 → 40.8/39.5/39.5 ms, −18 % to −24 %, maxima 59.3-59.6 → 43.3-45.4 ms (three interleaved rounds of the new collector — main checkout before vs branch worktree after back-to-back, 9.8 MB / 232-line worst on-disk worker log carrying one 9.5 MB tool_result line, live home read-only, resolved summary/blocker/iteration-summary identical across all six arms, every paired round faster at load 1.47-1.51 one-minute; component attribution on the pre-fix arm: the whole-log parse's orjson floor on the 9.5 MB line measured 35-37 ms bare (parse_ndjson_line, bytes and str alike) and the text-mode read+split 17.6 ms against 7.5 ms bytes; the improve success path (report present, thread not failed) drops the parse entirely — the unconditional `parse_ndjson_file` served only the failure/fallback branches; no-regression witnesses interleaved: M31 steady-state events-summary read 0.0007 s both arms, M85 verify-final report read 0.7 ms both arms with the report text identical, M76 finalize judgment pair 0.00000 s both arms, M80 changed-round 0.157/0.152 s with rows digest 505521008044 identical across arms, M12 codex usage scrape 0.0001 s both arms, M78 untouched-funnel parse 42.6 ms (its standing reading); 5467-passed suite plus 5 new contract tests — the newest-first scan pins, the malformed-tail skip, the multi-window big-line walk parity, and the fused both-settle pass) | every reviewer completion and every improve iteration full-parsed the worker's events log to scan it newest-first — review.py's worker-summary scan and improve_command's unconditional parse feeding three reversed() walks — while the from-the-end walk (the M85 generator) already serves this shape: the scans now stream the log from the end and stop at the first answer, the failed path's two judgments share one pass that stops once both settle, and the from-the-end walk itself carried the corpus-drift trap the tail-follow funnel's landing fixed earlier today: a line longer than the 512 KiB window re-concatenated its accumulated carry every window — O(line × windows), the 9.5 MB line × 19 — so the walk now accumulates one window-piece per step and joins once at the line's closing newline (60-randomized-trial parity against the full parse, big-line position varied); the M78 worker-log reading this round (42.6 ms, unchanged by this diff) rides the same corpus drift with the funnel already at the orjson floor — its range recalibration is carried by this PR's evidence |
| 2026-09-11 | this PR | M7 changed-round collect median 0.823/0.839/0.830 → 0.226/0.285/0.168 s, −66 % to −80 %, maxima 0.879-1.241 → 0.232-0.306 s (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, live corpus read-only, every paired round faster at load 3.0-4.4 one-minute; rows 20 both arms, the quiet round's rows digest f717b41a6d96 identical, rounds 2-3's digests move with the live corpus between arms — the live server appends to its own thread logs constantly); warm whole-tally hit median 762/770/1035 → 143/138/156 ms over 7 (three interleaved rounds, every paired round faster, rows 20 all arms); M80 churn changed-round wall 0.909/0.754/0.807 → 0.167/0.180/0.164 s, −79 % to −82 % (three interleaved rounds of the verbatim collector, rows digest b9da8204c7cc identical across all six arms); primed restart-cold 1.638/1.046/0.993 → 1.481/0.847/0.888 s (three interleaved rounds, every paired round faster — the opencode db scan dominates the shape, the walk saving rides underneath); the standing live-document restart-cold stays 8.2 s on both arms until the live server deploys and persists the charlie-bot entries (the new source's one-time cold build, the transition shape #1278's opencode row-map landing documented); 5395-passed suite plus the new memo contract test | #1352's charlie-bot source joined every collect's signature walk and serve walk — two full passes over the 2.3 GB / 20,478-directory sessions tree per changed round (~0.9 s of walk) and one pass per warm whole-tally hit (~0.8 s against the 16-21 ms pre-feature standing band), with the serve walk re-traversing the identical tree the signature pass had just proven; the corpus is now walked once per collect and the rows both consumers read, and each walked directory's listing memoizes on the directory's own (mtime_ns, size) — one stat validates a remembered listing, since an entry's create, delete or rename moves the containing directory's mtime_ns, while a file append moves only the file's mtime, which the walk's per-file stat takes every round — so a repeat walk pays ~29k stats instead of ~46k scandirs; M80's healthy range recalibrated < 0.020 s → < 0.30 s with this PR — the range predates the charlie-bot source, whose memoized walk floor on this host is ~0.1 s |
| 2026-09-11 | this PR | M89/M90 collector repair: since #1346 (15:37 today) renamed the two per-chunk log writes as one `base._write_chunk`, both standing collectors' `getattr(…, None)` dispatches missed the new name and silently timed the pre-fix aiofiles fallback — this round's sweep read M89 tee 178 µs, M90 chunk 180 µs / startup line 417 µs medians (re-runs 173-212 / 409-467 µs at load 1.75 one-minute), the #1303 landing's before numbers, and every M89/M90 reading in that window proved nothing; repaired commands read, three rounds back-to-back at load 1.72-1.90 one-minute: M89 tee 89/91/83 µs, M90 chunk 91/96/81 µs, line 87/96/71 µs — the one to_thread round-trip floor the #1303 landing documented, all inside the standing < 200 µs ranges, maxima 0.5-3.4 ms the scheduler jitter the M91 row classifies; collector commands only, no product code | the getattr-plus-aiofiles dispatch existed for the #1303/#1288 before/after arms and became a trap once the helper moved under it: the fallback has no failure mode, so a renamed helper silently re-prices a removed shape (the #1285 vacuous-read class, the M53 repair's precedent); both collectors now read the helper as a direct attribute — a checkout whose base module lacks the name fails the collector loudly instead of timing a shape the tee and pumps no longer run — and the dead aiofiles arms are gone |
| 2026-09-11 | #1348 (row recorded in this docs-only follow-up per the #1046 precedent, the landing PR shipped without it) | M68 marked changed-poll rebuild median 7.09/7.60/7.76 → 3.78/3.87/3.92 ms, −45 % to −49 %, maxima 7.75-8.84 → 4.60-5.29 ms (three interleaved rounds of the repaired collector — main checkout before vs branch worktree after back-to-back, 499-row / 4393 KB worst on-disk thread-metadata corpus of session dfe393f7, scratch CHARLIEBOT_HOME per arm, live home read-only, body byte-identical 179620 B across all six arms, every paired round faster at load 1.01-1.09 one-minute; no-regression witnesses interleaved ×2: M36 full poll 2.06/2.12 → 2.11/2.17 ms with body identical, conditional 204 1.92-2.13 ms both arms, M63 /view 1.02/1.14 → 1.01/1.03 ms with body 193415 B identical; 5124-passed suite plus 5 new contract tests — incremental body byte-identical to the full walk's etag included, vanished marked file drops the row, the sweep lands within 10 continuously marked polls, a path-less mark full-walks, and the marked-path cap drops the whole burst) | the marked round re-walked every row-source file (one stat per thread metadata) and re-parsed/re-serialized the whole body to find the one file the writer had just published; the writers' marks now carry the published path (mark_sidebar_dirty(session_id, path), post-rename), and the poll proves its stored body by stat-ing exactly the marked paths, patching the stored signature, and rebuilding rows from the row memo plus a parse of each moved file, with the every-10th-poll full walk kept on schedule by the incremental proof advancing the sweep countdown; the same PR repairs the standing collector, which had never driven a rebuild — it overrode deps.get_config while the endpoint resolves get_config_on_loop, so the endpoint walked the live home, the scratch rewrites were invisible, and every timed request was walk-plus-memo-serve (the vacuous-read class; this round's pre-fix sweep read 8.63 ms of that shape, and the reviewer's finding on the marked-path cap — clear-then-add leaving a one-path set that proved the burst's revision — fixed in the landing's second commit); M68 healthy range recalibrated < 0.007 s → < 0.005 s with this PR |
| 2026-09-11 | this PR | M84 stdout-stream replay median 36.8/35.8/36.1 → 12.7/12.9/13.3 ms, −63 % to −65 %, maxima 37.2-39.3 → 12.9-14.1 ms; tail-follow replay median 39.1/39.5/38.8 → 36.8/37.8/37.7 ms, −4 % to −6 %, maxima 40.0-40.1 → 37.2-38.6 ms (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, 10.1 MB / 64-line worst on-disk raw agent log whose single 9.99 MB observation line parses in ~12.5 ms bare, scratch copy, live home read-only, parser parity 0 divergences both arms, every paired round faster at load 1.4-1.7 one-minute; component attribution: the removed cost is the per-line `errors="replace"` decode — 6.0 ms on the 10 MB line — while `strip()` already returns the original object for a clean line and orjson parses str and bytes alike, so the str-mode readers move only where bytes lines flowed through a pre-decode); no-regression witnesses on the branch: M78 whole-file parse unchanged 51.6/52.4 → 51.4/49.9 ms (interleaved ×2, the str reader), M13 steady state 0.0000 s, M26 advance 0.18 ms parity True digest e94c56635194, M34 full 0.0055 s / after=total 0.0018 s (40 B), M52 append at the fdatasync floor; 5112-passed suite plus 4 new bytes-contract tests | every spawned-stdout and tail-follow stream line paid a full `errors="replace"` decode before the parse although orjson parses the wire's UTF-8 bytes natively — the decode rode both backend stream funnels (the live read side of every codex/opencode streamed turn and the raw-log tail-follow loop, the same pre-decode shape parse_raw_lines shed at its M74 landing); the strict bytes parse is now the fast path inside the skip contract's one home (parse_ndjson_line), with the replace decode demoted to the fallback that decides torn multibyte (the U+FFFD contract preserved and pinned by new tests), and both funnels feed raw bytes |
| 2026-09-11 | this PR | M94 page body median 16.14/16.14/16.14 MB → 0.84/0.84/0.84 MB, −95 %, page dumps 36.7/35.0/34.6 → 1.8/1.9/1.8 ms; streamed replay serialized 627.2/627.2/627.2 → 20.2/20.2/20.2 MB, −97 %, dumps wall 1390/1387/1386 → 45/44/44 ms (three interleaved rounds of the new collector — main checkout before vs branch worktree after back-to-back, 531-event / 16.2 MB live chat file of session 4914c102 whose largest single tool_result carries 9.92 MB of Bash output, live home read-only, every paired round faster at load 1.25-1.46 one-minute; live-log corroboration: that session's bootstraps read 342-890 ms across the 25 h server log — the 16 MB page's dumps+gzip+transfer — and its raw events.jsonl downloads 1.5-1.8 s, shapes the capped page bounds); no-regression witnesses interleaved ×2: M38 fan-out unchanged (same selected turn, 186 dumps calls 5 ms, final-frame parity True both arms), M26 advance 0.12-0.18 ms with parity True digest e94c56635194 identical, M35 view/bootstrap bodies byte-identical (digests ff7b6850b70d / e153e88533ab) and the events page digest moving 68668edc2776 → 776c27d41c44 (624733 → 624218 B — one barely-over-cap output's trimmed tail, the cap's only served-body change); 5094-passed suite plus 4 new cap tests | every render path re-serializes the aggregator's whole buffered draft — each page payload (bootstrap, events, view) and each `stream` delta snapshot — so one big tool_result rode every subsequent delta and every switch back to the session: the corpus replayed through the live broadcast shape serialized 627 MB across 356 deltas (1.4-1.6 s of event-loop json.dumps per replay) and its projection page shipped 16.14 MB per bootstrap; the aggregator now caps each tool's rendered output at 20000 chars with an `output_truncated` marker the renderer surfaces (the expand still reveals the capped tail; 99 % of on-disk outputs — p99 26016 chars — keep their full text, the cap bounds only the top 1 %), and the full text stays on the persisted event where the raw download, the fork reference, and the review scans already read it; M94 definition and healthy range introduced with this PR |
| 2026-09-11 | #1336 (row recorded in this docs-only follow-up per the #1046 precedent, the landing PR shipped without it) | M93 thread-detail 500s: 9 per 24 h server log (all `error=AttributeError`: 5 `'CharlieCodeBackend' object has no attribute 'cli_binary'`, 4 `'OpencodeBackend' object has no attribute 'cli_binary'`, across 5 distinct worker threads of 4 sessions — session fcef4323's thread 956e07e4 alone 500ed five times, the cadence of an open panel's 5 s attach poll) → the fixed code serves the same scratch shape 200/200; the live server carries the fix from its next deploy, so the standing count reads the pre-fix log until then. Same PR's M11 collector repair: the 500 count had grepped the uvicorn access-log shape the server stopped writing at the 2026-09-07 09:20 restart (the restart that turned on the structured `http_request` lines) — `HTTP/1.1" 500` matches 0 lines in every log since (the newest 24 h log carries 9 structured `status=500` lines) — so the count read 0 unconditionally for ~4 days, the #1285 vacuous-read class; the repaired pattern reads `path=/api/backlog status=500` (still 0 — the nine 500s are thread-detail, outside M11's endpoint set) and `|| [ $? -eq 1 ]` lets the healthy zero count exit 0 instead of grep's no-match 1 while a real grep failure (exit 2) stays loud; both GETs 200. Scratch A/B: the detail GET against a one-charlie-code-option config 500ed with the live traceback's own frame (`_backend_dispatch`, line 93) on main's code and serves full-row 200 + attach-mode 200 on the branch; new parameterized endpoint test over all nine backend-option types × both shapes red pre-fix (7 of 9 fail — exactly the seven types without `cli_binary`) and green after; 5090-passed suite; M59 no-regression witness 1.90/1.79 ms medians, bodies 59259/48 B identical to the standing band | the attach dispatch read `option.cli_binary` off the bare backend-option union, but `cli_binary` is declared on the CcClaudeBackend and TuiCliBackend option models only — the dispatch predates the sectioned config giving each backend type its own `extra='forbid'` pydantic model, which is what turned the bare attribute read into a crash; the dispatch now reads through the same type gate the backend registry uses |
| 2026-09-11 | this PR | M84 tail-follow replay median 92.0/95.1/94.7 → 38.9/38.8/39.0 ms, −58 %, maxima 96.8-103.8 → 39.3-39.5 ms (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, 10.1 MB / 64-line worst on-disk raw agent log whose largest event is a single 9.99 MB observation line, scratch copy, live home read-only, parser parity 0 divergences both arms, every paired round faster at load 1.95-2.04 one-minute; component attribution on the pre-fix replay: the per-chunk `buf + chunk` carry concat plus the from-byte-0 terminator re-scan read ~80 ms of the ~92 ms wall — cProfile tottime 38 ms in the loop frame, 17 ms in bytes.find over ~155 chunk rounds — against the 12-14 ms orjson parse floor the 9.99 MB line measures); no-regression witnesses interleaved: stdout-stream replay unchanged 36.0/36.4/35.9 → 35.9/36.3/36.2 ms (the untouched sibling funnel), M74 turn-end rescan loop-lag 0.0205 → 0.0231 s within noise (its parse_raw_lines walk is already linear); 5080-passed suite plus the new 1 MB single-line multi-chunk carry test | the worst on-disk raw log changed shape since the M84 landing (391 lines averaging 25 KB → 64 lines carrying one 9.99 MB line), and the pre-fix carry paid the quadratic twice per chunk round — the accumulated-bytes concat and the from-byte-0 terminator search — so the replay read 8x the landing's 11.2-11.6 ms and outside the < 60 ms range; the carry is now a bytearray appended per chunk and compacted once per chunk with the search resuming at the previously scanned boundary, so a multi-chunk line costs O(line) in the loop that is the live read side of every covered backend's streamed turn and the re-attach replay path; range recalibrations carried by this PR's evidence, both corpus-shape moves the orjson floor drives: M84 stdout-stream < 0.010 s → < 0.040 s (funnel measured at the floor: parse_ndjson_line 12.0 ms on the 9.99 MB line vs bare orjson 12.4 ms, decode 1.0 ms, reader hops 0.0 ms; the old line was calibrated on the small-line corpus whose floor was 4.7 ms per 9.9 MB) and M74 loop-lag < 0.015 s → < 0.030 s (the rescan's threaded orjson parse holds the GIL 12-14 ms on this corpus before the walk and project add) |
| 2026-09-11 | this PR | M91 streamed-turn replay: replay wall median 0.2641/0.2672/0.2290 → 0.0226/0.0222/0.0220 s, −89 % to −92 %; per-event median 105.0/104.2/93.6 → 4.7/4.6/4.5 us, −95 %; worst single event 1.74/1.42/1.43 → 0.45/0.41/0.42 ms, −69 % to −74 %, back inside the < 1.0 ms range (three interleaved rounds of the verbatim collector, main checkout before vs branch worktree after back-to-back, 6.7 MB / 2315-event worst on-disk worker log, scratch append target, live home read-only, load 2.67-2.79 one-minute, every paired round faster); attribution: the worst single event is scheduler jitter, not payload — during the sweep the corpus's 256 B system events spiked to 1.0-3.9 ms while the 234 KB assistant event read 0.62 ms, and a same-loop probe append's own worst hop read 1.35-3.93 ms, so the head was the per-event executor round-trip's wakeup, on top of which the biggest event paid 0.47 ms of stdlib json.dumps; on-loop vs hop microbenchmark on this host: 256 B 1.7 vs 97 us, 3 KB 4.0 vs 119 us, 234 KB 89 vs 217 us, the on-loop worst bounded by the write itself; no-regression re-measure interleaved ×2: M82 events-log append 72/82 → 2/2 us median, maxima 99-121 → 10-24 us (the same function, now write-only); 5070-passed suite plus 2 new round-trip tests | every streamed worker event paid one asyncio.to_thread executor round-trip for its append — the M82 landing's one hop — whose wakeup under load spikes to milliseconds (the worst-single-event readings 2.81 ms / 1.34 ms the last two sweeps carried), while the events log is a page-cached append-only diagnostic stream with no fdatasync, so the hop bought no durability; the append now writes on-loop through the shared write-all (worst case bounded by the write itself, ~90 us per 100 KB), and the persisted line serializes through orjson (the M78 parser-swap precedent: 0.467 → 0.036 ms on the 234 KB event, compact UTF-8 bytes where stdlib emitted spaces and \\uXXXX escapes — every reader JSON-parses per line, the NaN literal the stdlib form could emit is one the orjson read funnels already reject, so null is strictly round-trippable); `_append_event_line` keeps its awaited (fd, line) shape, the M82 collector's contract |
| 2026-09-11 | this PR | M33 replay wall median 0.409/0.398/0.405 s → 0.032/0.033/0.033 s, −92 %, maxima 0.412-0.428 → 0.033-0.033 s (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, 98.0 KB / 100373-byte draft of 538 top-level tokens, 502 deltas at 40 ms virtual cadence, 102 paints, final-frame parity true every round; component attribution on the pre-fix replay: marked re-lex+render 326 ms of the 406 ms paint wall — the whole draft re-lexed per paint, 7.8 ms at the final size against 0.23 ms for a 4 KB tail — and the rest escape/DOM/scheduling; no-regression re-measures interleaved ×2: M54 paint-work 0.101/0.110 → 0.107/0.116 s median with maxima 0.197-0.212 s both arms — the fence-bearing corpus stays on the conservative path, its cuts never qualify behind a code token — and final-frame parity true; M60 repeat-page 0.01 ms both arms, cold 0.037 vs 0.038 s, flush 0.184 vs 0.190-0.194 s; M81 page re-render 1.45 → 1.07 ms, 0 walks, parity true; 5064-passed suite plus the 7-test incremental suite — 360 randomized drafts across three seeds and the real corpus, per-paint byte equality against the whole-draft streaming render, plus the ref-definition reset, the list-cut guard, the line-start cut pin, and the shrink/rewrite reset) | every paint re-lexed the whole accumulated draft — O(paints × draft), the chat UI's remaining main-thread jank during long streamed turns; the parse is now incremental across paints: a frozen prefix whose token stream can no longer change carries its rendered HTML forward and only the tail after the last safe block boundary re-lexes — a boundary is a blank line a list's blank-line continuation or a code block cannot cross, located by true source offsets (token raws do not tile the input: a link reference definition is consumed into tokens.links with no token emitted) and only accepted at a line start (a partially streamed indent inside a space token's raw lexes differently from mid-line than from the line start); the first paint whose new text carries a reference definition re-parses whole from then on (document-wide resolution), a rewritten or shrinking draft resets, and the growing tail's escaped-plain skip rides the same recorder — every painted frame stays byte-identical to the whole-draft streaming render the old code produced; M33 healthy range recalibrated median < 1.0 s → < 0.1 s with this PR |
| 2026-09-11 | this PR | M92 CLI invocation wall, `charliebot schedule-trigger --help` median 0.423/0.427/0.433/0.438 s → 0.338/0.339/0.344/0.347 s, −17 % to −21 %, every paired round faster (four interleaved rounds of the new collector — main checkout before vs branch worktree after, back-to-back at load 0.8-1.1 one-minute, checkout resolved cwd-first per arm); `plan list` (a real common-family command, registry read included) 0.449/0.448/0.449 → 0.358/0.361/0.362 s, −20 %, every paired round faster; `memory query` (config-bound, imports no `src.cli.common`) 0.293 → 0.298 s unchanged within noise — its asyncio rides structlog.stdlib, outside this diff; component attribution on the pre-fix chain (`-X importtime`): `src.cli.common` cumulative 366 ms, of which `src.agents.backends.base` +123 ms (runs → numpy) and `src.core.threads` +17 ms on top of the config+requests core; 5064-passed suite including the new import-weight contract test | every `charliebot` invocation is a fresh process and the master and workers run several per turn; the CLI's shared module imported `SESSION_ID_ENV_VAR` from `src.agents.backends.base` and the thread-layout names from `src.core.threads` at module top, dragging the backend stack (`src.core.runs` → numpy), the sessions stack, and config's top-level `asyncio` import into processes that only parse args, read config, and POST to the internal API; the env-var name moved to `src/core/models.py` beside the constants config already pays for (all four importers updated, one spelling everywhere), the thread names lazy-import at the readback path (the rare sent-but-lost class), and config's `asyncio`/`create_logged_task` imports moved into the cron-alert function's lazy block (the existing no-event-loop skip untouched); requests stays a top-level import so the collector's floor is the floor real commands pay |
| 2026-09-11 | this PR | M91 worker per-event quota-scan head, streamed-turn replay: worst single event 1.36/1.37/1.40 → 0.66/0.69/0.87 ms, −38 % to −53 %, every paired round faster (three interleaved rounds of the new collector — main checkout before vs branch worktree after back-to-back, 6.7 MB / 2315-event worst on-disk worker log, scratch append target, live home read-only, load 0.86-1.29 one-minute; per-event median 84-104 us both arms — the M82 append floor — and replay wall median 0.222-0.270 s both arms, within noise); component attribution, the head the diff gates — the per-event `str().lower()` copies of both payload fields — measured standalone 5.91/6.05/6.12 → 0.22/0.25/0.27 ms per corpus replay, −95 % to −96 %, every paired round faster; 5059-passed suite plus 3 new quota-scan gate tests | every streamed worker event paid a repr of its whole message dict plus lowercase copies of the message and content payloads for the quota-pattern check although only ERROR events can match — the head scaled with payload (the corpus's 642 KB tool_result event paid ~0.7 ms of scan beside its append), so the copies now ride the ERROR gate, the check's own condition, and non-ERROR events skip the stringification; M91 definition and healthy range introduced with this PR |
| 2026-09-11 | this PR | M66 merged build median 4.82/4.79/4.80 → 4.38/4.46/4.35 s, −9 % to −11 %, maxima 4.82-4.85 → 4.35-4.48 s (three interleaved rounds of the verbatim collector — `CHECKOUT` at the main checkout before vs branch worktree after, back-to-back, 307.3 MB worst on-disk trace /home/chaoli/data/hayden_243809_traces/step000110/trace_rank008_step000110.json (1,068,461 events), scratch output under /tmp, live home read-only, every paired round faster at load 1.68-1.92 one-minute; artifact 21.5 MB.gz identical across arms; 5056-passed suite plus the mixed-tid-form contract test; component attribution: the pre-fix build's ~4.8 s reads 0.19 s file read + 2.1 s orjson parse + 1.36 s walk+batch-dumps (no-op sink) + 0.74 s level-1 gzip) | the build's per-event walk paid a `str()` on every tid-bearing event for the str-canonical sequencer probe — 0.16 s standalone on the 1,068,461-tid corpus, the same shape #1151 removed for pid — while the generational GC passes over the ~1M dicts the parse allocates and the walk mutates cost 0.3-0.6 s per build (measured gc-on vs gc-off interleaved ×2); the walk now probes a raw-value tid map beside the str-keyed sequencer map (on a raw miss the str-keyed lookup still answers, so int 7 and "7" stay one thread — pinned by a new contract test) and the build runs with GC disabled inside the spawn-context merge pool, re-enabled with one collect so cyclic leftovers never accumulate across builds in a long-lived worker; M66's corpus has grown 2.15x since the range was set (496,116 events at the 2026-09-09 landing, 1,068,461 today) and stays inside < 8 s with margin |
| 2026-09-11 | this PR | M81 collector command repair: the verbatim command failed from any non-repo CWD (`Cannot find module '<cwd>/tests/katex_walk_collector.js'`, verified from `/tmp` and `/` — the script path was CWD-relative and the `CHECKOUT` prefix assignment it already carried does not feed same-line shell expansion, so the sweep silently lost M81 whenever its runner did not start in the checkout); repaired command runs verbatim from /tmp against both checkouts — main reading page re-render wall 1.03 ms, 0 walks, parity true (corpus sha1 7409bcd20e4c), branch reading 1.01 ms, 0 walks, identical sha1 — inside the healthy ranges, no metric movement | one-word-class fix: `node tests/…` → `; node "$CHECKOUT/tests/…"`, the statement form so the assignment lands before the expansion; collector command only, no product code |
| 2026-09-11 | this PR | M90 stdout pump chunk median 143/155/145 → 124/96/132 us, −13 % to −38 %, and startup line median 356/383/348 → 82/75/89 us, −77 % to −80 %, every paired round faster (three interleaved rounds of the new collector at load 0.82-1.27 one-minute, main checkout before vs branch worktree after back-to-back; component attribution: the after arm sits at the one to_thread round-trip floor — 96-132 us against the ~67-104 us no-op round-trip the M34/M52/M82 rows document — so aiofiles' second hop was the chunk gap and the per-line open+close the line gap); live-scale note: this host's on-disk opencode stdout.log volumes are small (4171 run logs, p90 118 B, max 9.4 KB — 1-2 chunks plus a handful of startup lines per run), so the removed hops are ~0.2-1.1 ms of executor time per opencode run off the pool every poll and chat append shares, and the antigravity envelope pump rides the same helper; no-regression re-measures interleaved ×2: M89 stderr tee 77/78 → 78/79 us, M82 events-log append 81/96 → 72/99 us (the write_all consumers this diff leaves untouched); 5057-passed suite plus 2 new contract tests (the write-all stdout contract, the opencode fd handoff) | the opencode run teed `opencode serve`'s stdout through aiofiles — the streamed pump paid the write+flush pair per 8 KB chunk (two executor round-trips) and the startup wait paid a full open+write+flush+close per printed line (four round-trips) — while the claude-family backends' raw stdout lands through the spawn fd and the stderr tee has ridden one hop since M89; the run now holds one raw O_APPEND fd for the attempt (O_APPEND keeps the lock-retry attempts appending the way the per-line "ab" opens they replace did), both phases write through the shared one-hop helper beside _tee_stderr_chunk, and _cleanup_server closes the fd after the drained stdout task; the antigravity envelope pump rides the same helper with its fd scoped to the run and the "wb" truncate kept; no durability change — the stdout log is a diagnostic stream and carried no fdatasync; M90 definition and healthy range introduced with this PR |
| 2026-09-11 | this PR | M89 stderr tee chunk median 161/162/141/148/150 → 114/127/104/99/91 us, −22 % to −39 %, every paired round faster (five interleaved rounds of the new collector at load 1.18 one-minute, main checkout before vs branch worktree after back-to-back; a quieter first pass read 209 → 93 us, −55 %, and one round of an earlier series landed 173 → 229 us under a load spike the later series excludes); component attribution: the after arm sits at the one to_thread round-trip floor (91-127 us against the ~67-104 us no-op round-trip the M34/M52/M82 rows document), so aiofiles' second hop was the whole gap; live-scale corroboration: on-disk stderr.log volumes 0.6-4 MB per run mean 75-490 chunks per run, so the removed hop is ~7-55 ms of executor time per run off the pool every poll and chat append shares; no-regression re-measure interleaved ×2: M82 events-log append 76/73 → 74/74 us (the sibling consumer of the now-shared write_all); 5-passed backend-logging suite plus the new write-all contract test | every covered backend's run teed subprocess stderr through aiofiles' write+flush pair — two executor round-trips per 8 KB chunk on the default pool — while the read half is a native asyncio stream and the write lands through one asyncio.to_thread hop around the shared write-all loop (the M82 events-append pattern, now single-homed in src.core.ndjson.write_all alongside the fdatasync append's own loop); the open keeps the "wb" truncate so a run's log starts empty for its tail -f readers, the in-memory 64 KB tail update stays on-loop, and there is no durability change — the stderr log is a diagnostic stream and carried no fdatasync; M89 definition and healthy range introduced with this PR |
| 2026-09-10 | this PR | M53 broken steady state, repaired collector: onset 1 warning + 1 re-parse, steady state 0 warnings / 0 re-parses over 60 calls, fingerprint-move round 1 re-parse / 0 new warnings, call wall median 0.00 ms max 0.03 ms (back-to-back arms at load 0.78-0.96 one-minute, scratch CHARLIEBOT_HOME, live home untouched); before — the stale collector read vacuously: onset 0 warnings / 0 re-parses, steady 0/0, fingerprint-move round 0/0 (the broken corpus never broke anything, so every reading since the sectioned config proved nothing); the sweep's other 85 standing collectors all read inside their healthy ranges this round (load 0.66-1.03 one-minute across the sweep) | the 2026-09-09 config-schema series moved the whole sectioned mapping into config.yaml, leaving config.d/ to cron.d/ only: load_config now rejects a config.d/*.yaml fragment outright, and the reload fingerprint stats exactly config.yaml — so the M53 collector's broken-corpus shape (a fragment declaring an unknown key) could never fire the reload it exists to exercise: the fragment is not config, and writing it moves no fingerprint stat; the key now goes into config.yaml itself, restoring the collector's contract — onset 1 warning + 1 parse (the warn-once registry's first sighting), steady state 0/0 on the recorded failed fingerprint, and a fingerprint move re-parses once with no new warning; collector command only, no product code |
| 2026-09-10 | this PR | M7 restart-cold collect median 2.58 s → 0.93 s, −64 %, maxima 2.56-2.67 → 0.43-0.96 s (three interleaved prime+timed rounds of the new collector — each arm primes its own document seconds before its timed run, main checkout before vs branch worktree after back-to-back, live corpora read-only during an active turn's churn at load 2.0-2.1 one-minute; scanned bytes 58.0 → 0.0 MB — the db's whole 121k-row data-blob corpus re-read per restart vs only the rows that moved since the document was written; rows digests agree across 4 of 6 arms, the drift is the live turn appending between arms); component attribution on the pre-fix arm: `_scan_opencode_rows` 1.95 s of the 2.84 s collect (the json_extract pass measured standalone 1135 ms over 121k rows, `_opencode_row` parse 0.59 s, replay fold 0.26 s); no-regression re-measures interleaved ×2: M7 changed-round 0.041/0.041/0.041/0.042 s medians (verbatim harness, rows digest identical), M80 churn 0.0037/0.0030/0.0032/0.0032 s with rows digest 8efb9506fc07 identical across all four arms — the v2 document's orjson save rides those rounds (dump 28 ms vs stdlib 176 ms measured on the 20.8 MB document, which the rows map grows from 14.7 MB); 5043-passed suite plus 5 new tests (seed+diff blob-free round, moved-row recount with insert+in-place-upsert deltas, stored-partial adoption without replay, v1 records entry serve, NaN document cold-rebuild note) and 2 re-pins (the persisted entry shape, the stored-partial adoption contract) | the persisted document held the opencode db's records but not their row keys, so a process restart — the doc's whole purpose — could not tell which rows had moved and re-read every contributing row's data blob through the json_extract scan (2.26 s of the 2.84 s collect, once per server start); the entry now persists the row memo's map (id → [time_updated, record]) plus the partial, and the restart-cold advance seeds the memo from it and diffs one key pass against the live table — the same per-row diff the warm incremental path runs, so the restart cost drops to the key scan plus the moved rows' fetches; the document reads and writes through orjson (the M78 parser-swap precedent, machine-written JSON, load 237 → 177 ms and dump 176 → 28 ms on the grown document, NaN literals now fail loud into the existing unreadable-document cold-rebuild note), v1 documents still serve through the records replay until their first scan-path store rewrites them; M7 restart-cold definition, collector and healthy range introduced with this PR |
| 2026-09-10 | this PR | M19 framing median 18.20/17.90/17.88 → 3.00/3.27/3.28/2.94/3.14/3.10 ms, −82 % to −84 %, maxima 17.98-18.78 → 2.99-3.48 ms (six interleaved rounds of the collector — main checkout's str path before vs branch worktree's byte mode after, back-to-back, 16 MB payload / 16 KB chunks / ~1 MB frames, 32 lines both arms, synthetic read-only, every paired round faster at load 1.42/1.39/1.12 one-minute; component attribution: the per-chunk UTF-8 decode the str path paid measured standalone at 8.69 ms per 16 MB; str-mode no-regression witness: branch 13.24/12.84 ms vs main 18.03/17.85 ms interleaved ×2 — the per-line decode replaced the incremental chunk decoder and is itself cheaper); 5036-passed suite plus 8 new byte-mode tests (mode parity on every two-way split and 50-round random chunkings ×3 corpus shapes, multibyte split reassembly, raw splitline-boundary bytes, unterminated-tail and trailing-CR flush, invalid-UTF8 raw pass-through with the parse-side raise pinned) | the framer decoded every byte chunk to str on the event loop before both SSE consumers immediately JSON-parsed the completed lines — orjson parses the wire's UTF-8 bytes natively, so the decode was pure overhead on the funnel that carries every opencode turn and proxied anthropic call; `iter_sse_lines` gains the byte mode (framing runs on raw bytes; the terminators are ASCII so a multibyte character can never be split), the default str mode keeps the errors="replace" contract per completed line (itself cheaper than the old incremental chunk decode), and the boundary change is deliberate and test-pinned: in the byte mode an invalid UTF-8 byte reaches the consumers' JSON parse and raises there (the SSE readers' existing malformed-JSON class) instead of degrading to U+FFFD; M19 collector now drives the production byte mode and the healthy range recalibrated median < 0.2 s → < 0.010 s with this PR — the old line sat 6x above the new readings |
| 2026-09-10 | this PR | M88 direct-pass build median 5.28/5.07/5.23 s → 3.93/3.95/3.90 s, −23 % to −26 %, maxima 5.40/5.09/5.25 → 4.15/3.96/3.92 s (three interleaved rounds of the new collector, main checkout before vs branch worktree after back-to-back, 307.3 MB worst on-disk trace /home/chaoli/data/hayden_243809_traces/step000110/trace_rank008_step000110.json, scratch output under /tmp, live home read-only, every paired round faster at load 1.0-2.0 one-minute; artifact 23.8 MB.gz identical across arms; component attribution: the validation parse measured standalone on the same corpus 4.22 s stdlib json.load → 2.56 s orjson including the 0.21 s read; live-log corroboration: two 5.7-6.4 s direct-pass builds and one 19.5 s two-rank merge in today's 7 h server log); 5015-passed suite plus 2 test changes — the corrupt-JSON assertion re-pinned to the orjson message and the NaN-boundary rejection pinned by a new test | the single-trace first-view build validated parseability with stdlib json.load — the slowest parser available, its result discarded before the stream-compress re-read — while the merge path's build has parsed with orjson since the M66 swap (2.56 s vs 4.22 s on the same corpus); the validation now parses with orjson, cutting the build's dominant slice ~40 % and giving both serve shapes one JSON boundary: NaN/Infinity literals stdlib accepts fail the direct-pass build loudly (the merge path's existing rejection) instead of gzipping a literal Perfetto cannot render into the cache; M88 definition and healthy range introduced with this PR |
| 2026-09-10 | this PR | M87 `_abort_session` wall median 22.3/22.5/20.9 ms → 2.2/1.8/1.9 ms, −90 % to −92 %, maxima 27.4-30.5 → 2.2-2.9 ms (three interleaved rounds of the new collector — the run-start client pays the same construction — over a local stub serve, main checkout before vs branch worktree after back-to-back at load 1.75-2.51 one-minute; loop-lag maxima 6.4-7.1 → 5.8-6.0 ms at the ~5 ms ticker floor; component attribution: `ssl.create_default_context` 18.3 ms of the per-call client construction measured standalone, httpx.AsyncClient construct+POST+close 20.54 ms median → 1.65 ms with the prebuilt context; live-server attribution: py-spy over the running instance carried `create_ssl_context` under the opencode backend's `_abort_session`/run-start client at 43 of 893 samples in a 30 s window while an opencode master turn ended); 5013-passed suite plus 1 new construction-contract test | every opencode turn (master runs and opencode workers) built two fresh httpx.AsyncClients per run — one at run start, one at the cleanup abort — and each construction built a default SSL context (~20 ms of event-loop CPU, the CA-set load) although the serve URL is plain localhost HTTP that never uses TLS; both constructions now pass one process-wide prebuilt context (`_SERVE_SSL_CONTEXT`), the per-call client lifecycle unchanged; M87 definition and healthy range introduced with this PR |
| 2026-09-10 | this PR | M36 full-poll body 214877 → 154388 B, −28 %, back inside the < 200 KB range (three interleaved rounds of the verbatim collector, 429-row / 3777 KB worst worker-list corpus of session dfe393f7, live state read-only, main checkout before vs branch worktree after back-to-back at load 2.04-2.12, body byte-identical across all six arms; full poll median 1.95/2.19/2.07 → 1.88/1.95/1.91 ms, conditional 204 0 B unchanged; no-regression re-measures interleaved ×2: M63 /view body 231488 → 170999 B with handler median 1.27/1.28 → 0.98/0.95 ms, M68 marked rebuild 4.50/4.60 → 4.80/4.66 ms, M59 full row 1.93/2.00 → 1.86/2.19 ms with body 59259 B identical; 5009-passed suite) | every delegation-heavy row shipped a 240-char description prefix — 52 % of the 429-row body — while the card paints one CSS-truncated line and the full-text modal fetches the thread row on click; the cap drops to 100 chars, one text-sm line at ~700 px, so every visible character still ships and longer text reaches the modal through the existing description_full_len click-fetch; the corpus's thread count grows without bound (266 rows at the 2026-09-02 calibration, 429 today), so the body range stays honest only with the per-row payload bounded |
| 2026-09-10 | this PR | M10/M15/M48/M73/M70 standing collectors: before — five of 86 crashed in the round's sweep (M10/M15/M48/M73 IndexError at `create_session`'s `backends.options[0]` on the scratch config, M70 AssertionError "artifact-comments injection missing"), no readings; after (repaired commands, main checkout, load 1.78/1.87/1.44) — M10 3000 save_metadata calls / 25774 concurrent reads, 0 torn; M15 3000 _write_cache_entry calls / 425724 concurrent reads, 0 torn; M48 0 search_read_failed lines over 60 scans; M73 amend-validation loop-lag median 0.0058 s / wall median 0.0369 s (14 KB plan page); M70 repeat-view median 0.0026 s, body 1084806 B (injection present) | the 2026-09-09 config-schema series changed the two contracts the five collectors' scratch fixtures leaned on without updating them: 77e1e405 moved the default session backend to the sectioned `backends.options`, whose default is empty (the old flat `backend_options` carried a built-in claude-opus entry), so any `create_session` on a bare scratch config IndexErrors — the suite's own fixtures already pass `backends={"options": […]}`, the baseline's four did not; 126d4cd8 moved the files routes' access-key read from the monkeypatchable `get_config()` to the env-scoped `get_credentials()`, so M70's uncredentialed TestClient request was checked against the live key and served the clean page; the repair seeds one backend option in the four scratch configs (the conftest fixture shape, no behavior change — the metrics are orthogonal to backend choice) and gives M70 the M65 isolation shape (snapshot-seeded credentials.yaml with an empty access key plus `CHARLIEBOT_HOME` pointed at the snapshot before any request); collector commands only, no product code |
| 2026-09-09 | this PR | M86 delegation takeoff-gate, delegation-flow shape median 2.27/3.65/4.00 → 0.00/0.00/0.00 ms, maxima 2.73-6.46 → 0.00-0.03 ms over nine timed warm calls (three interleaved rounds of the verbatim collector, 20534-event worst live chat file of session d321b9ad, scratch CHARLIEBOT_HOME per round, live home read-only, main checkout before vs branch worktree after back-to-back at load 8.2-11.2 one-minute — a host build was spiking, so the before side's spread is load noise, and every paired round still landed ≥2 orders faster); parity witness, the corpus as it stands (blocked verdict both arms): before median 2.20/2.50/5.01 ms, after 3.73/3.73/2.34 ms, same full-walk span both sides, verdicts identical; 39-passed gate-test file plus a 400-history randomized parity test against the verbatim forward walk, full 4948-passed suite | every `/api/internal/delegate` and `/api/internal/improve` POST ran `check_takeoff_gate` as an O(full-history) forward walk, re-normalizing every real user message's whole content and overwriting the two answers the verdict reads (the file-last real user message's takeoff phrase, the file-last parseable pre-takeoff stamp) — per-delegation thread-pool time growing with the busiest master session without bound (2.2-4.0 ms at 20,534 events today, on the spawn path behind the executor pool); the scan now walks backward and stops once both answers are settled — a file-older message can never overwrite either, so the walked span is the tail after the last user message, one turn's length, while a blocked misfire (no take-off, no parseable pre-takeoff anywhere) still walks the whole file, the same span the forward form always paid; one documented divergence, diagnostic only: pre-takeoff bearers file-older than the first parseable one no longer emit `_parse_pre_takeoff_timestamp` warnings (verdict-exact, fewer warning lines) |
| 2026-09-09 | this PR | M75 first-event catch-up loop-lag median 0.0103/0.0106/0.0102 → 0.0082/0.0087/0.0067 s, −20 % to −35 %, every paired round faster (three interleaved rounds of the verbatim collector, 20534-event worst live chat file of session d321b9ad, scratch CHARLIEBOT_HOME per round, live home read-only, main checkout before vs branch worktree after back-to-back at load 2.70-2.83 one-minute); loop-lag maxima 0.0805-0.0839 → 0.0774-0.0955 s and wall medians 0.0976-0.1010 → 0.0984-0.1130 s unchanged within noise — the residual stall is the threaded cold load's own CPU and its GC pause (component check on the same corpus: cold threaded load max ticker gap 27.9 ms with gc on vs 12.1 ms off, wall 90 vs 61 ms), outside the feed this diff slices; feed-only attribution: the unsliced on-loop feed's worst ticker hold 23.3 ms vs 6.3 ms sliced at 256 events, wall 19 → 22.6 ms; no-regression re-measures interleaved ×2: M45 catchup replay loop-lag 0.0063/0.0063 → 0.0063/0.0065 s max 0.0064-0.0065 → 0.0064-0.0068 s with digest 314dfbe9fd89 identical, M26 advance 0.15/0.15 → 0.17/0.15 ms parity True digest e94c56635194, M6 append-round 0.06/0.05 → 0.06/0.05 ms parity True; 4899-passed suite plus one new mid-feed drop test | the first persist_and_broadcast for a session after server start fed the whole caught-up corpus through the aggregator inside one threaded span — the M45 pathology's one un-sliced sibling: the pure-Python feed parked the event loop behind GIL handoffs for the feed's full span (23 ms worst hold measured on the on-loop form, on top of the load's), and the streamed turn's first delta after a restart waits behind the whole init; the feed now runs on the event loop in 256-event slices with a yield between slices (the `_CatchupWalk` shape), and the drop epoch re-check moved from once post-init to every slice boundary so a mid-init drop aborts at the next boundary instead of finishing the stale feed; the corpus load keeps its threaded hop (a cold parse is one C-heavy pass), and the slice loop re-reads the list length so an append landing mid-feed is fed like the threaded form's list iteration reached it; M75 healthy range unchanged (the reading was already inside < 0.020 s) |
| 2026-09-09 | this PR | M74 turn-end rescan loop-lag median 0.0104/0.0105/0.0105 → 0.0085/0.0072/0.0098 s, wall median 0.0122/0.0120/0.0122 → 0.0086/0.0073/0.0099 s, −18 % to −39 %, wall maxima 0.0201-0.0209 → 0.0127-0.0158 s (three interleaved rounds of the verbatim collector, 9.9 MB / 391-line worst on-disk raw agent log of session 4fcd4c43, live home read-only, main checkout before vs branch worktree after back-to-back at load 3.0-3.1 one-minute / 2.24 five-minute, every paired round faster, 391 projected events both arms); component attribution: parse_raw_lines measured 9.76 ms of the 12.2 ms wall on the same corpus (whole split 8.26 ms, replace-decode pass 5.69 ms, orjson-on-bytes 4.68 ms — the walk+strict-bytes fast path 6.17 ms); no-regression witness on the sibling funnel: M84 tail-follow replay 11.4 → 11.6 ms, stdout-stream 5.9 → 5.3 ms, within noise, that loop untouched; parity 0 divergences over 1650 live raw logs / 186,647 lines (old inline implementation vs new, event-identical); 4940-passed suite plus 2 new contract tests (torn-multibyte U+FFFD fallback, valid final line without newline) | parse_raw_lines whole-split every raw log (one list of all 391 lines per pass) and replace-decoded every line before the parse funnel (5.7 ms per 9.9 MB) although orjson reads raw bytes directly; the walk now emits one find+slice piece per line (the M84 tail-follow walk) and the strict bytes parse is the fast path — the errors="replace" decode runs only on a line the strict parse rejects, keeping the torn-multibyte-parses-as-U+FFFD contract (pinned by a new test on a corrupted byte mid-line, the shape a truncation cannot produce) and the funnel's single skip-contract home; the change rides every claude-family master turn end (the model-attribution rescan) and the re-attach result scan (scan_result_exit and resolve_run share the helper); M74 healthy range unchanged (already inside < 0.015 s) |
| 2026-09-09 | this PR | M84 tail-follow replay median 29.9/29.1/29.6 → 11.2/10.9/11.3 ms, −62 % to −63 %, maxima 30.6/29.9/29.9 → 11.8/11.1/11.6 ms (three interleaved rounds of the verbatim collector — arms swapped in round 2 — 9.9 MB / 391-line worst on-disk raw agent log of session 4fcd4c43, scratch copy per round, live home read-only, every paired round faster at load 1.0-1.6; parser parity 0 divergences over all 391 lines in every round, 391/391 events both arms; stdout-stream replay unchanged 5.1-5.2 ms, the untouched sibling funnel; component attribution: cProfile puts bytes.split at 65 % of the pre-fix replay wall, 21.7 ms per pass, and the chunked split microbenchmarks 3.37 ms per 9.9 MB pass against 0.56 ms for the resumable find+slice walk — 6x; 4934-passed suite plus 2 new carried-partial and torn-tail contract tests) | the tail-follow loop's consumed-line split ran `buf.split(b"\n")` per 64 KB chunk — a full list allocation of every complete line in the chunk per read, re-copying and re-boxing the chunk's whole content per cycle (the loop's split cost measured 21.7 ms of the 37 ms replay on the 25 KB-average-line corpus) — while a resumable `chunk.find(b"\n", start)` + slice walk yields the identical line sequence (empty pieces, trailing partial carry, torn-tail drop all unchanged) at one list-free slice per line; the loop is the live read side of every covered backend's streamed turn (master runs and thread transports both write agent.raw.ndjson), so the win rides every live turn's read path, and the re-attach replay shares it; M84 healthy range unchanged (the reading was already inside < 0.060 s) |
| 2026-09-09 | this PR | M85 verify-finalize report read median 14.4/13.5 → 0.4/0.4 ms, −97 %, maxima 29.6-31.8 → 0.6-0.7 ms (three interleaved rounds of the new collector — one sequential then two back-to-back interleaved main-before/branch-after, 6.7 MB / 2315-event worst on-disk worker log of session 47ff1e6c thread c8eb0a1e, live home read-only, every paired round faster at load 2.0-3.3; report output identical 2487 chars in all arms; no-regression re-measures interleaved ×2: M31 events-summary read 0.0006/0.0007 → 0.0006/0.0006 s median, maxima 0.0009-0.0010 both arms; 4915-passed suite plus 10 new from-the-end walk and resolve-parity tests) | the verify finalize chain's report read full-parsed the thread's whole events log (`parse_ndjson_file`) to scan its last events — the M31 pathology's one uncovered sibling — although the RESULT event the report quotes sits at the log tail on every on-disk verify thread; the read now resolves through `iter_ndjson_events_from_end`, a new from-the-end generator sharing the M31 segment mechanics (512 KiB segments walked backward, the left-truncated first line carried into the next older segment), and `parse_ndjson_tail_parseable` re-homes on the same generator — one home for the from-the-end walk, its last-N consumer unchanged and pinned by its standing tests; the walk stops once both judgments settle — the result judgment at the first result event from the end, the assistant judgment at the first non-empty assistant text — so a whole-file walk survives only for logs one judgment stays open on (a fallback whose text never appears, or no result event whose proof needs the file's end); M85 definition and healthy range introduced with this PR |
| 2026-09-09 | this PR | M84 tail-follow replay median 39.1/36.5/38.3 → 29.9/29.8/29.6 ms, −18 % to −24 %, maxima 40.1/37.0/39.4 → 32.8/30.0/29.9 ms; stdout-stream replay median 14.8/14.7/14.7 → 5.1/5.0/5.3 ms, −64 % to −66 %, maxima 15.0-17.6 → 5.2-5.4 ms (five interleaved rounds of the new collector — three sequential then two back-to-back interleaved main-before/branch-after, 9.9 MB / 391-line worst on-disk raw agent log of session 4fcd4c43, scratch copy per round, live home read-only, every paired round faster at load 1.0-3.2; parser parity 0 divergences over all 391 lines in every round — stdlib json and orjson agree on the whole corpus; 4889-passed suite plus 4 new stream-funnel contract tests) | the backend stream funnels — the raw-log tail-follow loop (the live and re-attach read side of every covered backend's streamed turn), the spawned-stdout NDJSON reader, the opencode SSE payload reader, and the anthropic proxy's upstream chunk reader — parsed every stream line with stdlib json.loads while orjson parses the same lines ~2x faster (the M78 file-read funnel's measured ratio; this corpus: stdout-stream −65 %); the swap covers the four funnels with the skip contract unchanged (a malformed line yields nothing in the tail/stdout readers, raises in the SSE/proxy readers where it always raised), and the boundary follows the M78 file readers' deliberate precedent: the stdlib NaN/Infinity extensions and double-overflow floats now skip as malformed in the tail/stdout funnels and fail the frame loudly in the SSE/proxy readers — machine-written upstream JSON carries none of those literals, pinned by the NaN contract test and the collector's corpus parity check; M84 definition and healthy range introduced with this PR |
| 2026-09-09 | this PR | M82 worker events-log append median 140/140/146 us → 71/72/90 us, −49 % to −51 %, maxima 183-384 us → 96-123 us (three interleaved rounds of the new collector, 50 timed appends after 5 warm on a scratch worker log under /tmp, live home untouched, main checkout before vs branch worktree after back-to-back at load 2.66/2.58/2.38, every paired round faster; component check: one to_thread no-op round-trip is ~67-104 us on this host, the M34/M52 rows' figure, so the pair's second hop was the whole gap); 4880-passed suite | the worker's per-event events-log append rode aiofiles' write+flush pair — two executor round-trips per streamed event (text delta, tool use, tool result, thinking) on the default pool every poll read, chat append, and probe shares; the run now holds one raw O_APPEND fd and each event lands through one asyncio.to_thread hop around a write-all loop (the same short-write contract as append_ndjson), with no durability change — the events log is a diagnostic stream and carried no fdatasync (aiofiles flush is not fsync); the fd is held for the run because a worker's events log is append-only and nothing rewrites or replaces it mid-run (only chat files archive); M82 definition and healthy range introduced with this PR |
| 2026-09-09 | this PR | M81 page re-render wall 33.13/33.64/31.08 → 0.87/1.20/0.95 ms, −97 %, 40 → 0 walks (three interleaved rounds of the new collector, 40 math-free bodies / 57.6 KB / corpus sha1 7409bcd20e4c of the 36.3 MB worst live chat file, katex 0.16.21 over jsdom, main checkout before vs branch worktree after back-to-back at load 3.0-3.4, every paired round faster, page innerHTML parity true every round); streamed math-free draft (11.4 KB, sha1 b155f860788f — the M54 corpus) 13 paints: walk wall 4.29/4.52/5.08 → 0.00/0.00/0.00 ms; 4877-passed suite plus the 7-case gate suite registered in _NODE_TESTS | the chat's KaTeX auto-render walk scanned every prose text node for the four delimiters on every message re-render (each session switch and page re-render) and on every coalesced streamed paint even when the message carries no math — the walk M33's replay and M60's repeat-page metric both stub away, so no standing number saw it; renderChatMath now skips the walk when the message's own source carries none of the three delimiter initials ($, \(, \[) — the streamed paint passes the draft text, message renders fall to the existing `.prose-msg[data-raw]`, elements with neither (raw backend output) keep the unconditional walk, and the predicate also forces the walk on character references (any numeric reference or a named reference of the four delimiter characters), which the browser decodes into the walk's text nodes — the review's entity finding; a $-bearing draft keeps the full walk — the worst on-disk draft (100 KB, $ inside) is unchanged by design |
| 2026-09-09 | this PR | M63 /view handler median 0.93/0.90/1.00 → 0.86/0.89/0.88 ms, maxima 1.22/1.02/1.20 → 1.18/1.14/1.16 ms (three interleaved rounds of the verbatim collector, 339-thread worst corpus of session 3b91d606, scratch CHARLIEBOT_HOME, main checkout before vs branch worktree after back-to-back at load 1.15-1.69, every paired round faster; body 179222 B identical across arms); M35 view request median 4.40/3.78/3.88 → 3.45/3.60/3.41 ms, maxima 5.09/4.50/4.72 → 4.48/4.34/4.16 ms (three interleaved rounds of the view slice on the shared 20534-event snapshot of session d321b9ad, body 183003 B and digest ea2d0c6b27c3 identical across all six arms; events and bootstrap digests 68668edc2776 / 8e4653af40df unchanged); component attribution: build_session_view_data warm wall 0.152/0.145/0.136 → 0.036/0.043/0.032 ms on the 20534-event corpus, −73 % to −77 %, every paired round faster (three interleaved rounds, scratch home per run); no-regression re-measures on the branch: M26 advance 0.12 ms parity True digest e94c56635194, M6 append-round 0.05 ms parity True; 4879-passed suite | the session view's warm-projection path loaded the session's whole chat-event corpus through a threaded `load_chat_events_sync` call purely to fill `SessionViewData.raw_events` — a field no consumer reads (the route payload, the tests, and the frontend never touch it; the usage resolution that follows reuses the same events cache, so the load warmed nothing the next read needed) — the poll-during-a-turn and every SPA switch paid the executor round-trip for a dead list; the field and the `_tail_events_page` return element that fed it are gone, the full-history branch (message_limit=None) keeps its local list for `events_to_view` |
| 2026-09-09 | this PR | M66 merged build median 3.126/3.108/3.134 → 2.956/3.003/2.996 s, −4 to −5 %, maxima 3.128-3.153 → 3.015-3.022 s (three interleaved rounds of the collector's build with the checkout asserted per arm, main checkout before vs branch worktree after back-to-back at load 1.1-1.9, every paired round faster; minima 3.097-3.127 → 2.955-2.989 s; parsed-trace parity True across arms — 496,116 events both, artifact 15.6 MB.gz both; 4879-passed suite plus the merge-core id-contract test) | the walk paid a str() per event on the pid plus a second on the tid, a method call into the id sequencer per tid, and a second per-event set probe for the thread_name first sight — 0.77 s standalone on the 496k-event corpus; the walk now probes a pid_map keyed on each label's raw pid value beside its str key (99.5 % of this corpus's events skip the str), probes the sequencer's live map inline with the thread_name riding the same first sight, and the sequencer's call answers a hit with one dict probe; label merging keeps the str-keyed last-wins rule (int 7 and "7" are one pid), pinned by the merge-core contract test |
| 2026-09-09 | this PR | M80 churn changed-round wall median 0.0048 s over three rounds (0.0046/0.0048/0.0048), max 0.0048 s, scanned 1.07 MB per round, rows digest dfcd8e461081 identical across all three; before side is not a number: the collector as #1140 committed it crashed before printing (AttributeError: 'PosixPath' object has no attribute 'rsplit'), so the metric had no successful standing measurement since its landing — this row is its first, taken by the repaired command at load 0.85-0.93, scratch corpus copied once per round, live home read-only | the collector's print line called `.rsplit` on a `Path` (one str wrapper dropped); the command is otherwise #1140's verbatim, and the reading corroborates that PR's post-fix numbers (0.0064/0.0065/0.0064 s) |
| 2026-09-09 | this PR | M80 churn changed-round wall median 0.0566/0.0544/0.0565 s → 0.0064/0.0065/0.0064 s, −88 %, maxima 0.0570-0.0575 → 0.0065 s, scanned 24.16 → 2.93 MB per round (three interleaved rounds of the new collector, the 6.7 MB worst claude transcript and the 15.1 MB worst codex rollout copied to a scratch corpus with one ~1 MB line-aligned self-append per file per round, live home read once for the copy, main checkout before vs branch worktree after back-to-back at load 1.58-2.96, every paired round faster, rows digest 84adb4d24b91 identical across all six arms); live-churn corroboration on the real corpus before the fix: a changed round 3 s into an active turn scanned 16.7 MB in 0.430 s (the 40 h server log's /token-usage p90 219 ms, max 2354 ms, n=220, against the 20 ms warm median); no-regression re-measures on the branch: M7 warm page 0.020 s = main's 0.021 s, M7 quiet changed round 0.039 vs 0.041 s (0.0 MB re-read both), whole-corpus cold pass 3.87-4.00 → 3.63-3.65 s (the marker-line find scan rides C level) with rows digest d97bb6fb4fc8 and notes digest 60c1b1a01c4c identical across back-to-back main/branch rounds ×2 and scanned 636.6 MB main / 650.3 MB branch — the bytes count is now the true byte total where the text-mode read counted decoded chars, M26 advance 0.18 ms parity True digest e94c56635194, M56 /status 2.08 ms; 4878-passed suite plus 5 new tests (tail parity for both sources, completed-partial-line, replaced-or-shrunk guard rejection, pre-tail-schema entry) | the tally's cached parse re-read every moved log file whole per changed round, so the /token-usage page paid a whole-transcript re-read for each file an active turn appended to since the last collect; the parse now tracks the consumed byte offset (the last complete line's end, which a mid-read append pushes past the signature's size — the offset the tail continues from, so an over-read line can never double-count) and proves the unchanged prefix from a guard hash of its final 8 KB plus the boundary newline before parsing only the appended tail, Claude records deduping against the cached keys and Codex carrying the model context, rootness and self-check state forward; a replaced, truncated or mid-line prefix fails the guard and re-parses whole, and the jsonl logs' append-only write shape is the ground the window proof stands on; M80 definition and healthy range introduced with this PR |
| 2026-09-08 | this PR | M71 capped search request median 6.30/6.42/6.57 ms → 4.71/4.48/4.39 ms, −25 % to −33 %, maxima 6.91-7.34 → 5.50-8.43 ms (three interleaved rounds of the verbatim collector, 200 rows / 207540 B body, shared snapshot of 1090 metas + 178.2 MB active live chat files + triggers dirs, parsed-body digest ea61f509e6d7 identical across all six arms, main checkout before vs branch worktree after back-to-back at load 1.63-1.66, every paired round faster; route-body wall 5.6 → 2.4 ms per call in-process; no-regression re-measures interleaved ×2: absent-needle search_sessions_readonly 1.09/0.97 → 1.09/1.02 ms median and M56 /status 2.02/2.12 → 2.03/2.09 ms with digest c45ec651955b identical; 4873-passed suite plus 3 new tests — byte parity against the merged-render reference, write-funnel rename invalidation, and the memo cap) | the capped search's per-row render paid 200 pydantic model_dump(mode="json") calls (~1.4 ms profiled) plus one 207 KB encode per request; the static fields now render once per metadata object into segment bytes memoized on the object's identity (the value pins the object, so an id reuse can never serve another object's bytes; the metadata cache replaces the object whenever its file provably changes — every writer publishes through the atomic tmp rename and the re-parse is a fresh instance — the same ground the read-only search's shared-reference contract stands on), and the five overlay fields (dict-assignment updates on keys the model already declares, so they sit at their model-definition positions) render per request as scalar bytes — the spliced body is byte-identical to the FastJsonResponse render of the merged dicts, pinned by a test against the merged-render reference; the manager's name-match pass lowered every session name twice (match test + content-candidate split) and now lowers once; healthy range recalibrated median < 0.010 s → < 0.006 s with this PR |
| 2026-09-08 | #1131 | M55 first view 0.2082/0.2191/0.2184 s → 0.1488/0.1499/0.1469 s, −28 % to −33 %, maxima 0.2184-0.2349 → 0.1493-0.1531 s (three interleaved rounds of the verbatim collector, 1.5 MB worst artifact pair understanding_packed-batch-cost-balance_v10.html vs _v9.html, scratch CHARLIEBOT_HOME, main checkout before vs branch worktree after back-to-back at load 0.28-0.89, every paired round faster; served bodies 150829 B both arms, same-process byte-identical, the per-checkout cache-bust digest gap the M70 row documents; repeat view 0.0025 → 0.0024 s, the M55 memo path, no regression; component corroboration: in-process annotate wall median 0.1724/0.1798/0.1730 → 0.1283/0.1331/0.1270 s, −25 % to −27 %; 4870-passed suite plus 3 new parity tests) | the annotate's tokenizer walked every text character in Python — per-char isspace/isascii/isalnum plus a 4-range CJK membership call, ~28k characters per side — and every parsed text node carried a per-character raw-range tuple list (~1.4M tuples per compare-view pair) consumed only to map token offsets back to source spans; the tokenizer is one compiled-regex finditer whose alternation reproduces the walk exactly (whitespace runs skipped, `[A-Za-z0-9_]` word runs, every remaining character its own token — the per-character granularity the CJK alignment relies on, pinned by a 500-document randomized parity test against the reference walk), and the text part carries a text_is_raw flag plus raw_span() — parsed data maps raw offset = start + logical (handle_data already proves source[start:end] == text), an entity reference maps every logical range to its whole raw span — so _leaf_tokens, _raw_bounds and _wrapping_removes_direct_text compute spans arithmetically and the flat per-character map is gone; annotate output byte-identical across the fixture pair, the 1.5 MB worst pair, diff_text, and 500 randomized CJK-bearing pairs; recorded in this docs-only follow-up per the #1046 precedent, the landing PR #1131 shipped without it |
| 2026-09-08 | this PR | M72 listing request median 9.92/10.99/10.17 ms → 5.99/5.99/5.96 ms, −39 % to −45 %, maxima 10.85-12.12 → 6.69-7.04 ms (three interleaved rounds of the verbatim collector, 1091-entry sessions root, live state read-only, main checkout before vs branch worktree after back-to-back at load 0.58-0.61, every paired round faster; served body byte-identical across all six arms — 239486 B, sha1 7e724dd3aff9; builder-level repeat median 6.06 → 3.66 ms over 15 interleaved calls; no-regression re-measures interleaved ×2: M70 repeat view 2.7/2.9 → 2.8/2.6 ms with the per-checkout cache-bust digest the M55 row documents, M55 repeat 2.6/2.5 → 2.4/2.5 ms and first views 0.21-0.23 → 0.21-0.22 s; 4867-passed suite plus the new formatter fuzz) | the repeat listing re-paid the sort (~0.8 ms) and the per-entry row build (~2.8 ms: f-strings, the escape fast-path check, the size text, and a gmtime+strftime pair per entry) on top of the stat walk no listing can skip; the served page now memoizes on the walk's own entry snapshot ((name, is_dir, size, mtime) per entry, resolved dir and URL prefix around it) — the HTML is a pure function of the walked state, so equal walked state proves the stored page equals what this walk would build, an invalidation-free ground that leans on no rename-atomicity assumption the sibling (mtime_ns, size) memos require, and a repeat view pays the walk plus one memo lookup; the mtime text renders through an integer civil-from-days formatter floored the way gmtime floors a fractional epoch, pinned byte-identical to strftime(gmtime()) by a 24k-epoch fuzz plus a 67k-mtime live-corpus check; the route's resolve/exists/listing executor hops collapsed into one (a vanished path answers 404 from the scandir's own FileNotFoundError, and only the ambiguous not-a-directory case keeps its explicit exists); healthy range recalibrated median < 0.013 s → < 0.008 s with this PR |
| 2026-09-08 | this PR | M7 changed-round collect median 0.059/0.061/0.060 s → 0.040/0.041/0.041 s, −31 % to −33 %, maxima 0.071-0.073 → 0.048-0.051 s (three interleaved rounds of the verbatim changed-round harness, 7 timed rounds each, live corpus read-only, main checkout before vs branch worktree after back-to-back at load 1.0-1.9, every paired round faster; 15 rows both arms; mechanism probe: pathlib._parse_path calls per changed round 1574 → 23, posix.stat 3122 and posix.scandir 1551 unchanged — the removed cost is the serve walk's per-file Path construction and its double str(), the syscall count identical; no-regression re-measures: warm whole-tally hit 16.5 → 17.0 ms (standing band), rows digest 34a34e01daa1 and notes digest f8747d8fdc5b identical across arms, cold pass 2151 → 2111 ms; 4866-passed suite) | the fresh-sources serve walk (`_walk_source`) re-walked the corpus through the Path-yielding `_iter_jsonl` and paid `TallyCache.lookup`'s own `path.stat()` per file — a second walk-shaped pass over the corpus the signature walk had just measured — while `_iter_jsonl_stats` already produced each file's str path with its stat attached; the serve walk now consumes that walker, looks the cache up through `lookup_sig` on the walker's own stat pair, and the parse functions take str paths; cache keys are byte-identical (`entry.path` equals the old `str(Path(dirpath)/name)`), so persisted documents keep hitting; `TallyCache.lookup` (the Path-taking form) is gone — `lookup_sig`/`store_sig` are its only remaining shapes |
| 2026-09-08 | this PR | M62 chain median 0.3660/0.3613/0.3516 s → 0.3316/0.3565/0.3360 s (three interleaved rounds of the verbatim collector against the real origin, main checkout before vs branch worktree after back-to-back at load 1.21-1.94, every paired round faster; the ls-remote network RTT dominates both arms and carries its own jitter); component A/B of the changed function alone, `resolve_base_branch(REPO, "origin/main", remote_tip=tip)` with the caller's fresh tip, 20 timed calls after a warm pass, four interleaved rounds: 28.7/29.5/29.0 → 7.0/6.8/6.6 ms median, −76 % to −77 %, maxima 31.7/31.7/29.9 → 7.3/7.2/7.2 ms; start_point origin/main identical across all arms; 4866-passed suite including the 23 resolution-matrix tests | the probed resolution ran four git subprocesses where one suffices: the origin get-url probe (a supplied remote_tip is the caller's own successful ls-remote, which already proves origin configured and reachable), the second remote-tracking rev-parse after a fetch decision that skipped the fetch (no fetch between the reads, so the tracking ref still holds the probed tip), and the refs/heads rev-parse whose divergence check only the bare-branch form reads (an explicit origin/<b> request never used it); the no-tip self-probing path, the fetch on a probe mismatch, and every resolution-matrix error path are unchanged |
| 2026-09-08 | this PR | M73 amend-validation wall median 0.6661/0.6793/0.6707 s → 0.0420/0.0395/0.0397 s, −94 %, maxima 0.6840-0.6967 → 0.0418-0.0482 s (three interleaved rounds of the verbatim collector, 11 KB bound plan page plan_02.html passing the current pure assertion set, scratch CHARLIEBOT_HOME, main checkout before vs branch worktree after back-to-back at load 1.3-3.7, every paired round faster; the after arm's `present` cold pass pays the one-time browser launch, the five timed amends ride warm; loop-lag medians 0.0066-0.0074 → 0.0057-0.0059 s at the 5 ms ticker floor; measured-height parity: the same corpus page renders 1198 px through the branch's warm CDP drive and main's dump-dom drive; 4866-passed suite including the local_only real drive, 4863 non-local (CI's selection), plus 2 new lifecycle tests and 1 local_only real-drive test) | the page-height measurement launched a fresh headless Chrome per plan/understanding registration — ~0.55 s of browser startup against ~25 ms of render, the whole M73 wall — while the registration gate runs off-loop since the M73 loop fix; the browser now stays warm per OS process and serves every measurement over the DevTools websocket (websockets.sync, already a dependency), the probe page and its template unchanged, the trade being one idle browser's resident set for the process lifetime; the dump-dom drive moves verbatim into the suite as the stub renderer the shell-script chrome needs; healthy range recalibrated wall median < 1.0 s → < 0.2 s with this PR |
| 2026-09-08 | this PR | M79 repeat-view median 0.1045/0.1040/0.1046 s → 0.0011/0.0011/0.0011 s, −99 %, maxima 0.1052-0.1058 → 0.0016-0.0019 s (three interleaved rounds of the new collector, 2718-ref charlie-bot checkout, main checkout before vs branch worktree after back-to-back at load 1.07-1.32; first view unchanged 0.1083-0.1091 → 0.1048-0.1075 s, subprocess-bound both arms; list digest 520bfb331161 (50 names) identical across all six arms; the repeat's remaining 1.1 ms is the ref-state signature walk, measured standalone 0.9-1.0 ms; no-regression re-measures interleaved ×2: M41 repeat-view 0.0014-0.0016 → 0.0014-0.0016 s, M43 0.0014-0.0015 → 0.0014-0.0015 s, M14 loop-lag 0.0056 → 0.0055 s at the 5 ms ticker floor; 19-passed git-diff API suite plus 2 new memo tests) | the /diff branch picker re-ran one `git branch -a --sort=-committerdate` subprocess per fetch over a ref set that grows one loose file per branch this workflow leaves behind (2718 refs, 852 loose at measurement; the listing's 104 ms scales with it, the same unbounded trend the M41 row documented for the resolution walk) — the listing now memoizes on (repo, ref-state signature), the ref-resolution memo's own key and invalidation ground: the ref set, every listed branch's committerdate, and HEAD all publish through ref mutations the signature covers, so an unchanged signature proves the listing current; M79 definition and healthy range introduced with this PR |
| 2026-09-08 | this PR | M78 cold whole-file parse median 97.9/98.0/100.1 → 55.4/53.4/51.5 ms on the 36.3 MB / 5519-event worst live chat file and 26.5/27.0/26.5 → 12.6/13.1/12.7 ms on the 6.7 MB / 2315-event worst on-disk worker log, −44 % to −53 % (three interleaved rounds of the new collector, live corpora read-only, main checkout before vs branch worktree after back-to-back at load 0.96-1.03, every paired round faster; parser parity asserted beyond the parse-success check: a strict type-sensitive deep comparison of orjson vs stdlib json.loads over the whole 14 GB / 2,258,811-line live corpus — 6,279 files — found 0 divergences; M75 catch-up wall re-measured interleaved ×2: 0.1755/0.1751 → 0.0988/0.0965 s, loop-lag unchanged 0.0153 → 0.0155/0.0116 s; no-regression re-measures on the branch: M26 advance 0.17 ms parity True digest e94c56635194, M6 append-round 0.05 ms parity True, M23 0.0003 s, M30 0.0002/0.0003 s, M76 0.00000 s, M77 0.06 ms with 0/36 rebuilt; 4861-passed suite plus 9 new parser-contract tests) | the NDJSON event parse funnel ran stdlib json.loads per line — the hottest parse in the system, feeding every cold events load (the M75 catch-up, the projection build, usage resolution, the M13 cold worker-log read, archive and range reads) — while orjson parses the same lines ~2x faster with identical output; the swap covers the one funnel (`iter_ndjson_events`, which the M13 worker-events reader feeds bytes lines through) plus chat_events' live-range and archive readers; the boundary change is deliberate and test-pinned: the stdlib json NaN/Infinity extensions and double-overflow floats skip as malformed (invisible lines, the skip contract's own answer) and ints at or beyond 2**64 parse as float where stdlib kept exact precision — no live line sits at any boundary; orjson>=3.11 already a dependency since the M66 merge; M78 definition and healthy range introduced with this PR |
| 2026-09-08 | this PR | M66 merged build median 5.26/5.30/5.39 s → 3.26/3.21/3.19 s, −38 % to −39 %, maxima 5.29-5.42 → 3.22-3.29 s (three interleaved rounds of the collector, 191.2 MB / 496,099-event worst on-disk trace /home/chaoli/data/stage3_current_traces/221054_trace_rank000_step000110.json, scratch output under /tmp, main checkout before vs branch worktree after back-to-back at load 0.87-1.12, every paired round faster; artifact 15.6 MB.gz both arms; parsed-trace parity True across all rounds — 496,116 events both arms, normalized digest b07896653af5 identical; 4847-passed suite, the two payload reference tests re-pinned to the orjson encoder's own forms) | the merge's two dominant passes rode the stdlib json module — the 2.25 s parse of the 191 MB corpus plus the 1.6 s batched C-encoder dumps on a corpus that is machine-written JSON parsed and re-serialized with no hand-authored edge cases — while orjson parses the same corpus ~3.5x faster and renders each batch ~5x faster; the event walk and the level-1 gzip pass are untouched, and the artifact size is unchanged; the wire payload changes rendering form (raw UTF-8 where the stdlib form emitted \uXXXX) and the parsed trace is pinned identical; the parse pass rejects the NaN/Infinity literals stdlib json.load accepts, so a trace carrying them fails the build loudly instead of shipping Perfetto-invalid JSON (orjson.dumps renders an in-memory non-finite float as null — unreachable from a trace file); orjson>=3.11 joins the dependencies (uv.lock updated, the host venv carries 3.12.0); healthy range recalibrated median < 12 s → < 8 s with this PR — the old line sat ~2x above the pre-fix reading and 3.7x above the new one |
| 2026-09-08 | #1086 | slack follow-backfill listing 3.42/3.52/3.66 ms → 0.18/0.19/0.20 ms, group-rewrite listing 3.49/3.76/3.84 ms → 0.04/0.04/0.05 ms medians, maxima 5.17-6.00/29.81-34.35 → 0.19-0.40/0.08-0.11 ms (three interleaved manager-level rounds per arm, 1090-meta live corpus read-only — 46 active, 4 slack-active — main checkout before-shape vs branch worktree after-shape back-to-back at load 0.76-0.95, survivor-set parity asserted for both shapes in both arms; the manager code is identical across arms, the diff is the callers' arguments; 4847-passed suite) | the Socket Mode (re)connection backfill and the group rewrite listed every session unfiltered and read one field each — the leaving-the-manager copy, thinking stamp, and sidebar populate ran over the ~1044 archived rows they drop on the next line; the backfill lists ACTIVE directly (identical survivor set — `_load_session_metas(status)` filters `meta.status == status`) and the rewrite scans the shared cached metas read-only, the M40 pattern; no standing collector drives either caller, so the row self-measures its before numbers per the no-baseline-row rule; recorded in this docs-only follow-up per the #1046 precedent, the landing PR #1086 shipped without it |
| 2026-09-08 | this PR | M7 changed-round collect_claude median 87.6/84.1/80.6 → 41.8/40.9/41.0 ms, maxima 91.0/86.3/82.6 → 45.8/42.6/41.3 ms (three interleaved rounds of the component harness — the standing changed-round collector's stale-document restore around tt.collect_claude alone, 7 timed rounds each, live corpus read-only, main checkout before vs branch worktree after back-to-back at load 1.70-1.74, every paired round faster; Claude notes identical across all six arms — 23,293 unique API responses, 30,273 replayed lines skipped, 9 models; mechanism probe: t.add calls per changed round 23,293 → 0 over two interleaved 5-round sets in the quiet regime, the fold's per-record replay gone; a busy-window corroboration run with one live session appending ~16.5 MB per round: 199.8/190.2/204.8 → 146.2/152.9/140.5 ms median, t.add 23,296 → 1,519 — the moved files' own new keys, the full-corpus fold gone there too; whole-collect changed-round medians 232.5/142.2/137.6 → 98.4/60.4/109.4 ms in the same interleaved shape, maxima polluted by the db row-landing regime #1059's row documents, untouched here; no-regression re-measures interleaved ×2: whole-tally warm hit 16.3/16.2 → 17.4/15.3 ms, rows 15 / notes 3 identical both arms, M26 advance 0.20 ms parity True digest e94c56635194; 4847-passed suite plus the 10-round partial-parity test) | the changed round re-folded every served entry's 24,948 Claude records through the cross-file replay dedupe — the seen-set scan and one t.add per unique key were 84-87 ms of the component wall — although the corpus's keys and their first-fold values change only when a file moves; the merged buckets are now incremental per file: each file keeps a partial (its post-dedupe bucket deltas, span, record and within-file-dupe counts, per replay key its copy count), the corpus keeps per-key copy counts with the contributing file, its record values and the copy holders, and a round releases the moved, re-parsed, relabelled, failed and vanished files' partials and key copies, re-folds only those files' records, and sums the surviving partials plus an orphan pool for contributions whose file moved while a copy survives elsewhere, anchored per round at the earliest-walked surviving holder — verbatim replays carry identical token values (the dedupe's own premise), so only the account label a fresh scan would credit moves, and an earlier-walked newcomer carrying an already-credited key takes the credit back (the review-found divergence, fixed before landing); parity pinned by a sequence test asserting the incremental collect equals a fresh fold after an append, the credit transfer, a contributor dropping a replayed key (orphan transfer), the last copy dropping, a file deletion, an account relabel and a cacheless round |
| 2026-09-08 | this PR | M20 cold per-divider extract median 88.1/88.6/91.3 ms → 6.8/7.2/7.2 ms, maxima 122.2-125.2 → 10.4-10.6 ms (three interleaved rounds of the new collector, 5519-event / 36.3 MB worst extract corpus of session aa196b47, 6 unseen dividers 0.70-0.95 of the corpus, events cache warm, scratch CHARLIEBOT_HOME, main checkout before vs branch worktree after back-to-back at load 1.4-1.8, every paired round faster; asks 19 identical across all arms and the standing repeat digest bb99828aa5b6 identical; no-regression re-measures on the branch: M23 8-page scroll 0.0003 s, M30 live-half 0.0002 s / append-round 0.0003 s, M26 advance 0.18 ms parity True digest e94c56635194, M6 append-round 0.06 ms parity True, M17 fork 0.0556 s; 4845-passed suite plus 3 new range-reader tests) | the recap's per-divider extract re-entered load_chat_events_range, whose unarchived half re-read and re-parsed the whole live prefix per divider — 88 of the 92 ms per-divider wall on the worst corpus — while the events cache held the same parsed events; the unarchived range read now serves a warm events cache as a slice (the cache is the parsed truth load_chat_events_sync's consumers already trust: save_chat_event is the single append funnel and every whole-file rewrite — archive rotation, fork, delete — drops the cache in the same flow), which also removes the physical-line/parsed-event index skew a malformed line injected between the warm count and the disk read; cold per-divider sub-metric added to the M20 definition and collector in this PR |
| 2026-09-08 | this PR | M56 /status request median 2.18/2.07/2.04 ms → 2.01/2.18/2.09 ms, maxima 10.12/10.36/10.10 → 9.68/9.63/9.89 ms (three interleaved rounds of the verbatim collector, 46 sidebar ids, live corpus read-only, main checkout before vs branch worktree after back-to-back at load 1.2-1.7, body 10141 B and parsed-body digest ab06240028f8 identical across all six arms; the poll-10 wall no longer contains the self-heal sweep — single-portal shape, one TestClient context as the server's one-loop shape, 30-request runs: max 8.88/8.15 ms → 2.30/2.38 ms, requests paying > 4 ms 3/30 → 0/30, p50 1.17/1.14 → 1.36/1.45 ms, the detached task's scheduling and its background overlap spread across the run; no-regression re-measures interleaved ×2: M21 sweep 0.0041-0.0046 s median both arms (46 active sessions), M51 post-write deep probe 2.11-2.17 ms both arms (339-file corpus), M40 starred 0.16-0.19 → 0.14-0.17 ms and groups 0.14 → 0.11-0.12 ms, M44 scheduled 2.13/2.11 → 2.16/2.10 ms with maxima 6.00/6.11 → 6.07/6.02 ms and digest 81fafe421f19 identical, M71 capped search 6.35/6.26 → 6.27/6.19 ms with digest 0eab5cddcdf2 identical; 4842-passed suite) | the every-10th-poll self-heal sweep ran inline inside the poll's request wall — the poll awaited a 46-session signature sweep while its answer needed only the snapshot and the dirty set; the sweep now runs detached (single-flight through create_logged_task): the poll answers from the snapshot plus its dirty/cold probes and the sweep's results land for the polls that follow it, so a missed mark still heals one poll later inside the same window; dirty-marked sessions stay with the synchronous polls (a mark must be consumed only by a probe whose result lands in a response), force=1 keeps its synchronous full probe, a failed sweep re-marks its selected sessions, and a loop-teardown cancellation re-raises — create_logged_task treats a cancelled task as clean, which keeps the per-request-portal collectors whose 10th request schedules the sweep (M56/M44/M71) free of the ~290 ms CancelledError-traceback render the first draft logged as a sweep failure |
| 2026-09-08 | this PR | M7 changed-round collect, entry-served regime (db WAL quiet): median 239.5/240.6 ms → 99.5/107.2 ms, every paired round faster (235.3-246.2 → 97.8-100.0 ms and 239.8-242.8 → 106.1-114.2 ms over the two 5-round sets; three interleaved rounds of the verbatim changed-round harness, main checkout before vs branch worktree after back-to-back at load 1.2-2.3; round 3's WAL-moving regime unchanged 137.0 → 128.1 ms median — the scan path is untouched; row-landing maxima 364.1 → 402.0 ms, the scan+rewrite still runs when rows land by design; mechanism probe: the before arm's one entry-served round replayed 87,357 records, the after arm replayed 0 records over 3 rounds; no-regression re-measures interleaved ×2: whole-tally warm hit 16.3/16.0 → 16.2/15.9 ms with rows 15 / notes 3 identical; 4841-passed suite plus the adoption contract test) | the entry-served changed round replayed the document entry through the per-record fold — 84 % of the round (201 of 239 ms profiled, 111,743 `t.add` calls at the morning measurement) — although a served entry's rows are provably the rows the partial sums: the entry is served only while its stored signature still matches the db, and a row move writes the db or its WAL sidecar, moving that signature; the buckets now adopt through the partial's empty-delta adjust and a process's first entry-served round replays once to build the partial, so the steady-state entry-served round pays the walk and the serve, zero record folds; #1059's rewrite skip (landed earlier today) left this replay regime untouched — its evidence measured the WAL-moving regime, where the scan's probe already served the partial |
| 2026-09-08 | this PR | M63 /view body 277206 B → 179222 B, −35 %, handler medians within noise 1.20/1.12/1.11 ms → 1.24/0.90/0.85 ms (three interleaved rounds of the verbatim collector, 339-file worst threads corpus of session 3b91d606, scratch CHARLIEBOT_HOME, main checkout before vs branch worktree after back-to-back at load 0.66-1.12; M35 no-regression set: events page 2.71/2.63/2.60 → 2.82/2.65/2.77 ms, body 633236 → 624733 B, digest 46d1d509a0d6 → 68668edc2776 — the dropped fields' bytes — while view and bootstrap bodies stayed byte-identical 183003/137271 B with identical digests, that corpus's served window carrying none of the dropped fields; component attribution on the M63 corpus: the 339-row threads array carries none of the dropped fields — the 98 KB sits in 12 projected messages, `full_content` ~68.7 KB over 8 worker summaries (~8.6 KB each) plus `description` ~28.7 KB over 4 task_delegated rows; no-regression re-measures on the branch: M26 advance 0.19 ms parity True digest e94c56635194, M6 append-round 0.08 ms parity True, M75 catch-up loop-lag 0.0109 s, M45 loop-lag 0.0065 s digest 314dfbe9fd89; 4837-passed suite plus the frontend renderer slice, 2 contract tests updated) | the message projection carried the worker summary's full text (``full_content``) and the delegation's task-spec-length ``description`` on every view/bootstrap/events page and every websocket message delta, while no reader of the projection reads either: the worker_summary bubble renders content alone (the renderer contract test pins the full body absent even when carried), the delegation card reads ``delegate_invocation``, the recap asks read role/content, and the review-context scan reads raw events; the fields stay on the persisted chat event — the fork reference and the review scan's data source — and the projection and its wire deltas stop carrying them |
| 2026-09-08 | #1059 | M7 changed-round collect median 238/238/240 ms → 129/128/129 ms, minima 237/236/237 → 128/128/128 ms (three interleaved rounds of the verbatim changed-round harness per checkout, 7 timed rounds each, main checkout before vs branch worktree after back-to-back at load 1.00-1.26, 15 rows and 0.0 MB re-read both arms, every paired round faster; `TallyCache.save`'s equality check skipped the rewrite in every after round — 11.2 MB document, `json.dumps` 91.4 ms + `posix.replace` 6 ms + the 86k-record list rebuild ~8 ms removed from row-unchanged rounds; no-regression re-measures interleaved ×3: M7 warm whole-tally fast-hit 16.6/18.1/15.7 → 18.3/16.7/16.7 ms, inside the standing band, rows 15 both arms; 4837-passed suite plus 2 new contract tests — the WAL-only round leaves the document byte-identical, moved rows still reach it with a fresh signature) | the source-walk changed round re-stored the opencode db's cache entry every round purely to re-sign it at the db's current signature — a WAL write over rows the tally never reads is exactly the shape the probe gate proves harmless, and the re-sign forced the persisted document's full re-serialization and rewrite each round; the entry now keeps its stored signature while the row memo is unchanged (`_opencode_doc_synced`, set False by any scan that changes the memo — cold build or record-bearing row move), so the save's equality check skips the rewrite and the entry re-stores with fresh records and signature on the next round where a row actually moved; a fresh process is unchanged — the stale signature misses the lookup and the row memo rebuild proves the rows the way every miss path already does; row recorded in this docs-only follow-up per the #1046 precedent, the landing PR shipped without it |
| 2026-09-08 | this PR | M77 re-entry median 5.16/5.21/4.87 ms → 0.06/0.06/0.06 ms, maxima 75.18/71.27/71.31 → 0.22/0.23/0.20 ms; rebuilt re-entries 33/36 → 0/36 (three interleaved rounds of the new rotation collector, 12 active live sessions, largest 12483 events, live home read-only, main checkout before vs branch worktree after back-to-back at load 1.5-1.7, every paired round faster; no-regression re-measures on the branch: M26 advance 0.17 ms parity True digest e94c56635194, M34 full 0.0055 s / after=total 0.0017 s (40 B), M63 /view handler 1.16 ms / 277206 B, M56 /status 2.05 ms digest 4f25da4dcc7f identical; 4835-passed suite, the two projection eviction tests parameterized on the constant) | the projection LRU window (8) sat under the tabs' rotation breadth — the live switch diagnostic rotated among 21 distinct sessions in the 16.37 h sample, so most re-entries landed past the window and re-paid the M26 cold build (4-36 ms measured across the live corpus, 84.40 ms worst re-entry); the window rises to 64, covering the observed rotation with headroom; retained projections share the events cache's strings (~0.3-0.6 MB per big session, tracemalloc current-size delta), so the window's memory rides the unbounded events cache's profile; M77 definition and healthy range introduced with this PR |
| 2026-09-08 | this PR | M41 repeat-view median 0.0037/0.0035/0.0036 s → 0.0014/0.0012/0.0014 s, maxima 0.0054/0.0048/0.0045 s → 0.0017/0.0018/0.0017 s; M43 repeat-expand median 0.0041/0.0037/0.0036 s → 0.0014/0.0014/0.0014 s, maxima 0.0043/0.0045/0.0044 s → 0.0015/0.0017/0.0018 s (three interleaved rounds of the verbatim collectors, 192-file charlie-bot root..HEAD manifest, main checkout before vs branch worktree after back-to-back at load 0.9-1.6, every paired round faster; first views unchanged within subprocess noise — M41 0.0724-0.0743 → 0.0696-0.0713 s, M43 0.0212-0.0218 → 0.0177-0.0190 s; served bodies repeat-identical both arms; component attribution: the ref-state signature walk over the live repo measured standalone 718 entries / 2.90 ms before → 29 entries / 0.86 ms after — the repo carries 2569 refs, 706 of them loose ref files, 1538 accumulated charliebot/latency-perf branch refs; M14 loop-lag re-measured unchanged 0.0054 s at the 5 ms ticker floor, no regression; 4835-passed suite plus 2 new signature tests) | the ref-state signature walked every loose ref file under refs/ per repeat view, and the loose tree grows one file per branch the workflow leaves behind — the walk's 2.90 ms had tripled the standing M41/M43 repeat readings as the branch count grew, an unbounded trend; git publishes every ref mutation through a lockfile rename into the containing directory, and a rename that creates, replaces, or removes an entry moves the directory's own mtime_ns, so the signature now carries the refs trees' directory entries (one stat per namespace directory, bounded as branches accumulate) while packed-refs keeps its own (mtime_ns, size) over every packed entry — the same rename ground the M24/M67 directory verdicts stand on |
| 2026-09-07 | #1045 | M17 fork median 0.0753/0.0762/0.0757 s → 0.0560/0.0551/0.0551 s, −26% to −28%, maxima 0.0756-0.0829 s → 0.0560-0.0588 s (three interleaved verbatim-collector rounds, 36.3 MB / 5519-event worst fork corpus of session aa196b47, scratch CHARLIEBOT_HOME, main checkout 828adbce before vs branch worktree after back-to-back at load 1.4-1.9, every paired round faster; component attribution on the same corpus: whole-file decode 26.3 ms, read 25.7 ms, frame check 10.4 ms, write 11.4 ms, and isascii() 4.1 ms — the measured −20.6 ms median matches the removed decode minus the new scan; 4831-passed suite plus one new utf8-parity test pinning both branches: a non-ASCII valid line forks byte-identically, undecodable bytes still raise UnicodeDecodeError at fork time with no reference written) | the fork reference's byte-stream write kept a whole-file `data.decode("utf-8")` per source purely for UnicodeDecodeError parity with the text-mode read it replaced — the decoded str is discarded, and the pass was the fork's single biggest component on the worst corpus; ASCII bytes are always valid UTF-8, so a `bytes.isascii()` scan proves validity and only a non-ASCII corpus pays the full decode, keeping the error contract byte-for-byte (undecodable bytes still raise at fork time, corrupt lines still raise the not-a-serialized-event ValueError); row recorded in this docs-only follow-up per the #976 precedent, the landing PR shipped without it |
| 2026-09-07 | this PR | M55 first view 0.2783/0.2864/0.2735 s → 0.2050/0.1998/0.2016 s, −26% to −30% (three interleaved verbatim-collector rounds, 1.5 MB worst artifact pair understanding_packed-batch-cost-balance_v10.html vs _v9.html, scratch CHARLIEBOT_HOME, main checkout before vs branch worktree after back-to-back at load 0.86-1.16, every paired round faster; repeat view unchanged 0.0025 s both arms — the M55 memo path, no regression; served bodies byte-identical across arms except the per-checkout cache-bust version query the injection embeds — main HEAD 63f291f9 vs worktree HEAD 5fee2a3b, the M70-documented exception; component corroboration: in-process annotate wall median 0.2216 → 0.1950 s over 5 timed calls; 4832-passed suite plus 2 new anchor-parity tests, one a 1500-document seeded fuzz) | the style/header splice re-parsed the whole spliced page through the full DOM builder just to find where head ends and body starts — the third parse cost ~43 ms of the warm annotate's ~212 ms and more on a cold first view, the parser's tree build (per-character text ranges, per-node objects) being the cost; the anchor-only parser rides the same tokenizer walk — convert_charrefs off, the same line-offset math, the same innermost-open-tag end matching (the record rides the stack slot that opened it, so a nested same-tag element takes the end tag exactly as the tree's node would, leaving the first element end-less the same way) — and drops the tree build nothing downstream reads; anchor parity pinned against the full parse on a 1500-document seeded randomized corpus plus the real fixture pair and its spliced output |
| 2026-09-07 | this PR | M76 finalize-judgment pair loop-lag median 0.0053/0.0076/0.0039 s → 0.0000 s all rounds, maxima 0.0073-0.0078 s → ≤ 0.00001 s; wall median 5.33/7.59/3.93 ms → 0.002/0.002/0.001 ms (three interleaved rounds of the collector, 20534-event worst live chat file of session d321b9ad, scratch CHARLIEBOT_HOME, main checkout before — the pre-fix inline call shapes `finalize_effects.terminal_summary_present(mgr.load_chat_events_sync(SID), TID)` + `master_woke_after_summary` — vs branch worktree after back-to-back at load 2.85/1.85/1.27, every paired round faster; cold-corpus component: inline load+scan loop-lag 139.2 ms → fold-method 9.6 ms, the whole-file parse now in the load thread (wall 134.1 → 163.1 ms, the fold build's one pass riding the load); no-regression re-measures: M52 append 2951 µs parity True, M26 advance 0.16 ms parity True digest e94c56635194, M6 append-round 0.06 ms parity True; 4822-passed suite plus 5 new fold-parity tests) | every worker and reviewer completion ran the finalize chain's two idempotency judgments — the duplicate-summary check and the master-wake judgment — as full O(history) scans of the delegating session's chat events inline on the event loop (~47 finalize chains in the 9.4 h live server log), and the pair's event-list load could pay a whole-file parse on the loop when the cache was cold (139 ms on the worst corpus); the chat-event store now derives both answers into a per-session finalize fold — built in the loading thread, advanced O(1) per append through the save funnel, dropped with the cache entry, rules imported from finalize_effects' own predicates so the fold and the pure scans cannot drift — and the two call sites read it through new SessionManager methods that pay one threaded whole-file load on a cold cache; the startup reconcile pass keeps the pure scans over its threaded load; M76 definition and healthy range introduced with this PR |
| 2026-09-07 | this PR | M75 first-event catch-up loop-lag median 0.1600/0.1611/0.1616 s → 0.0157/0.0108/0.0153 s, maxima 0.2423-0.2486 → 0.1008-0.1067 s (three interleaved rounds of the collector, 20534-event worst live chat file of session d321b9ad, scratch CHARLIEBOT_HOME, main checkout before — the pre-fix inline call shape `mgr._get_or_init_aggregator(SID)` — vs branch worktree after back-to-back at load 0.82-1.56, every paired round faster; an earlier interleaved trio under the full test suite's load read 0.2259/0.1595/0.1594 → 0.0108/0.0123/0.0105 s, same shape; wall median 0.1600-0.1616 → 0.1618-0.1750 s — the catch-up's own CPU, now off-loop, unchanged as expected; after maxima ~0.10 s are the whole-corpus thread run's GIL-handoff surcharge the M45 history documented, medians at the ~11 ms M74 post-hop shape) | the first persist_and_broadcast for a session after server start caught the live aggregator up to the whole on-disk corpus inline on the event loop — parse plus feed of every persisted event including a per-event draft-snapshot build whose delta the catch-up discards — freezing every concurrent request and WebSocket at the session's first persisted event after every server restart (the M14 pathology on the streamed-turn funnel); the catch-up hops to a thread behind a per-session init lock (the same instance then carries the live feed with stream-delta emission restored), and the feeds that discard stream deltas (catch-up, history projection, events_to_messages/events_to_view) construct with emit_stream_deltas=False; rider measured within noise: cold projection build 186.4/183.0 → 182.0/183.6 ms on the d321b9ad corpus and 167.2/173.1 → 175.8/159.3 ms on a481fbde's 12452-event corpus (interleaved pairs of the cold-build harness); no-regression re-measures: M26 advance 0.18 ms parity True digest e94c56635194, M6 append-round 0.07 ms parity True, M35 events/view/bootstrap digests 46d1d509a0d6 / ea2d0c6b27c3 / 8e4653af40df identical, M70 repeat view 0.0028 s; review round: a drop landing mid-catch-up now bumps a per-session epoch the finished init checks, discarding the possibly-mid-drop corpus read (and the events cache it re-primed) instead of resurrecting dropped runtime state, and the healthy range is calibrated at < 0.020 s (round 1's 0.0157 s after-median exceeds M74's 0.015 s pin — the catch-up's parse+feed holds the GIL longer than M74's scan); M75 definition and healthy range introduced with this PR |
| 2026-09-07 | this PR | M74 turn-end rescan loop-lag median 0.0302/0.0262 s → 0.0110/0.0104 s, maxima 0.0377/0.0353 s → 0.0115/0.0109 s (two interleaved rounds of the collector, 9.9 MB / 391-event worst on-disk raw log of session 4fcd4c43, live home read-only, main checkout before — the pre-fix inline call shape `runs.project_raw_events(runs.parse_raw_lines(...))` — vs branch worktree after back-to-back at load 1.54/1.18/0.82, every paired round faster; earlier same-conditions pair 0.0203 → 0.0110 s at load 1.45; wall median 0.0211-0.0251 → 0.0205-0.0215 s — the scan's own CPU, now off-loop, unchanged as expected; typical recent turns 0.02-0.55 MB read the 5.4 ms ticker floor in both shapes) | every claude-family master turn ended with the model-attribution rescan inline on the event loop — a whole read+parse+project of the turn's raw log through a fresh translate, freezing every concurrent request and WebSocket at turn end (the M14 pathology on the turn-end path); the live turn-end projection hops to a thread via `runs.project_raw_file` (new whole-file helper, also single-homing `scan_result_exit` and `resolve_run`'s inline scans), and the re-attach path's whole-file result scan hops the same way; M74 definition and healthy range introduced with this PR |
| 2026-09-07 | this PR | M7 changed-round collect median 0.453/0.463/0.461 s → 0.231/0.273/0.254 s (three interleaved rounds of the new changed-round harness, main checkout before vs branch worktree after back-to-back at load 1.52-2.08, 15 rows and 0.0 MB re-read both arms; pre-fix profile on the same round: 10.5 MB document json.loads ~80 ms + db key scan ~106 ms + document re-dump ~89 ms + record replay ~138 ms + walk ~40 ms); warm row-memo advance median 0.1058/0.1058/0.1064 s → 0.0264/0.0279/0.0271 s (interleaved ×3 against the live 15.28 GB db, 85k message rows; branch maxima 0.133-0.139 s are rounds where live rows landed between calls — the probe misses and the full scan runs, correct); HTTP-level paired interleaved rounds: WAL-noise rounds (sidecar moved, no message row) live-before 0.1404/0.1419/0.1435/0.1456 s → scratch-after (branch server, scratch CHARLIEBOT_HOME, verbatim M7 curls) 0.0377/0.0382/0.0392/0.0399 s, row-landing rounds unchanged (live 0.1328-0.1576 s ≈ scratch 0.1445-0.1606 s — the scan still runs when rows land, by design); fast-hit floor unchanged (live 0.022 s ≈ scratch 0.011 s) | the changed round paid three fixed costs: the 10.5 MB cache document's json re-parse (~80 ms) on every fresh walk, the opencode row memo's full-table key re-read (`select id, time_updated` over 85k rows, ~106 ms) on every WAL-sidecar move, and the proof-less rescan's downstream serve; the row-memo advance now checks proof aggregates first — (row count, sum of time_updated) read under the same snapshot as the scan it may gate; every single-row move changes the pair, so equal aggregates skip the per-row key read (a strictly weaker proof than the key scan's per-id diff: a same-millisecond delete+insert coincidence whose count and sum both net to zero dodges the probe until the next proof miss re-scans, and a data-only rewrite with an unchanged time_updated is invisible to the key scan itself); the parsed document memoizes per cache path and each save adopts the round's next-document state, so a changed round re-parses zero document bytes and entries this round stopped seeing drop out with the save; changed-round sub-metric, healthy range, and harness added to the M7 row in this PR |
| 2026-09-07 | this PR | M73 plan amend validation loop-lag median 0.6384/0.6540/0.6360 s → 0.0073/0.0070/0.0075 s (at the 5 ms ticker floor), maxima 0.6644/0.6645/0.6385 s → 0.0079/0.0088/0.0085 s; wall median 0.6390/0.6547/0.6367 s → 0.6516/0.6319/0.6486 s — the render's own cost, now off-loop, unchanged as expected (three interleaved rounds of the collector, the 11 KB bound plan page plan_02.html passing the current pure assertion set, scratch CHARLIEBOT_HOME, main checkout before vs branch worktree after back-to-back, every paired round faster; live corroboration: the 1.37 h server log shows the freeze as POST /api/internal/plan/amend avg 670 ms max 715 ms over 5 calls and /plan/present avg 597 ms max 624 ms over 3, the instance predates this change; no-regression re-measures: M57 /plans 2.27-2.55 ms standing band and M27 tolerant read 9.0 µs unchanged; plan-registry suite 67 passed at this branch's head, the pre-fix archive one short (the new loop-responsiveness pin), red there, green here) | the plan registration gate (the DOM assertion set plus the headless-Chrome page-height render, hundreds of ms per page) ran inline in the async present/amend verbs, so every plan delivery froze the event loop for the full Chrome render — the M14 pathology on the plan-delivery path; the assertion run now rides one asyncio.to_thread hop inside `_validate_new_version_file`, the same shape as the M55 annotate and M65 gzip hops, leaving every verb's rejection and save semantics untouched; M73 definition and healthy range introduced with this PR |
| 2026-08-30 | #457 | M5 median 0.068 s → 0.029 s (117-thread worst session) | one executor hop for the threads metadata scan; M5 definition and healthy range introduced with this PR |
| 2026-08-30 | #461 | M2 tui/status share 1396/3146 status polls (44 %) → 0 tui/status requests in a 15 s headless-page window (scratch A/B) | sidebar tui/status poll scoped to rows rendered as tui-cli; zero tui-cli backends configured on this host |
| 2026-08-30 | #463 | M6 median 0.015 s → 0.011 s (20534-event worst session, live-before vs scratch-after) | one-pass event scan for usage resolution; M6 definition and healthy range introduced with this PR |
| 2026-08-30 | #468 | M7 median 10.269 s → 2.087 s (live-before vs scratch-after warm; scratch cold 11.17 s; 2.2 GB agent logs) | persisted per-file Claude tally cache keyed on (mtime, size); M7 definition and healthy range introduced with this PR |
| 2026-08-30 | #476 | M8 median 0.269 s → 0.309 s, max 1.162 s → 0.555 s; 401-during-search-storm p90 0.148 s → 0.027 s, max 0.312 s → 0.041 s (scratch A/B, 153 MB corpus) | character-window content scan bounds per-call GIL holds in sidebar search; M8 definition and healthy range introduced with this PR |
| 2026-08-30 | #481 | M9 per-round spend recompute 0.402 s → 0.002 s steady state, 0.063 s with the 5.2 MB active file changed (200 rollout files, 33 in the 7-day window, 41 MB; identical spend results) | per-file spend-event memo keyed on (mtime_ns, size) on the codex usage provider; M9 definition and healthy range introduced with this PR |
| 2026-08-30 | #489 | M10 torn reads 38932/59366 → 0/35975 concurrent reads over 3000 save_metadata calls (collector verbatim; pre-fix code also 500ed one threads/list poll in the live server log) | thread metadata.json writes routed through the repo's atomic-write rule (atomic_write_text), mirroring the session-metadata path; M10 definition and healthy range introduced with this PR |
| 2026-08-30 | #492 | M11 GET /api/backlog + /api/backlog/history 500/500 → 200/200 `[]` on the unconfigured host (live-before with 5 backlog 500s in the 16 h server log; scratch TestClient A/B for the after) | unconfigured backlog reads return the empty state /repos already reports; PATCH keeps the loud raise; M11 definition and healthy range introduced with this PR |
| 2026-08-31 | #496 | M12 median 0.0071 s → 0.0006 s, max 0.0073 s → 0.0008 s (200 rollout files, 0.98 MB newest rollout; scrape results identical modulo fetched_at) | per-newest-rollout usage memo keyed on (mtime_ns, size) plus a 1 MiB tail-window read on the codex provider; M12 definition and healthy range introduced with this PR |
| 2026-08-31 | #502 | M7 warm median 1.609 s → 0.846 s collector-level, 1.758 s → 1.158 s HTTP-level (scratch A/B: base #496 vs branch, scratch CHARLIEBOT_HOME each; 200 rollout files 180 MB, 4 Claude homes 676 MB, opencode.db 10.5 GB; tallies byte-identical) | codex rollouts joined the persisted per-file tally cache (token_count records plus the root-session self-check pair) |
| 2026-08-31 | #509 | M13 median 0.0673 s → 0.0000 s, max 0.0818 s → 0.0001 s (6.7 MB worst worker log, 2177 projected events; projection identical including timestamps) | byte-offset incremental read with per-path cached projection for the workers-panel thread-events poll; M13 definition and healthy range introduced with this PR |
| 2026-08-31 | #510 | M5 live median 0.026 s→ (scratch A/B on the 154-thread worst session's copied metadata corpus) list_threads steady state 14.72 ms → 0.96 ms, max 17.45 ms → 1.22 ms; list output byte-identical | per-file (mtime_ns, size) memo plus an os.scandir/str-path scan for the workers-panel threads poll |
| 2026-08-31 | #512 | M14 loop-lag median 0.1504 s → 0.0060 s, max 0.1616 s → 0.0061 s (463 files in charlie-bot root..HEAD; diff payload identical) | git API subprocess calls moved off the event loop via asyncio.to_thread; M14 definition and healthy range introduced with this PR |
| 2026-08-31 | #518 | M15 torn reads 24521/44026 → 0/51495 concurrent reads over 3000 _write_cache_entry calls (collector verbatim, main-checkout before vs branch after, scratch state) | recap summary-cache writes routed through the repo's atomic-write rule (write_json_atomically), mirroring the session/thread metadata paths; M15 definition and healthy range introduced with this PR |
| 2026-08-31 | #525 | M16 torn reads 87232/128449 (PR base) and 48061/71863 (main checkout) → 0/51072 concurrent reads over 3000 _save_trigger calls (collector verbatim, scratch state) | trigger-file writes routed through the repo's atomic-write rule (atomic_write_text), mirroring the session/thread metadata and recap-cache paths; M16 definition and healthy range introduced with this PR |
| 2026-09-01 | #526 | M17 fork median 0.4350 s → 0.2126 s, max 0.4850 s → 0.2554 s (5519 parent events over a 36.3 MB archive+live corpus, scratch CHARLIEBOT_HOME A/B; reference bytes identical to the pre-fix output) | full-corpus forks copy raw parent event lines into parent_reference.jsonl instead of parse+reserialize; first M17 history row |
| 2026-09-01 | #530 | M6 median 0.0119 s → 0.0046 s, max 0.0123 s → 0.0051 s (scratch TestClient A/B on the 20534-event worst session's copied corpus; live-before median 0.014 s, max 0.022 s; usage payload identical) | whole-result usage-facts memo keyed on the cached events list's identity+length (LRU cap 8); load+scan moved off the event loop into the load's to_thread |
| 2026-09-01 | #533 | M7 live-before median 1.526-1.662 s → scratch-after warm median 0.443 s, max 1.158 s (scratch-server A/B, scratch CHARLIEBOT_HOME with empty cache, cold 10.49 s; collector-level warm 0.9105 s → 0.5261 s, warm re-scan 10.4 MB → 0 bytes; cached vs cacheless tallies byte-identical at a pinned db signature) | opencode db contribution joined the persisted tally cache, signatured on the main db file plus its WAL sidecar |
| 2026-09-01 | #535 | M18 241 → 0 poll fetches per simulated 10 hidden min (one expanded running worker + ext-usage strip; 1 bootstrap fetch excluded; 10 s visible re-check 4 fetches before and after) | workers-panel thread-detail poll and ext-usage strip timers registered through page-timers like every other timer; M18 definition and healthy range introduced with this PR |
| 2026-09-01 | #538 | M19 median 2.6548 s → 0.1151 s, max 2.8682 s → 0.1377 s (16.0 MB payload in 16 KB chunks, ~1 MB frames; framed line stream identical) | terminator search resumes at the proven-clean remainder cursor instead of re-scanning the whole remainder per chunk; M19 definition and healthy range introduced with this PR |
| 2026-09-01 | #542 | M20 repeat-divider extract median 0.2026 s → 0.0000 s, max 0.2429 s → 0.0000 s (5519 events over 36.3 MB corpus, scratch CHARLIEBOT_HOME A/B; extraction digest identical bb99828aa5b6; live-before recap HTTP median 0.305 s, max 0.538 s on the same session) | (session_id, divider end) LRU memo for extract_recap with a count-free explicit-divider hit path; M20 definition and healthy range introduced with this PR |
| 2026-09-01 | #545 | M8 median 0.2430 s → 0.0155 s, max 0.2504 s → 0.0176 s (scratch A/B, identical 147.6 MB / 33-session corpus, identical result sets; live-before median 0.235 s, max 0.259 s; remaining 15 ms is metadata loading — memoized repeats read zero corpus bytes) | per-chat-file proven-absent-needle LRU memo keyed on (mtime_ns, size); identical and superstring queries serve with one stat per file, appends re-read |
| 2026-09-01 | #549 | M21 steady-state sweep median 0.0353 s → 0.0068 s, max 0.0358 s → 0.0070 s (33 active sessions, live corpus; 33 → 0 deep probes per sweep; cold sweep unchanged at 33; probe entries identical) | stat-only probe-input signature (thread metadata/trigger/plans (mtime_ns, size) + name sets + 30-day rollover) narrows the 10th-poll self-heal sweep; force=1 keeps the full deep probe; M21 definition and healthy range introduced with this PR |
| 2026-09-01 | #554 | M22 180 → 0 ext_usage_unknown_limit_shape warnings per 60 steady-state transform rounds (collector verbatim, main-checkout before vs branch after; transform windows identical over the 60-round repeat; live-log corroboration 1536 lines in 20.47 h ≈ 75/h) | all eight emitters of the event routed through a (provider, account, slot, reason) warn-once guard — one alarm per unrecognized shape per process; M22 definition and healthy range introduced with this PR |
| 2026-09-01 | #560 | M23 8-page scroll steady-state median 0.0926 s → 0.0022 s, max 0.0952 s → 0.0024 s (collector verbatim, main-checkout before vs branch after; 8.2 MB across 2 weekly archive files, archive_offset 2799, scratch CHARLIEBOT_HOME A/B under load 4.49/4.58/4.61; page contents asserted identical) | per-archive-file parsed-events memo keyed on (mtime_ns, size) with a 16-file LRU, mirroring the live events cache; M23 definition and healthy range introduced with this PR |
| 2026-09-01 | #566 | M24 steady-state median 0.0164 s → 0.0006 s, max 0.0222 s → 0.0008 s (collector verbatim, main-checkout before vs branch after; 78-file worst trigger corpus, live state read-only; serialized list output over the top-5 trigger sessions identical) | per-trigger-file parsed-record memo keyed on (mtime_ns, size) with a name-set diff for deletions and a 32-session LRU; M24 definition and healthy range introduced with this PR |
| 2026-09-01 | #568 | M25 steady-state loop-lag median 0.0115 s → 0.0001 s (0.000083 s wall), max 0.0116 s → 0.0001 s (0.000091 s wall) (collector verbatim, main-checkout before vs branch after at load 0.36/0.55/0.62 and 0.64/0.60/0.63; live config corpus read-only; after run under the 5 ms ticker resolution) | scheduler _reload_config routed through the fingerprint-cached get_config instead of a per-tick load_config parse; M25 definition and healthy range introduced with this PR |
| 2026-09-01 | #572 | M26 projection advance median 46.69 ms → 0.19 ms, max 100.40 ms → 0.27 ms (collector verbatim, 8 single-event appends on the 20534-event worst live corpus, scratch CHARLIEBOT_HOME A/B, main checkout before at load 2.35/1.88/1.51 vs final branch head after at load 1.35/1.32/1.75; served-view digest identical e94c56635194) | append-incremental message projection: closed-prefix single feed plus cloned-aggregator open-region view, advanced copies swapped in atomically; M26 definition and healthy range introduced with this PR |
| 2026-09-01 | #576 | M27 steady-state tolerant read median 319.3 µs → 10.6 µs, max 355.6 µs → 41.5 µs (collector verbatim, main checkout before at load 1.28/1.17/1.15 vs final branch head after at load 0.91/0.97/1.02; worst plans corpus 15.4 KB / 12 plans, live state read-only; projected payload identical) | per-file (mtime_ns, size) memo with a 32-path LRU for read_plans_tolerant, mirroring the M24 trigger-list memo; OSError reads answered fresh every call per the sibling memo policy; M27 definition and healthy range introduced with this PR |
| 2026-09-02 | #581 | M28 parse_ndjson_tail median 50.20 ms → 17.34 ms, max 52.47 ms → 18.35 ms; count_ndjson_lines median 46.68 ms → 13.13 ms (collector verbatim, 36.3 MB / 5519-line worst live chat file, main checkout before vs final branch head after at load 0.54-0.71; tail events and line counts identical) | newline counting per 1 MiB chunk through a numpy SIMD compare replacing Python per-line iteration (~3.4 GB/s vs ~0.7 GB/s measured on this host), file-iteration count contract preserved; M28 definition and healthy range introduced with this PR |
| 2026-09-02 | #588 | M29 steady-state listing median 9.59 ms → 2.03 ms, max 11.64 ms → 2.30 ms (collector verbatim, 34 active sessions / 976 session dirs, live corpus read-only, main checkout before vs branch after back-to-back at load 0.84/0.90/0.87; full listing byte-identical) | os.scandir DirEntry names with d_type is_dir replacing Path.iterdir()+per-entry stat in the _load_session_metas preamble; M29 definition and healthy range introduced with this PR |
| 2026-09-02 | #592 | M7 scratch-server warm median 0.647 s → 0.326 s, max 0.667 s → 0.344 s, cold 20.9 s → 9.4 s at load 1.3-1.9; collector-level paired medians: quiet-db 0.62 s → 0.37 s, rescan-db 1.50-1.67 s → 1.02 s; live-before median 1.721 s measured against pre-#510/#533 live code (flagged); cacheless tally rows+notes digests pairwise identical | in-process aggregate memo serves the merged Claude/Codex partial keyed on the walk signature, and the opencode db scan projects its eight tally fields through json_extract; document save moves to the miss path |
| 2026-09-02 | #597 | M19 median 0.1072 s → 0.0247 s, max 0.1136 s → 0.0257 s (collector verbatim, main checkout before vs final branch head after back-to-back at load 0.80/0.99/1.07; framed line stream identical, 32 lines; LF-dense 1.4 MB / 64 KB-chunk production shape 0.0922 s → 0.0840 s) | chunk terminator search through cached-CR str.find passes instead of the alternation regex (re engine ~0.087 s per 16 MB measured vs memchr speed), piecewise fragment accumulation instead of concatenating the accumulated remainder per chunk |
| 2026-09-02 | #600 | M30 8-page live-half scroll steady-state median 0.0453 s → 0.0002 s, max 0.0500 s → 0.0002 s (scratch CHARLIEBOT_HOME A/B, main checkout before vs final branch head after back-to-back at load 0.82/0.59/0.74; 6.3 MB live file of archived session 3b91d606, archive_offset 1136, total 3760 events; collector verbatim on the branch 0.0004 s median) | per-physical-line parsed-events memo on the live file keyed on (mtime_ns, size), 4-file LRU, gated to archive_offset > 0 sessions (unarchived ones paginate via the M26 projection), mirroring the M23 archive memo; M30 definition and healthy range introduced with this PR |
| 2026-09-02 | #603 | M31 steady-state median 0.0377 s → 0.0016 s, max 0.0605 s → 0.0021 s (collector verbatim, 6.7 MB worst worker log, live state read-only, main checkout before at load 0.58/1.48/1.29 vs branch after at load 0.91/1.50/1.30; summary output identical on the worst log; sibling review-context delegation scan 159.4 → 13.4 / 60.9 / 148.2 ms medians by front/mid/tail match position, context identical) | read_events_summary collects the last-80 parseable events through 512 KiB from-the-end segments (parse_ndjson_tail_parseable) instead of full-parsing the log; extract_review_context streams to the thread's first task_delegated instead of full-parsing the chat log; M31 definition and healthy range introduced with this PR |
| 2026-09-02 | this PR | M32 steady-state median 3.88 ms → 0.57 ms, max 3.95 ms → 0.67 ms (collector verbatim, 68 entry files, live memory corpus read-only, main checkout before at load 0.92/0.71/0.69 vs branch after at load 0.47/0.62/0.66, back-to-back; assembled block sha256-identical c561298a159a) | load_store memoized on a stat-only (path, mtime_ns, size) signature over every parsed file, walked with os.scandir + string joins (Path.glob/relative_to allocation would dominate the stats); only successful loads memoize, a corrupt rewrite drops the stale hit; M32 definition and healthy range introduced with this PR |
| 2026-09-02 | this PR | M33 replay wall median 2.624 s → 0.589 s, max 2.829 s → 0.649 s (collector verbatim, 98.0 KB worst on-disk assistant draft sha1 6e0cb6e8f159, 502 deltas at 40 ms virtual cadence, 502 → 102 paints, main checkout before vs final branch head after back-to-back at load 0.22/0.43/0.87; final-frame parity true both arms) | stream-draft paints coalesced to a 200 ms leading+trailing cadence, hideStreaming cancels the pending trailing paint (every terminal path hides first: committed bubble, error, session swap); M33 definition and healthy range introduced with this PR |
| 2026-09-02 | #615 | M8 append-round absent-needle search median 0.1797 s → 0.0021 s, max 0.1901 s → 0.0025 s (scratch CHARLIEBOT_HOME A/B over the 5 biggest active sessions, 109.2 MB, one event appended to every file before each timed round; main checkout before at load 0.87/2.71/3.37 vs branch after at load 0.97/1.51/2.61; cold full scan 0.1975 s → 0.1466 s under the same load shift; absent-round and positive-round parity both arms) | proven-absent memo signature gains the inode, and a same-inode file that grew re-proves absence from a window over the appended tail (old_size − 4·len(query) − 8 seek) instead of a full re-scan — chat files mutate only by append between inode-swapping atomic archive rewrites; whole-file path keeps strict decoding |
| 2026-09-02 | this PR | M34 events poll full fetch 0.0097 s / 860706 B → after=total 0.0035 s / 40 B (collector verbatim, 6.7 MB / 2177-event worst on-disk log, scratch CHARLIEBOT_HOME TestClient, main full arm vs branch full arm back-to-back at load 0.43/0.66/0.86, payload sha256-identical deb85be56bf4dbba; live-before corroboration 0.010 s / 860706 B warm from the running instance; prefix+tail reconstruction parity true, envelope rows byte-equal to plain-list rows) | events endpoint gains after=N envelope (append-only prefix cut on the projection, reset+full payload when the count is ahead) served through a model_dump JSONResponse — FastAPI's jsonable_encoder fallback for mapped returns measures 6x slower on the same list (48.8 ms vs 8.1 ms); client polls pass the rendered raw count and append tails via scratch paint + insertAdjacentHTML; M34 definition and healthy range introduced with this PR; M18 collector element stub gains dataset to match the real DOM |
| 2026-09-02 | this PR | M35 events page median 21.99 ms → 14.75 ms, max 24.88 ms → 16.00 ms (558888 B, 211 msgs); view median 17.26 ms → 15.55 ms; bootstrap median 8.41 ms → 6.54 ms (collector verbatim, 20534-event worst live corpus snapshot, shared scratch CHARLIEBOT_HOME TestClient A/B, main checkout before vs final branch head after back-to-back at load 1.27/1.08/0.87 and 1.39/1.11/0.88; all three bodies sha-identical across arms: events f552ad6de73b, view 33e20ccff76d, bootstrap 61b3ca97cd5e) | events/view/bootstrap message-page handlers return JSONResponse directly, skipping FastAPI's jsonable_encoder pass (~3x a plain json.dumps on the 559 KB page); M35 definition and healthy range introduced with this PR |
| 2026-09-02 | #628 | M17 fork median 0.2178 s → 0.1769 s, medians of three interleaved verbatim-collector rounds (5519 parent events, 36.3 MB corpus, scratch CHARLIEBOT_HOME A/B, main checkout before vs final branch head after at load 4.7-5.8; every paired round faster: 0.324 → 0.211, 0.218 → 0.177, 0.190 → 0.140; reference bytes byte-identical across arms on the corpus and on 12 synthetic framing shapes; earlier light-load phase profile: per-line str strip+join+encode ~190 ms of the fork's 140 ms) | full-corpus reference streams raw line bytes per source file into the atomic tmp sibling (new atomic_write_stream in json_utils) instead of decode→strip→join→encode in one big str: bulk numpy `{}`-shape check answers the common all-plain shape with one window write, per-frame fallback keeps CR folding and corrupt-line rejection; utf-8 validity keeps the text-mode read's UnicodeDecodeError parity |
| 2026-09-02 | this PR | M36 poll median 15.60 ms → 7.98 ms, max 48.64 ms → 8.75 ms, body 1799215 B → 131896 B (collector verbatim, 1963 KB / 266-row worst worker-list corpus, 265 rows truncated after; main checkout before vs final branch head after back-to-back at load 0.91/0.98/1.02 and 1.00/1.00/1.02; card renders the identical one-line prefix; full text fetches on modal click only; earlier same-corpus round median 16.37 ms before vs 8.27 ms after at load ≤1.0) | workers-panel list rows ship a 240-char description prefix plus a description_full_len marker instead of whole task-spec-length descriptions; the full-text modal fetches the thread row on demand; M36 definition and healthy range introduced with this PR |
| 2026-09-03 | this PR | M7 live-before median 15.884 s, max 21.738 s (busy host, load 5.53/7.30/11.03; 16.4 GB db whose WAL moves every ~5 s, so the file-signature entry misses every load) and max 16.801 s in the quiet re-pair → scratch-after warm median 0.422 s, max 0.437 s (scratch server on the branch with an empty scratch CHARLIEBOT_HOME, cold 20.996 s; verbatim M7 commands back-to-back at load 1.07/0.91; collector-level over the live cache doc: opencode rescan 12.928 s → 0.164 s, warm collect total 16.996 s → 0.49 s) | per-row (id, time_updated) memo over the opencode message table: a leaf-page key diff plus per-id fetch of moved rows replaces the whole-table re-scan per WAL-invalidated load; rows the scan SQL filters out memoize as non-contributors; record parity pinned by the suite |
| 2026-09-03 | this PR | M37 steady-state median 9.12 ms → 0.01 ms, max 12.30 ms → 0.03 ms (collector verbatim, 7.4 MB / 3081-line worst archived-session live file of session 3b91d606, live state read-only, main checkout before at load 1.72/2.44/2.12 vs branch-after at load 1.23/1.69/1.86, back-to-back; served tail page repeat-identical in both arms) | whole-page (mtime_ns, size) memo for parse_ndjson_tail plus a shared (mtime_ns, size) memo for count_ndjson_lines, both keyed on the pre-read signature so an entry recorded during a concurrent append can never be served for the newer bytes; M37 definition and healthy range introduced with this PR |
| 2026-09-03 | #646 | M36 poll median 13.92 ms → 5.46 ms, max 18.28 ms → 6.43 ms, body 137363 B in both arms (collector verbatim, 2051 KB / 277-row worst worker-list corpus, live state read-only, main checkout before vs branch head after back-to-back at load 3.81/2.27/1.64 and 2.76/2.17/1.63; 100-rep interleaved corroboration min 5.79 ms → 3.19 ms) | whole-body memo for the 3 s workers-panel list poll keyed on the union (path, mtime_ns, size) signature of every thread metadata.json and trigger *.json behind the rows (all writers rename atomically, so an unchanged signature proves the body current); steady-state polls skip row building and JSON serialization |
| 2026-09-03 | this PR | M38 turn replay 174 → 2 stream frames, 286 → 114 json.dumps calls, 78 ms → 5 ms dumps total, replay wall 0.08 s → 0.01 s (collector verbatim, session ebace12d worst on-disk stream turn by preview bytes, instant feed, one subscriber, main checkout before at load 3.02/5.02/5.05 vs branch head after at load 1.74/4.20/4.76; final-frame parity true both arms) | per-channel leading+trailing coalescing of stream preview frames in StreamingManager at the client's 200 ms paint cadence, pending draft dropped only on preview-hiding frame types (message/assistant_error/error), one json.dumps per fan-out replacing one per subscriber, subscriber-less channels short-circuited to a dict lookup; M38 definition and healthy range introduced with this PR |
| 2026-09-03 | this PR | M21 steady-state sweep median 0.0088 s → 0.0045 s, max 0.0092 s → 0.0048 s (collector verbatim, 38 active sessions, live state read-only, main checkout before vs branch head after back-to-back at load 0.76/1.50/1.31; signature-pass microbench 8.8 ms → 4.0 ms with signatures identical over every active session) | os.scandir + str-joined os.stat in the sidebar probe-input signature pass, replacing per-entry Path()/__truediv__ allocation whose parse overhead measured ~half the sweep, mirroring the M29/M32 str-stat pattern |
| 2026-09-03 | this PR | M39 inline loop-lag median 15.34 ms → 5.34 ms (5 ms ticker floor), max 16.24 ms → 5.47 ms; busy-check wall median 15.34 ms → 0.26 ms, max 16.24 ms → 0.43 ms (collector verbatim, 942-dir ~/.claude/projects corpus, synthetic never-present id, main checkout before at load 0.25/0.34/0.74 vs branch head after at load 0.84/0.80/0.82; busy result False in both arms) | per-session transcript-path memo in _find_existing_claude_jsonl (stable hit memoized for the process life with an exists() recheck, miss re-globbed at a 30 s TTL) plus the tui/status busy check awaited in a thread instead of inline on the event loop; M39 definition and healthy range introduced with this PR |
| 2026-09-03 | #665 | M40 starred list median 12.80 ms → 3.25 ms, group-name reduction median 13.63 ms → 3.05 ms; maxima 35.92 ms → 25.02 ms / 14.20 ms → 3.82 ms (collector verbatim, main checkout before vs branch head after back-to-back at load 2.90/2.82/2.16 and 3.15/2.90/2.22; live 1012-meta corpus read-only, 10 starred rows, 25 groups; list outputs identical; the residual starred maxima are dirty deep-probe spikes in the unchanged enrich path) | starred/scheduled filters run against the shared cached metas (read-only) before the model_copy+thinking stamp, so only surviving rows pay the leaving-the-manager copy; /api/sessions/groups and the autonamer's group list go through list_group_names, a copy-free read-only reduction of the cached metas; M40 definition and healthy range introduced with this PR |
| 2026-09-03 | this PR | M41 first view 0.1781 s → 0.1217 s, repeat-view median 0.1766 s → 0.0063 s, max 0.1874 s → 0.0069 s (collector verbatim, 492 files in the charlie-bot root..HEAD diff, main checkout before at load 1.20/1.21/0.98 vs branch after at load 0.49/0.89/0.93, back-to-back; manifest body sha256-identical ed990917efcc across arms; M14 loop-lag re-measured unchanged, 0.0058 s → 0.0060 s at the 5 ms ticker floor) | git diff/files manifest memoized on (repo, resolved base/head SHAs, mode, .gitattributes signature) — a commit-pair diff is immutable — plus both refs resolved in one rev-parse and the two manifest diffs run concurrently on a miss; M41 definition and healthy range introduced with this PR |
| 2026-09-03 | this PR | M42 steady-state tick loop-lag median 0.0135 s → 0.0060 s (the 5 ms ticker floor), max 0.0146 s → 0.0071 s (collector verbatim, 12 enabled tasks over the 1012-session live corpus, fire stub awaited 0x in both arms, main checkout before vs final branch head after back-to-back at load 1.06/1.16/1.16; first standalone round 0.0102 s before) | scheduler tick's per-task session cache built via the M40 scheduled=True pre-copy filter: only the 52 scheduled rows pay model_copy+thinking stamp instead of all ~1012 cached metas; M42 definition and healthy range introduced with this PR |
| 2026-09-03 | this PR | M43 repeat-expand median 0.0441 s → 0.0061 s, max 0.0541 s → 0.0063 s; first expand 0.0549 s → 0.0503 s (collector verbatim, heaviest charlie-bot root..HEAD manifest file web/static/css/tailwind.css +2515/-0, 45756 B, main checkout before vs branch head after back-to-back at load 1.20/1.13/1.02; bodies repeat-identical in both arms; live log corroboration 8 diff/file requests in the 24.4 h server log, one a re-expand) | per-file diff body memoized on the M41 manifest key plus the pathspec, miss-path range spec built from the resolved SHAs; M43 definition and healthy range introduced with this PR |
| 2026-09-03 | this PR | M29 steady-state median 1.67-1.96 ms → 0.84-0.90 ms across five interleaved verbatim-collector rounds (1019 session dirs, 44 active, live corpus read-only, main checkout before vs branch after back-to-back at load 1.2-2.0; listings identical in both arms; earlier same-day round under the sibling CUDA build's load 7-12 read main 6.08 ms vs the <5 ms healthy range, recovering to 1.69-1.96 ms once the build drained at identical code — load noise, no regression) | session-dir name list memoized on the sessions root's own (mtime_ns, size), signature taken before the scandir: session create/delete is what moves the root's mtime (metadata writes land one level below), so an unchanged signature proves the name set current and steady-state listings pay one stat instead of the ~1 ms per-1000-entry scandir |
| 2026-09-03 | this PR | M44 steady-state median 5.11 ms → 2.90 ms, max 8.63 ms → 6.29 ms (collector verbatim, 12 scheduled rows / 13194 B body, live session + cron corpus read-only, main checkout before vs branch after back-to-back at load 0.36/1.28/2.58 and 0.49/1.29/2.57; body digest identical ec0f7b654411 both arms; component corroboration: croniter get_next measured 2.97 ms over the 12-task corpus, one expand each) | per-(cron, timezone) memo for the /scheduled rows' next-fire resolution, entries valid until the fire time they name passes — get_next is a pure function of (cron, timezone, now) and no occurrence can land before that first next fire, so repeat requests inside the window recompute an identical string; M44 definition and healthy range introduced with this PR |
| 2026-09-03 | this PR | M7 collector-level warm collect median 0.3767 s → 0.0729 s, max 0.4274 s → 0.3471 s (verbatim warm-collect command, 16 rows / 3 notes, main checkout before at load 1.43/1.50/1.40 vs branch after at load 1.48/1.51/1.40 back-to-back; rows+notes digest agreement across interleaved arms whenever the live corpus held still between them — opencode db WAL moves every few seconds under live traffic; the after max is one such WAL-moved memo miss still paying the old fresh path; suite pins hit serves the first collect's rows/notes with zero scanned bytes) + HTTP-level live-before warm median 0.336 s → scratch-after warm median 0.056 s (verbatim M7 curls, live-before at load ~1.75, scratch server on the branch with a scratch CHARLIEBOT_HOME, cold 9.25 s) | whole-tally memo keyed on the walk signature plus the opencode db signature serves repeat collects without the cache-document JSON parse (~0.09 s), the apply(t) record replay (~43.5k add calls, ~0.075 s) or the db open per load, and the signature walk itself switches to str joins + raw os.stat (Path construction measured over twice the stat syscall on this corpus), with one shared os.walk error-hook home for both walkers |
| 2026-09-04 | this PR | M45 stale-cursor replay loop-lag median 0.0259 s → 0.0102 s, max 0.0281 s → 0.0102 s (collector verbatim, 20534-event worst live corpus at cursor 20484, 47 frames replayed, scratch CHARLIEBOT_HOME A/B, frame-list digest identical 314dfbe9fd89 across arms; main checkout before vs branch after back-to-back at load 1.77/1.32/0.91 and 1.79/1.33/0.91; replay wall 0.0259 s → 0.0242 s — the walk's CPU still runs, now off the loop; suite pins ws.sent == _catchup_frames output and the stop-at-first-failure send count) | session-WS catchup replay's full-history aggregator walk moved off the event loop: `_catchup_frames` builds the ordered frame list via asyncio.to_thread and `_replay_aggregated_catchup` only sends it in order; M45 definition and healthy range introduced with this PR |
| 2026-09-04 | this PR | M46 GET /api/cron/tasks body 96235 B → 3745 B (resolved-prompt bytes 90348 → 0 of 12 task rows), handler median 2.71 ms → 1.90 ms, max 3.12 ms → 2.31 ms (collector verbatim, live cron corpus read-only through TestClient, main checkout before at load 0.40/0.77/0.82 vs branch head after at load 1.02/1.22/0.93, back-to-back; row keys identical minus prompt; live-log corroboration 96 KB fetch bodies) | cron tasks list dump excludes prompt, the resolved body the in-process scheduler/master reads while every consumer of the route edits prompt_file (POST/PUT responses never carried it), mirroring M36's description prefix cut; M46 definition and healthy range introduced with this PR |
| 2026-09-04 | this PR | M47 60 → 0 claude_declared_window_degraded warnings per 60 steady-state resolutions (collector verbatim, main checkout before vs branch after; live-log corroboration 62 lines in the 7.89 h server log ≈ 8/h, host exports CLAUDE_CODE_MAX_CONTEXT_TOKENS=400000) | declared-window degradation warnings routed through an (event, variable, value) warn-once guard — one line per degradation per process, a swapped bad value earns one new line; M47 definition and healthy range introduced with this PR |
| 2026-09-04 | this PR | M48 60 → 0 search_read_failed debug lines per 60 steady-state content scans of a no-live-file session (collector verbatim, main checkout before vs branch after back-to-back at load 0.75/0.78/0.64; live-log corroboration 30 lines in the 11.92 h server log, all naming the one active session with no live chat file; search results and M8 monitoring shape unchanged) | both search content-scan failed-read loggers (the stat failure and the scan-open failure) routed through a (session, error) warn-once guard — one line per reported failure per process, a swapped path or errno earns one new line; M48 definition and healthy range introduced with this PR |
| 2026-09-04 | this PR | M49 60 → 0 opencode_part_unhandled debug lines per 60 steady-state parts of one unhandled type (collector verbatim, main checkout before vs branch after back-to-back at load 0.25/0.59/0.86; live-log corroboration 152 lines in the 12.88 h server log, every line type=patch; unhandled parts still translate to []) | the part type falling through `_translate_part` routed through a part-type warn-once registry — one line per unmapped type per process; M49 definition and healthy range introduced with this PR |
| 2026-09-04 | this PR | M50 60 → 0 ext_usage_no_access_token warnings per 60 steady-state credential reads of a tokenless file (collector verbatim, main checkout before at load 0.97/0.75/0.66 vs branch after at load 0.29/0.59/0.62, back-to-back; live-log corroboration 135 lines in the 13.89 h server log ≈ 10/h, every line ext_usage_no_access_token naming the same path; every read still returns None and a token-bearing read re-arms the path) | both `_read_credentials` failure-site warnings (ext_usage_credentials_not_found, ext_usage_no_access_token) routed through a recovery-aware warn-once registry — one line per (event, path) per broken streak, a read returning a token re-arms the path; M50 definition and healthy range introduced with this PR |
| 2026-09-04 | this PR | M51 post-write deep probe median 25.82/25.62 ms → 6.12/6.30 ms, max 26.22/25.98 ms → 6.33/8.04 ms (collector verbatim, 339-file worst threads corpus, scratch CHARLIEBOT_HOME, main checkout before vs branch after interleaved back-to-back ×2 at load 1.16-1.70/1.18-1.32/0.95-1.04; probe verdicts identical — unchanged-sig sweep re-measured 0.0021 s vs 0.0022 s standing M21, no regression) | the sidebar deep probe's thread-metadata scan (`iter_recent_thread_metas`, shared with the boot recovery scan) memoizes each in-window metadata file's parsed dict on (path, mtime_ns, size) — every writer publishes through the atomic tmp-file rename so a content change always moves mtime_ns, the signature is taken before the read so a mid-rewrite entry keys the older signature, only successful parses memoize, and yielded dicts are shared read-only; a post-write probe re-parses the moved file alone instead of all 339; M51 definition and healthy range introduced with this PR |
| 2026-09-04 | this PR | M36 unchanged-poll body 168209 B → 0 B (conditional ?etag= repeat answers 204; collector with the conditional round, 2551 KB / 339-row worst worker-list corpus, live state read-only, main checkout before vs branch head after back-to-back ×2 at load 0.85/1.13/0.89 and 1.42/2.84/2.64; full-poll first-paint path unchanged — medians 4.27/4.66/6.09 ms → 4.25/5.06/4.57 ms with the byte-identical 168209 B body; live read-only check confirms the running instance still serves 200 full) | the list body carries a strong content-addressed ETag (sha1 of the body bytes) plus Cache-Control: no-store; the poll repeats the ETag it rendered via ?etag= and the whole-body memo's unchanged signature serves a bodyless 204 — the client keeps its rendered rows and skips the 339-row JSON.parse while nothing behind the list moved (a query param, not If-None-Match, because the browser's HTTP cache fulfils a revalidation itself and fetch never surfaces the 304); conditional sub-metric added to the M36 definition and collector in this PR |
| 2026-09-04 | this PR | M52 save_chat_event append median 346/382 µs → 176/184 µs, maxima 2353/2477 µs → 1977/1963 µs (collector verbatim, 20534-event worst live corpus, scratch CHARLIEBOT_HOME, main checkout before vs branch after interleaved back-to-back ×2 at load 1.25/1.79/2.09; appended lines parse back in order both arms; isolated-component check: raw open+write+close inline 19 µs, one executor round-trip ~104 µs under the same load) | chat-event appends left aiofiles' open+write+close — three executor round-trips, ~355 µs — on the streamed-turn delta path (one append per stream delta, each gating its broadcast; a 500-delta turn paid ~180 ms of append overhead); one to_thread hop around a raw open(O_APPEND)+write+close keeps the off-loop write rule at one round-trip, O_APPEND re-resolves the path per call so an atomic archive rewrite or recreate never lands behind a stale handle, and the write loop keeps the io stack's write-all contract; the same funnel serves the worker events-log appends (src/agents/worker.py); M52 definition and healthy range introduced with this PR |
| 2026-09-04 | this PR | M30 append-round 8-page scroll median 0.0406/0.0417 s → 0.0003/0.0003 s, max 0.0515 s → 0.0005 s (collector + append-round rounds, 9.4 MB live file of archived session 3b91d606, archive_offset 1136, scratch CHARLIEBOT_HOME A/B, main checkout before vs branch after interleaved back-to-back ×2 at load 0.65-1.93; live-half event count 3840 identical across all four arms; unchanged-file steady state re-measured 0.0002 s both arms, no regression) | the live-half memo extends on a same-inode size growth, re-parsing only the appended tail instead of the whole file — chat files mutate only by append between archive rewrites and the rewrite publishes through os.replace (new inode), so the inode rules out reading a rewrite as append growth; the covered byte offset tracks the bytes actually parsed (an entry whose read raced a landing append keys its pre-read stat and stays reachable only as an extension base), and a covered content ending mid-line blocks extension until a full re-parse lands on a line boundary, so a completed append is never glued onto a half-parsed last line; append-round sub-metric added to the M30 definition and collector in this PR |
| 2026-09-04 | this PR | M3 standing collector median 0.001 s, max 0.001 s (unchanged; live server runs pre-fix code, flagged); in-process raw-ASGI A/B of the real app stack, identical harness command both arms (import server, drive GET /api/sessions/status, 300 timed calls after 20 warm), main checkout before vs branch after back-to-back ×2 at load 1.67-1.78: 401 no-auth path median 137/141 µs → 81/78 µs, max 717/923 µs → 111/156 µs; 200 bearer path (one live session id) median 599/615 µs → 425/433 µs, max 1008/974 µs → 970/767 µs | AuthMiddleware rewritten from BaseHTTPMiddleware to pure ASGI — the wrapper's per-request task plus anyio memory-stream round-trip were the middleware floor's dominant slice (M3's timing target); the 401 branch synthesizes its HTML/JSON responses by hand and the authorized branch forwards the scope untouched |
| 2026-09-04 | this PR | M7 live-before warm median 0.327 s and 0.751 s across two rounds, max 0.926 s (verbatim M7 curls against the running instance at load 0.82-0.91) → scratch-after warm median 0.056 s, max 0.574 s (scratch server on the branch with a scratch CHARLIEBOT_HOME, cold 9.516 s; verbatim M7 curls back-to-back with the live rounds); in-process forced-WAL-miss A/B: before median 0.3557 s → after median 0.1406 s (collect_token_usage interleaved back-to-back over the live corpora with the db signature forced to move every round — the trigger a WAL write produces, rows untouched; rows+notes digests identical on the 8 signature-stable rounds of 9, one skipped round's corpus moved mid-pair) | whole-tally memo gains a second hit tier keyed on the row memo's change epoch: the moved-WAL miss pays one incremental key-diff scan instead of the 12.9 MB cache-document load plus the ~50k-record opencode replay, an unchanged epoch re-serves the memo rows and re-signs them at the scan's own signature, and the cache document loads only on the source-walk path that needs it |
| 2026-09-04 | this PR | M35 events page median 9.29/9.41/8.59 ms → 6.10/5.58/5.61 ms (633236 B), view median 8.51/8.66/8.32 ms → 6.89/6.88/6.50 ms, bootstrap median 3.69/3.96/3.76 ms → 3.39/3.39/3.30 ms (three interleaved rounds of the verbatim collector, main checkout before vs branch after back-to-back on the shared M35 snapshot at load 4.9-5.8; every paired round faster; parsed bodies equal across arms — raw bodies differ by design, \uXXXX escaping) | the five hot JSONResponse sites (events pages, view, bootstrap, worker-events envelope) render through FastJsonResponse, whose render is a plain ASCII-escaped dumps — CPython's C JSON encoder is ~3x faster with ensure_ascii=True on CJK-bearing payloads (render 5.4 vs 1.9 ms measured on the 559 KB page; the sessions corpus here is Chinese-heavy) and never slower on ASCII-only ones; NaN/Infinity still raise at render time, and the M34 steady-state envelope re-measured unchanged (0.0024 s median) |
| 2026-09-04 | this PR | M51 post-write deep probe median 6.83/7.01/6.26 ms → 3.47/3.62/3.32 ms, maxima 7.08/7.71/6.66 ms → 3.62/4.51/3.38 ms (collector verbatim, 339-file worst threads corpus of session 3b91d606, scratch CHARLIEBOT_HOME, main checkout before vs branch after interleaved back-to-back ×3 at load 2.5-3.4, every paired round faster; M21 unchanged-signal sweep re-measured 0.0023 s vs the 0.0038 s standing row, no regression) | the deep probe's thread-metadata scan (`iter_recent_thread_metas`, shared with the boot recovery scan) walks scandir's plain strings — `entry.path + "/metadata.json"` into raw `os.stat`, str memo keys, string yields, Path built only at the rare content read and the interrupted-run collection point — dropping the two per-entry Path allocations plus the Path.stat() indirection that measured 5.4 ms of the 339-file walk against 1.3 ms for the same files through raw os.stat, the same conversion the M21 signature pass took |
| 2026-09-04 | this PR | M53 61 → 1 warnings and 61 → 1 re-parses per 61 calls (onset 1 + 60 steady-state, collector verbatim, scratch broken-config corpus, main checkout before vs branch after interleaved back-to-back ×2 at load 3.03-3.12; call wall median 0.45/0.46 ms → 0.08/0.08 ms on the collector's minimal corpus, with the live corpus's full parse measured at 9.25 ms — the per-request cost while the live home was broken; fingerprint-move round re-parse 1 both arms, new warnings 1 → 0; served config identity `is`-asserted across all 60 steady-state calls both arms; live-log corroboration 4431 config_reload_failed lines in the 24.9 h server log, ~1/s inside the 16:00-18:00 burst window, three distinct error strings) | get_config memoizes the failed reload on its fingerprint — re-parse only when a file moves, the same freshness rule the success path follows, so an unchanged broken corpus costs one fingerprint stat set per call instead of a full parse + warning per call — and routes config_reload_failed through a warn-once registry, one line per error string per process, cleared on a successful load so a later relapse earns one new line; M53 definition and healthy range introduced with this PR |
| 2026-09-04 | this PR | M54 paint-work median 0.299/0.323 s → 0.141/0.142 s, maxima 0.390/0.406 s → 0.219/0.236 s (collector verbatim, 11.4 KB worst fence-bearing on-disk assistant draft sha1 b155f860788f, 59 deltas at 40 ms virtual cadence, 12 paints, page-pinned marked + hljs 11.9.0 common build (36 languages), main checkout before vs branch after interleaved back-to-back ×2 at load 2.24/1.15/0.80; final-frame parity true both arms; M33 stubbed-hljs replay re-measured 0.396 s median vs the 0.378 s standing row at a higher load, no regression) | highlight results memoized in renderer.code on (lang, code) with a 32-entry LRU — highlight is a pure function of its inputs, but every streaming paint re-parsed the whole draft and re-ran highlightAuto (a 36-language scoring pass, ~0.26 s per 24 KB measured) on every unchanged code block, and every message re-render paid it again; M54 definition and healthy range introduced with this PR |
| 2026-09-04 | this PR | M8 interleaved-family warm search: family B median 145.22 ms → 4.08 ms, family A 4.78 ms → 3.99 ms (two unrelated absent-needle families alternating over 6 interleaved rounds, collector over the identical 155 MB / 40-file snapshot corpus, main checkout before vs branch after back-to-back; rows 0 both families both arms; content scans across the 12 timed rounds 240 → 0 (collector totals 320 → 80 including the two cold passes); live corroboration: the running instance re-reads 151 MB per repeat search (rchar delta) and answers in 74-160 ms because its one-slot memo is occupied by shorter real-search needles) | the content-search miss memo keeps a per-file LRU of proven-absent roots (needle → signature, cap 8) instead of one shortest-needle slot — the one-slot form let the shortest needle ever searched permanently occupy the proof and sent every query family outside its superstrings back to a full 155 MB corpus scan per request |
| 2026-09-05 | this PR | M55 artifact compare-view repeat median 0.2484/0.2675 s → 0.0028/0.0025 s, maxima 0.2706/0.3052 s → 0.0035/0.0031 s (collector verbatim, 1.5 MB worst artifact pair understanding_packed-batch-cost-balance_v10.html vs _v9.html, scratch CHARLIEBOT_HOME, main checkout before vs branch after interleaved back-to-back ×2 at load 0.89-0.97; served body byte-identical across arms — 150741 B, digest a82879fdc034; first view unchanged at 0.2270-0.2532 s both arms, now off the loop) | the `?diff=` annotate moved off the event loop into one thread hop and its result memoized on both files' (path, mtime_ns, size) signatures plus the injection flag — the marks are a pure function of the two files' bytes and artifact pages are only ever written whole, so a repeat compare view re-runs zero annotate; the pre-fix inline annotate froze the event loop 246.2 ms per repeat request (raw-ASGI 5 ms-ticker round: loop-lag 246.2 ms, wall 246.6 ms before vs 5.2 ms / 1.6 ms after), the same pathology M14 measured on the git diff endpoints; the clean artifact view's comment-layer injection also left the event loop; M55 definition and healthy range introduced with this PR |
| 2026-09-04 | this PR | M56 /status request median 3.03/2.89 ms → 2.30/2.37 ms, maxima 8.57/8.55 ms → 7.65/7.72 ms (collector verbatim, 41 active-session ids, live corpus read-only, main checkout before vs branch after interleaved back-to-back ×2 at load 2.0-2.1; parsed-body digest identical a344862b7fe2 across arms; the 34-id live-poll shape measured 2.81/2.65 ms → 2.37/2.28 ms in the same interleaved protocol) | the sidebar's 3 s poll renders through FastJsonResponse, skipping FastAPI's jsonable_encoder pass over the 41-row derived-state dict; M56 definition and healthy range introduced with this PR |
| 2026-09-04 | this PR | M57 /plans request median 3.63/3.54 ms → 2.77/2.74 ms, maxima 4.43/4.53 ms → 3.62/3.49 ms (collector verbatim, 15.4 KB worst plans corpus of session a9bb2346, live state read-only, main checkout before vs branch after interleaved back-to-back ×2 at load 2.0-2.1; parsed-body digest identical f0098c1aae15 across arms) | the plan panel's 3 s poll renders through FastJsonResponse, skipping FastAPI's jsonable_encoder pass over the 12-plan dict (the registry read itself is already the M27 memo at 10.6 µs); M57 definition and healthy range introduced with this PR |
| 2026-09-05 | this PR | M6 append-round usage resolution median 6.38 ms → 0.12 ms, max 9.02 ms → 0.15 ms (collector + append-round rounds, 20534-event worst live corpus of session d321b9ad, scratch CHARLIEBOT_HOME, main checkout before vs branch after interleaved back-to-back ×3 at load 4.4-6.5, every paired round faster: 6.05/7.33/7.47 → 0.10/0.14/0.13 ms; resolved usage dict equals the full-scan reference on the real corpus both arms; unchanged-list steady state re-measured 0.13 ms → 0.14 ms, no regression) | the usage scan is a fold whose state now carries across resolutions in the per-session memo — an appending list (one chat event per streamed delta, the 3 s usage poll's steady companion during a turn) folds only the appended suffix instead of re-scanning the whole history per poll, and a wholesale list replacement (new identity) rebuilds from a fresh fold; append-round sub-metric added to the M6 definition and collector in this PR |
| 2026-09-05 | this PR | M58 steady-state get_config median 79.4/85.6/84.0 µs → 29.6/27.3/27.3 µs, maxima ~120 µs → ~51 µs (collector verbatim, live config corpus of 2 fragments + cron.d read-only, main checkout before vs branch after interleaved back-to-back ×3 at load 1.4-2.5; raw-ASGI 401 floor median 78 → 24 µs, max 162 → 70 µs over 300 calls on a scratch home; M53 collector re-run on its broken corpus: call wall 80 → 20 µs with the M53 metric unchanged — 1 onset warning, 0 steady re-parses, fingerprint-move freshness intact) | the per-request fingerprint walk left pathlib: the resolved home is memoized on the (CHARLIEBOT_HOME, HOME) env pair — resolve() is a per-component symlink walk and Path.home()/str(Path) re-parse per call, and both public readers (charliebot_home_dir, default_charliebot_home) serve one cached entry so callers comparing their home against the default stay coherent — and the config.d fragment scan is one shared os.scandir + raw-stat walker (name/dotfile/cron.yaml rule and S_ISREG gate in one place) used by both the loader and the fingerprint; M58 definition and healthy range introduced with this PR |
| 2026-09-05 | this PR | M46 steady-state GET /api/cron/tasks median 2.03/2.05/2.01 ms → 1.67/1.70/1.77 ms, maxima 2.34/2.20/2.34 ms → 1.96/1.99/2.00 ms (collector verbatim, 13 task rows, live cron corpus read-only, main checkout before vs branch after interleaved back-to-back ×3 at load 1.4-2.5; body 4154 B byte-identical, prompt bytes 0 both arms) | the cron tasks list renders through FastJsonResponse with a mode="json" model_dump, skipping jsonable_encoder's dict recursion on the mapped return — the same encoder pass M34 measured 6x slower than plain dumps on mapped lists; every dumped field is already a JSON primitive, so the bytes are identical to the encoder's output |
| 2026-09-05 | this PR | M45 stale-cursor replay loop-lag median 0.0102 s → 0.0063 s, max 0.0153 s → 0.0065 s (collector verbatim with the dual-shaped stub, main checkout before vs branch after back-to-back at load 1.83/1.48/1.08 and 1.76/1.47/1.08; 47 frames replayed, frame-list digest identical 314dfbe9fd89 across arms; replay wall 0.0237 s → 0.0250 s, within noise; earlier same-code sweep: 2500-event slices 0.0120 s, thread walk + 1 ms GIL switch interval 0.0062 s at wall 0.0269 s) | the catchup walk and its wire rendering moved from a whole-corpus thread run to ~1 ms on-loop slices with a yield between slices — the walk is pure-Python CPU and a whole-corpus thread run parks the loop behind GIL handoffs for its full span (measured 10.2 ms loop-lag; the 1 ms switch-interval probe confirmed the GIL attribution), while 400-event/4-frame slices keep every loop hold near the 5 ms ticker floor; sends leave send_json for pre-rendered send_text whose bytes match Starlette's send_json rendering exactly; the M45 collector stub models both send paths |
| 2026-09-05 | this PR | M60 repeat-page median 13.16/10.52/13.13 ms → 0.01/0.01/0.01 ms, maxima 16.57/11.59/16.26 ms → 0.03/0.02/0.03 ms (collector verbatim, 40 largest bodies / 57.7 KB, page-corpus sha1 490505464120, of the 36.3 MB worst live chat file, marked + hljs 11.9.0 common build, main checkout before vs branch after interleaved back-to-back ×3 at load ~1.7; repeat bodies byte-identical to a direct cold render both arms — parity true; cold first render unchanged 0.210-0.250 s, the M54 highlight cache already serving its repeat slice) | chat message-body parse (marked + fence fix) memoized on the body text (renderProseMarkdown, LRU 64 next to the M54 highlight cache) — every session switch rebuilds the turn engine and re-parsed every re-rendered body, so a repeat page render paid the full re-parse; the streaming draft paint stays off the memo on purpose (its content grows every delta and would only evict); M60 definition and healthy range introduced with this PR |
| 2026-09-07 | this PR | M60 cold first paint 0.214/0.215/0.214 s → 0.036/0.037/0.039 s deferred-parse + 0.174/0.178/0.183 s highlight flush (collector verbatim, same 40-body / 57.7 KB corpus sha1 490505464120, marked + hljs 11.9.0 common build, main checkout before vs branch after interleaved back-to-back ×3 at load ~1.7; repeat-page 0.01 ms and parity true unchanged — the settled memo bytes stay byte-identical to a direct render, and the flush swaps the same highlight bytes into the marker nodes; 4746-passed suite, 8 new deferral tests; cold-split sub-metric added to the M60 definition and collector in this PR) | the first-encounter code blocks' highlightAuto moved off the first paint into a scheduled flush — renderProseMarkdown parses with the deferral flag set, renderer.code emits escaped-plain code with a data-hl marker and records the block, postProcessRenderedMessages schedules one flush (rAF, setTimeout fallback) that runs the same cachedHighlight and swaps the settled bytes into the DOM and into the proseParseCache entries via string replace, so every later render serves the settled entry; the M54 streaming path and the modal path keep their own ordering (usage.js paints with the deferral off), and the repeat metric is untouched — only where the ~180 ms of first-encounter hljs work sits moves (off the bytes the user waits for); the review round hardened the flush: the memo settle uses a replacer function (String's replacement string would read $$/$&/$`/$' patterns out of the highlighted bytes) and the sweep covers postProcess roots registered while records were pending — the turn-engine prerenders atoms detached and holds the fragment alive until the segment materializes, so those markers get the swap too, with the retry bounded at 120 × 250 ms and the memo settling on the first pass regardless |
| 2026-09-05 | this PR | M59 detail poll full row median 3.29/3.46 ms → 2.03/2.07 ms (two interleaved verbatim-collector rounds, 99.9 KB worst metadata.json, body 50221 B → 59259 B raw — parsed-identical, the documented \uXXXX escaping; maxima 4.43/5.22 → 3.45/3.33 ms), attach mode (?attach=1, the poll's new shape) median 1.90/1.78 ms, max 2.07/1.93 ms, body 48 B (main checkout before vs branch after back-to-back at load 1.8-2.0; 4500-passed suite) | the 5 s poll fetched the whole row — 50 KB description-bearing body, an uncached aiofiles read+parse per call, response-model validation + jsonable_encoder render — to read the attach pair; the detail endpoint now serves the parsed meta from a (mtime_ns, size) memo (the endpoint's consumer is read-only; mutating callers keep the uncached manager getter, and every writer publishes through the atomic tmp rename so the signature is taken before the read), renders both shapes through FastJsonResponse, drops `context` (no HTTP consumer reads it; the modal fetches description once per click), and the poll fetches `?attach=1` — the 48 B pair — while the modal and the description-full fetch keep the full row; M34 re-measured unchanged (full 0.0057 s / 860706 B byte-identical, after=total 0.0022 s / 40 B) after the events no-after branch was left mapped — a FastJsonResponse rider there measured 0.0103 s (2177 per-event model_dump calls beat by the single encoder walk) and was reverted |
| 2026-09-05 | this PR | M8 warm absent-needle search, churn-modeled pool: median 99.71/107.45/101.95 ms → 27.80/38.46/33.19 ms, idle median 4.40/4.23/4.02 ms → 1.56/1.74/1.40 ms (three interleaved A/B rounds, manager-level collector over the shared 156.7 MB / 39-active-file + 1075-metadata snapshot home; 41 content candidates; churn model = 8 continuous 5 ms CPU bursts through the same default asyncio executor, the live server's pool shape; result digests identical across arms for absent, name-hit, and content-hit queries — 0/11/181 rows); live-before (verbatim M8 curls against the running instance) median 0.146-0.158 s, max 0.151-0.177 s at load 3.26-3.84 | the content-search classification (stat + proven-absent memo check) left the per-file executor round-trip — the default executor's ~cpu+4 workers are shared with every poll read, append, and probe, so the 41 per-file acquisitions queued ~2.4 ms each under the server's pool churn (live search 140-158 ms vs 27 ms for the name-hit shape that skips the fan-out; a fully-occupied pool stretched the warm search to 2013 ms in-process); classification now runs on the event loop (~0.1 ms of hot stats, no stall over the churn-only 20.6 ms ticker baseline — main's search measured 31.0 ms under the same ticker protocol) and only reads that move corpus bytes go to the pool; the window math moved verbatim into `_absence_rescan_start`, the suite pins scan counts and start offsets |
| 2026-09-05 | this PR | M61 idle-cold metadata reads: bare listing median 2.98-3.14 ms → 1.15-1.20 ms, archived page (limit 100) 3.80-4.12 ms → 2.09-2.13 ms, all-sessions listing 7.50-7.65 ms → 5.50-5.78 ms, single get_session 0.526-0.573 ms → 0.037-0.055 ms (five interleaved verbatim-collector rounds, 1075 cached metas / 41 non-archived, live corpus read-only, main checkout before vs branch after at load 1.6-2.3, every paired round faster; warm steady state re-measured unchanged — M29 0.55-0.59 vs 0.56-0.57 ms, M56 2.40-2.59 vs 2.34-2.52 ms with identical digests, M40/M44 within noise; suite pins the stat-revalidation chain: unchanged file zero reads, moved file exactly one) | an expired metadata entry revalidates against metadata.json with one stat instead of a re-read: the cache entry carries the (st_mtime_ns, st_size) its read took before parsing, every writer publishes through the atomic tmp rename so a content change always moves the signature, and a same-signature stat re-times the entry — strictly fresher than the 30 s TTL it replaces (a write-funnel populate keeps no provable signature and follows today's evict-and-re-read); M61 definition and healthy range introduced with this PR |
| 2026-09-05 | this PR | M62 base-less base-resolution chain median 1.03/1.00/0.92 s → 0.33/0.31/0.30 s, max 1.07 s → 0.37 s (three interleaved collector sets, main checkout before vs branch after against the real origin at load 2.33/2.11/1.32; start_point origin/main identical across arms; every paired set faster) | the base-less launch chain answered one unfiltered `git ls-remote --symref origin` listing — HEAD symref plus the default branch's tip — in a single round-trip instead of two filtered probes, and resolve_base_branch skips the fetch its own probe just proved a no-op (tracking ref already at the advertised tip; the probe is read straight from the remote, so no fetch could change that ref); the caller-fed `remote_tip` keeps the freshly-resolved start-point guarantee — the fetch still runs whenever the probe shows the tracking ref behind, and every error path is unchanged; M62 definition and healthy range introduced with this PR |
| 2026-09-05 | this PR | M63 /view handler median 11.76/12.37/12.12 ms → 4.09/4.15/3.96 ms, maxima 16.74/13.94/13.54 ms → 4.79/4.57/4.81 ms, body 2640195 B → 277184 B (three interleaved rounds of the collector, 339-file worst threads corpus of session 3b91d606, scratch CHARLIEBOT_HOME, main checkout before vs branch after at load 1.85/1.45/1.03, every paired round faster; TestClient-level A/B over the same scratch snapshots 41.28/41.19/37.25 ms → 30.34/28.11/28.80 ms — the request harness floor dominates both arms, so the handler level is the standing row; component attribution before: 339 whole-row dumps 1.65 ms + 2.6 MB render 7.75 ms, after: prefixed-row builds 1.08 ms + 277 KB render 0.48 ms; M35's /view corpus (d321b9ad, 19 threads) re-measured 7.24/6.51 ms → 6.68 ms, body 292279 B → 182981 B, events page unchanged (5.84 ms / 633236 B, digest 46d1d509a0d6) — no regression) | the session view's `threads` array ships the workers-panel list's prefixed rows (`_thread_list_item`, the M36 truncation contract the same card builder already consumes: one CSS-truncated description line per card, the full-text modal fetches the row on click) instead of whole ThreadMetadata dumps — task-spec-length descriptions made the worst session's view body ~7.8 KB per row, 2.6 MB per session open, which the gzip lane then recompressed per request; M63 definition and healthy range introduced with this PR |
| 2026-09-05 | this PR | M19 framing median 0.0176/0.0174/0.0174/0.0173/0.0176 s → 0.0171/0.0168/0.0165/0.0167/0.0166 s, maxima 0.0178/0.0178/0.0175/0.0177/0.0180 s → 0.0173/0.0170/0.0167/0.0168/0.0168 s (five interleaved rounds of the verbatim collector, main checkout before vs branch after back-to-back at load 1.1-1.7, every paired round faster; the framing work itself, measured by feeding the framer pre-decoded chunks, 16.21 ms → 2.46 ms per 16 MB; LF-dense production shape unchanged 3.00 → 3.00 ms per 1.6 MB / 7000 lines / 16 KB chunks; yielded-line digests identical across both arms over 12 sparse/dense/mixed corpus shapes at 997 B / 16 KB / 64 KB / single-chunk splits, U+2028/U+0085 content, CRLF/CR/LF and unterminated tails; 4539-passed suite) | the resolved line's tail joins with the accumulated pieces instead of concatenating onto the join's fresh result — a concat on a just-allocated large string re-allocates through the mmap lane and re-faults the line's pages (~0.9 ms per 1 MB line measured vs ~0.03 ms for one join of the same pieces), which once per multi-hundred-KB frame was the framer's dominant cost; the collector's remaining wall sits on the per-line 1 MB str materialization and fresh-chunk page faults both arms share |
| 2026-09-05 | this PR | M65 whole-body gzip loop-lag median 17.58/17.15/17.22 ms → 5.99/5.99/5.80 ms, maxima 18.20/17.81/17.79 → 6.06/6.08/6.10 ms (three interleaved rounds of the verbatim collector, 20534-event worst live corpus of session d321b9ad, scratch CHARLIEBOT_HOME, main checkout before vs branch after back-to-back at load 1.5-2.1; wire 165729 B in both arms (deflate payload equal with the header's wall-time mtime field zeroed — gzip embeds the construction time, so full bytes differ between runs) and wall median 17.29-17.73 → 18.23-18.75 ms, the compression now concurrent; live corroboration: the running instance answers the same events page 19.0 ms without Accept-Encoding: gzip and 30.4 ms with it, the delta the pre-fix loop pays inline) | whole-body responses' deflate moved off the event loop into one to_thread hop in a GZipResponder subclass (`_OffLoopWholeBodyGZipResponder`, threshold 8 KB — a smaller body's deflate costs less inline than the ~104 µs executor round-trip the M52 row measured); streaming bodies keep Starlette's inline per-chunk path (their gzip file state must not cross threads between writes); /perfetto/merged pass-through unchanged; wire bytes pinned equal to Starlette's inline output with the header mtime zeroed by the suite; M65 definition and healthy range introduced with this PR |
| 2026-09-05 | this PR | M36 poll full median 3.94/4.00/3.97 ms → 2.23/2.14/2.05 ms, conditional median 3.91/3.87/3.84 ms → 2.20/2.18/2.16 ms (three interleaved rounds of the verbatim collector, 2551 KB / 339-row worst worker-list corpus, live state read-only, origin/main tree vs branch tree back-to-back at load 0.93-0.94; full body 168209 B byte-identical and conditional 0 B (204) across arms; M63 /view 3.93 → 4.11 ms, M21 sweep 0.0024 → 0.0025 s, M56 2.45 → 2.46 ms with digest a6771f2115c9 — no regression; 4541-passed suite) | the 3 s poll's per-call disk proof left the walk: every row-source writer (thread metadata via _save_metadata, triggers via _save_trigger) already calls mark_sidebar_dirty, so a per-session change revision gates the signature walk — an unchanged revision serves the memo (204 or full bytes) with zero stats, an every-10th-poll sweep walk bounds a missed mark to the same ~30 s window the sidebar's populate sweep accepts, and the revision is read before the walk so a mark landing mid-walk or mid-rebuild only raises it; ThreadManager.list_threads keeps its per-call scan (the deletion contract pins next-call pickup of out-of-band removals) |
| 2026-09-06 | this PR | M7 warm collect median 18.9/22.2/19.2 ms → 16.4/16.3/14.7 ms (three interleaved collector rounds, 7 timed warm collects each, before maxima 72.2-79.1 ms are the moved-WAL diff rounds those runs caught vs after maxima 17.0-17.5 ms, rows+notes 14/3 identical both arms, origin/main archive vs branch worktree back-to-back at load 2.06-3.14); scratch-server warm median 102.4 ms → 97.2 ms over 20 interleaved verbatim-shape /token-usage curls (cold 1.83 s vs 1.95 s; the one natural changed round measured 223.6 ms before vs 129.1 ms after, load 2.23-2.51); synthetic 60k-row changed-round collect max 148.6/154.8 ms → 56.0/56.3 ms over two rounds per arm (the five step-finish upserts identical, 7 rows both arms); live-before corroboration: verbatim M7 curls against the running instance median 0.252 s, max 0.605 s (the instance predates this change) | the corpus signature walk recurses scandir entries carrying their own stat — one syscall per jsonl instead of one per file plus a re-stat, identical signature tuples and unreadable-directory notes (verified tuple-exact against the os.walk contract on the live corpus, 1574 files across 5 homes); the opencode merge adjusts the persisted per-db partial (buckets by model and account, per-model spans, contributing-record count) by the scan's per-row deltas instead of replaying every contributing record — subtract old, add new, re-derive spans a removal invalidated, drop emptied buckets — falling back to the full replay whenever the partial is absent, with all reads before any memo write so a mid-scan failure leaves the memo and partial untouched |
| 2026-09-06 | this PR | M55 first view 0.3078/0.3009 s → 0.2528 s (paired verbatim-collector arms on the shared 1.5 MB snapshot pair); in-process annotate wall median 0.2301 s → 0.1893 s over 5 timed calls each; repeat view unchanged 0.0025 s vs 0.0024 s (the M55 memo path); annotate output byte-identical across a 4000-pair randomized fuzz and the real pair both directions plus 20 synthetic shapes, exception parity included | the two per-character splice walks (every byte of a 120 KB page through one list append each, twice per annotate) became offset-sorted slice merges with the insertion-list order preserved — verified byte-identical over 20000 randomized insertion dicts in isolation; the third full HTMLParser parse stays: the render passes can rewrite the body start tag and insert synthetic start tags, so the spliced page's head/body anchors are a resynchronized-DOM answer the pre-splice parse cannot supply (a single-parse variant measured 0.1480 s but changed output bytes on 510/4000 fuzz pairs and was reverted) |
| 2026-09-05 | this PR | M6 route render: raw-ASGI GET /usage median 703 µs → 571 µs, max 1015 µs → 801 µs; paired interleaved rounds 710→581 and 672→574 µs (300 timed calls after 20 warm each through the real app stack, scratch CHARLIEBOT_HOME copy of the 20534-event worst usage corpus, origin/main tree before vs branch after back-to-back at load 0.5-1.0; parsed-body digest identical 19357dd8efa0 across all arms, body 1409→1421 B the documented \uXXXX escaping); TestClient GET /usage median 2.94 ms → 2.74 ms; M6 append-round resolution re-measured unchanged 0.12 ms parity True; M56 2.34 ms digest 2330485ecbb0 and M35 digests 46d1d509a0d6/4168f50ed5ff unchanged | the third-busiest polled route was still returning a plain dict through FastAPI's jsonable_encoder walk (101 recursive calls per request measured) — it renders through FastJsonResponse like its sibling polled routes; the encoder-pass cut rides the 3 s active-header poll during streamed turns (the tui/status poll keeps its mapped returns — zero tui-cli backends on this deployment leave it unrouted here) |
| 2026-09-05 | this PR | M65 wall median 17.76 ms → 8.40/8.42/8.44 ms, maxima 19.05 ms → 8.66-9.02 ms; loop-lag unchanged 5.96 ms → 5.70/5.72/5.70 ms at the 5 ms ticker floor (collector verbatim ×3 rounds on the 20534-event worst live corpus of session d321b9ad, scratch CHARLIEBOT_HOME, worktree at the origin/main tree before vs branch after back-to-back at load 0.88-0.89; wire 165729 B → 201149 B, +21 %); component table on the same body: deflate 15.0 ms at level 6 vs 8.9 ms at level 4 (wire 174321 B) vs 5.5 ms at level 1 | the whole-body deflate rides the response's send path (the off-loop hop moved it off the event loop but send still awaits it), so the compression level is client-visible wall latency and serve CPU on every big-page fetch — the mount drops level 6 → 1; M65's healthy range gains the wall pin (wall median < 0.012 s) in this PR |
| 2026-09-06 | this PR | M57 /plans request median 2.93/2.76/2.82 ms → 2.27/2.38/2.55 ms, maxima 3.29/2.92/3.42 ms → 2.61/2.73/3.14 ms (three interleaved rounds of the verbatim collector, 15.4 KB worst plans corpus of session a9bb2346, live state read-only, main checkout before vs branch after back-to-back at load 1.6-2.5, every paired round faster; parsed-body digest identical f0098c1aae15 across all arms; component corroboration: list_plans memo-hit await 181.3 µs → 28.9 µs per call; M27 steady-state tolerant read re-measured 9.0 µs vs the 8.8 µs standing row, no regression; 4561-passed suite) | the plans poll's list_plans awaited an executor round-trip (~170 µs) to serve an ~9 µs memo hit on every 3 s panel poll — the M57 reading had drifted to its < 3.0 ms line (2.81-3.02 ms measured this round); the memo-hit half of read_plans_tolerant (one stat plus a lookup) is exposed as tolerant_memo_hit so the async list_plans answers a hit on the event loop and pays the thread only on a miss (cold, changed, or missing file), the same hit-on-loop shape the M58 config walk took |
| 2026-09-06 | this PR | M17 fork median 0.0888/0.0892/0.0884 s → 0.0731/0.0689/0.0680 s, maxima 0.0891/0.0938/0.0991 s → 0.0744/0.0748/0.0687 s (three interleaved rounds of the verbatim collector, 5519 parent events over the 36.3 MB chat-event corpus of session aa196b47, scratch CHARLIEBOT_HOME, main checkout before vs branch after back-to-back, every paired round faster; 5519-event count identical across all rounds; standalone component: `_fast_reference_frames` 26.2 ms → 7.2 ms on the 36.3 MB corpus with identical frame output, chunked 1 MiB sweep; 104 session-scoped tests passed) | the fork's frame check swept the whole corpus through one `arr == 0x0A` bool mask — one scratch bool per source byte, ~1.4 GB/s measured on the 36.3 MB live file — while the same numpy compare in 1 MiB chunks keeps the mask in cache at ~5 GB/s (the M28 count scan's chunked shape); newline detection is position-local, so chunking cannot change the result, and the decode-validity pass, per-frame fallback, CR folding, and corrupt-line rejection are untouched |
| 2026-09-06 | this PR | M66 merged build median 8.19/7.96 s → 5.43/5.42 s, maxima 8.29/8.01 s → 5.44/5.45 s (two interleaved rounds of the collector, 191.2 MB / 496,099-event worst on-disk trace /home/chaoli/data/stage3_current_traces/221054_trace_rank000_step000110.json, scratch output under /tmp, main checkout 2d5fca48 before vs branch worktree after back-to-back at load 2.11/1.30/0.86, every paired round faster; decompressed payload byte-identical across arms — 146329651 B, sha256 5fbdd34f0317; artifact 11.6 → 15.6 MB.gz, the level-1 trade) | the merge serializer's per-event json.dumps writes became 512-event batched C-encoder calls — a batch's bracket-stripped rendering is byte-identical to the per-event form, and the serializer pass measured 3.0 s → 1.7 s on the same corpus — and the merged artifact's deflate dropped 6 → 1 (1.73 s → 0.57 s, +34 % wire), the big-payload level the transport gzip middleware already runs; the direct-pass build shares the constant; M66 definition and healthy range introduced with this PR |
| 2026-09-06 | this PR | M44 steady-state /scheduled median 3.15/3.17/3.07 ms → 2.96/2.92/2.98 ms, maxima 4.73/4.92/4.65 ms → 4.57/4.46/4.68 ms (three interleaved rounds of the verbatim collector, 13 scheduled rows / 14195 B body digest fe57036d7b53 identical across arms, live session + cron corpus read-only, main checkout before vs branch after back-to-back at load 2.55/1.76/1.14, every paired round faster) | the /scheduled route's per-call cron fingerprint walk left pathlib: one os.scandir over the raw string dir answers is_file from the directory record and stats via DirEntry with no per-entry Path construction — the walk runs on every get_scheduled_tasks call (each /scheduled and /api/cron/tasks request, every scheduler tick), 174 µs → 53 µs measured on the live 13-file corpus, the M58 conversion of the sibling config fingerprint applied to the cron one |
| 2026-09-06 | this PR | M46 steady-state GET /api/cron/tasks median 1.60/1.70/1.69 ms → 1.49/1.52/1.58 ms, maxima 2.26/2.16/2.12 ms → 1.70/1.75/1.88 ms (three interleaved rounds of the verbatim collector, 13 task rows / 4154 B body byte-identical across arms, live cron corpus read-only, main checkout before vs branch after back-to-back at load 2.55/1.76/1.14, every paired round faster; M42 tick re-measured 0.0057 s → 0.0058 s at the 5 ms ticker floor, no regression; 4564-passed suite) | the same converted fingerprint walk rides this route's per-call get_scheduled_tasks |
| 2026-09-06 | this PR | M3 standing collector median 0.011 s, max 0.011 s over 5 requests; 10-request recheck median 0.0104 s, max 0.0193 s (load 0.28/0.41/0.56) — inside the old < 0.05 s line yet 10x the 0.001-0.002 s healthy history. Attribution: the live server (started 2026-09-03 17:51, predates the 09-04..09-06 latency-perf merges) runs pre-M53/M58 code against a config corpus its build rejects (aigw_api_key declared 758a331b, publish_dir 52822579 — both after server start), so every request's get_config re-parses the full config (9.25 ms, the M53 evidence's live-corpus figure) and logs config_reload_failed; 9.25 ms parse + ~1 ms HTTP floor = the observed 10.4 ms. Current-code floor: 78-81 µs raw-ASGI 401 (the M3 A/B row), ~1 ms over HTTP | docs-only calibration: healthy range median < 0.05 s → < 0.005 s — the old line sat 25-50x above every healthy reading and passed a 10x live-path regression unflagged |
| 2026-09-06 | this PR | M34 after=total steady poll 2.27/2.23/2.24 ms → 1.83/1.84/1.95 ms; M35 events page 5.99/6.01/5.48 → 5.12/5.14/5.17 ms, view 7.35/6.24/6.38 → 5.81/6.00/6.09 ms, bootstrap 3.16/3.30/3.20 → 2.79/2.80/2.93 ms (interleaved verbatim-collector rounds, main checkout vs branch worktree back-to-back at load 1.6-1.8, every paired round faster; M35 body digests identical across arms — events 46d1d509a0d6, view 7992a1894ddf, bootstrap 4168f50ed5ff; M34 envelope 40 B / reset False / total 2177 unchanged; component corroboration: projection warm hit 86.0 µs threaded getter → 1.70 µs memo hit on the 20534-event corpus, worker-events steady 91.9 µs threaded → 14.4 µs hit on the 6.7 MB log, to_thread no-op round-trip 67 µs; M26 advance re-measured 0.13 ms, parity True, digest e94c56635194 and M13 0.0000 s — no regression) | the polled memo hits left the executor: get_message_projection's fast path (dict read + cache peek + len compare) exposed as projection_memo_hit so the events page, bootstrap, and view answer a warm corpus on the event loop and pay the threaded read/advance only on a miss, and the worker-events poll's unchanged-log proof (exists+stat under a non-blocking lock — a held lock means a concurrent reader may be mid-file-read, so it is never waited on here) answers inline with the incremental read still on a thread; the #865 hit-on-loop shape applied to the two remaining polled reads |
| 2026-09-06 | this PR | M56 /status request median 2.47/2.57/2.60 ms → 1.93/2.34/2.31 ms, maxima 8.63/8.40/8.91 ms → 8.34/8.22/8.41 ms (three interleaved rounds of the verbatim collector, 43 sidebar ids, live corpus read-only, main checkout before vs branch worktree after back-to-back at load 3.08-3.51, every paired round faster; body 9569 B and parsed-body digest ae02688caf42 identical across all six arms; component attribution: 43x get_session 0.31 ms (model_copy 0.09), populate_sidebar_state 0.14 ms, render 0.04 ms; no-regression re-measures: M21 sweep 0.0033/0.0037 s → 0.0031/0.0030 s, M40 starred 0.69/0.66/0.75/0.77 ms → 1.06/0.70/0.71/0.78 ms and groups 0.63/0.64/0.64/0.67 → 0.96/0.65/0.65/0.72 ms (round-1 after spike is the documented enrich-probe noise, parity in the other three rounds), M61 within noise both directions; 4573-passed suite) | the polled sidebar read left the per-row copy: resolve_sidebar_state holds the probe-or-serve machinery and returns the derived fields without mutating its inputs (has_running_tasks reads busy_since so cache references may arrive unstamped), populate_sidebar_state stays as the wrapper for the copy-owning callers, and the /status handler reads the cached metadata references through get_sessions_readonly (warm hits serve the cached objects themselves, misses keep the get_session read, order and unknown-id dropping preserved) |
| 2026-09-06 | this PR | M41 repeat-view median 0.0060/0.0061/0.0065 s → 0.0017/0.0018/0.0017 s, maxima 0.0067/0.0064/0.0066 s → 0.0021/0.0022/0.0021 s (post-review re-pair 0.0069 s → 0.0017 s at load 2.2); M43 repeat-expand median 0.0063/0.0064/0.0062 s → 0.0021/0.0020/0.0019 s, maxima 0.0067/0.0066/0.0068 s → 0.0024/0.0024/0.0021 s (three interleaved verbatim-collector rounds, 533-file charlie-bot root..HEAD manifest, main checkout before vs branch worktree after back-to-back at load 1.2-1.8; manifest body sha256-identical d38f3e7f84d4ccc9 and file body 982d39e617a32e42 across all arms; first views unchanged within subprocess noise — M41 0.067 s → 0.068-0.073 s paying the new signature walk once per ref move, M43 0.020 s → 0.016 s; M14 loop-lag re-measured unchanged, 0.0056-0.0058 s → 0.0058 s at the 5 ms ticker floor; 4578-passed suite) | the rev-parse left the repeat path: ref resolution memoized on a stat-only ref-state signature over the full rev-parse read set (git-dir top-level pseudo-refs, packed-refs, shallow/grafts, the shared loose refs tree, and — in a linked worktree — the worktree's own per-worktree refs tree, the review's soundness finding with the bisect-ref move pinned by test), the signature walked with os.scandir so d_type from readdir halves the syscalls (233-entry walk 3.1 → 1.2 ms, the pathlib is_file+stat double-stat pattern the M44 cron-fingerprint fix removed), and the walk + memo hit + miss resolve sharing one thread hop; a ref that moves rewrites its file and moves the signature, so soundness rests on the same rename-atomic-write ground as the sibling memos; M41/M43 definitions and healthy ranges unchanged |
| 2026-09-06 | this PR | M67 steady-state probe trigger scan median 3351/3339/3283 µs → 407/402/421 µs, maxima 3599/3602/3393 → 437/502/524 µs (three interleaved rounds of the collector, 101-file worst trigger corpus of session a481fbde, live state read-only, main checkout before vs branch worktree after back-to-back at load 0.89-1.06, every paired round faster; warm full deep probe of 44 active sessions 10.35 ms → 3.03 ms; no-regression re-measures: M21 sweep 3.85 → 3.03 ms, M51 post-write deep probe 3.44 → 3.43 ms, M56 /status 2.13 → 2.07 ms with parsed-body digest 4d1f75803e9b identical; 2796-passed session/trigger/sidebar test slice) | the sidebar deep probe's trigger scan (`pending_trigger_state_sync`) re-read and re-parsed every trigger `*.json` on every call while its two sibling probe reads (thread metadata, plans) were already per-file memoized; parsed dicts now memoize per file on (mtime_ns, size) behind the scandir str-path walk, mirroring the M51 thread-metadata memo — trigger files change only through _save_trigger's atomic rename so an unchanged signature proves the content current, and the signature is taken before the read so a mid-rewrite entry keys the older signature; M67 definition and healthy range introduced with this PR |
| 2026-09-06 | this PR | M68 marked changed-poll rebuild median 8.65/8.53/9.12 ms → 6.33/6.51/6.41 ms, maxima 10.16/10.27/10.03 ms → 7.16/6.97/7.18 ms (three interleaved rounds of the collector, 2551 KB / 339-row worst thread-metadata corpus of session 3b91d606, scratch CHARLIEBOT_HOME wired through get_config, main checkout vs branch worktree back-to-back at load 0.9-1.2, every paired round faster; body 168209 B byte-identical across arms; no-regression re-measures: M36 unchanged-poll 2.15 ms full / 2.16 ms conditional (204, 0 B) and M63 /view handler 4.24 ms / 277184 B, both at their standing readings; 4612-passed suite) | the marked rebuild paid two full scans of every thread metadata.json — the body signature's walk plus list_threads' own per-call scan (the deletion contract) — and rebuilt all 339 rows though _thread_list_item is a pure function of the per-file-parsed metadata; the rebuild now walks once (the walked pairs feed both the signature and a list_threads_from_stats parse-merge sharing list_threads' memo, so the proof and the rows behind the body describe one instant) and serves unmoved files' rows from a per-session row memo keyed on the same (mtime_ns, size) identity every atomic rename moves; M68 definition and healthy range introduced with this PR |
| 2026-09-06 | this PR | raw-ASGI component A/B, identical drive harness, 100 timed calls each: workers-list 204 median 694 µs → 248 µs, list full 655 µs → 266 µs, /status 1-id 341 µs → 190 µs (main checkout before vs branch worktree after back-to-back at load 1.2-2.1); M36 TestClient interleaved ×2: full 2.21/2.11 ms → 2.08/2.07 ms, conditional 2.12/2.31 ms → 2.00/1.96 ms, body sha1 d97c45b013d6 identical across all four arms; M56 2.18 → 2.05 ms, digest e34b8b212e0b identical; M59 full row 2.09 → 2.05 ms, attach 1.85 → 1.80 ms; M44 3.20 → 2.94 ms; M57 2.55 → 2.30 ms, digest f0098c1aae15 identical; M35 view 6.45 → 5.98 ms, events/bootstrap within noise, digests 46d1d509a0d6 / ea2d0c6b27c3 / 8e4653af40df identical across arms; M68 5.66 → 6.27 ms (load noise; body 168209 B byte-identical); the M3 401 floor and the live instance are untouched by this diff | the four manager getters and the polled routes' config dependency were sync callables, so FastAPI resolved every Depends on them through an anyio threadpool handoff per request — cProfile attributes ~250 µs of the 694 µs memo-hit list poll to the three hops (thread round-trip + event-loop self-pipe wake each); the getters are now async (the loop awaits a dict check) with plain-name sync forms for the direct callers (startup, websocket, tui autoname), and the polled routes (workers list, thread detail, tui/status, view, bootstrap, usage) resolve cfg through get_config_on_loop, so all 62 manager-dep annotation sites shed their hops with no annotation edits; 4674-passed suite |
| 2026-09-06 | this PR | M69 60 → 0 opencode_sse_event_unhandled debug lines per 60 steady-state `_translate_sse_event` frames of one unhandled type (collector verbatim, main checkout before at load 1.62/1.05/0.72 vs branch worktree after at load 2.73/1.84/1.11, back-to-back; live-log corroboration 306 lines in the 68.85 h server log, 295 type=todo.updated + 11 type=session.compacted; unhandled frames still translate to []; M49's part-type stream re-measured 0 lines per 60 rounds, no regression; 4675-passed suite) | the SSE event-type fallthrough routed through an event-type warn-once registry — one line per unmapped type per process, the M49 part-type registry's mechanism applied to the sibling SSE fallthrough; M69 definition and healthy range introduced with this PR |
| 2026-09-06 | this PR | M70 repeat-view median 0.0118/0.0124 s → 0.0027/0.0029 s, maxima 0.0128/0.0141 s → 0.0034/0.0038 s (two interleaved rounds of the collector, shared 1.08 MB / 1084780 B served-body snapshot of session 3dfa5384's worst artifact page, main checkout before vs branch worktree after back-to-back at load 0.59-0.72, every paired round faster; first views unchanged 0.0200-0.0218 s → 0.0192-0.0199 s both arms; served bodies byte-identical across arms except the per-checkout cache-bust version query the injection embeds; M55 repeat-view re-measured 0.0025 s → 0.0026 s, no regression; 4677-passed suite) | the credentialed artifact view re-read and re-injected the whole page on every request — 823 artifact views in the 69.85 h live log, 794 of them repeats of an already-viewed file, the read alone ~4.8 ms of an ~11.5 ms repeat view on the 1.08 MB worst page — the injected body now memoizes on (path, mtime_ns, size) served as pre-encoded bytes behind one executor hop, mirroring the M55 annotate memo; M70 definition and healthy range introduced with this PR |
| 2026-09-06 | this PR | M44 steady-state /scheduled median 3.08/3.78/3.11 ms → 2.05/2.14/2.11 ms, body 14457 B digest f035d8fa1147 identical across all six arms (three interleaved verbatim-collector rounds, 13 scheduled rows, live session + cron corpus read-only, main checkout before vs branch worktree after back-to-back at load 2.78/2.15/1.42, every paired round faster; no-regression re-measures: M56 /status 2.42/2.16 → 2.31/2.27 ms digest 2f2a9909e4c7 identical — the readonly path does not route through the listing; M61 single get_session 0.053/0.040 → 0.040/0.044 ms and its sweep-walk state 1.29 ms bare / 2.32 ms archived-page inside the row's healthy ranges; 4679-passed suite) | the listing preamble walked every cached meta (~1086 entries: per-entry TTL check plus round-rating migration) on every call to serve a filtered subset; the per-filter result now memoizes on (listings revision, sessions-root (mtime_ns, size), 10 s sweep) — in-process writes bump the revision through the save_metadata single funnel (and _invalidate_cache, the write-funnel entry's expiry eviction, and list_active_session_metas' repopulate), a create/delete moves the root signature, and the sweep bounds an out-of-band edit to the entry TTL plus one interval; every listing caller shares the cut |
| 2026-09-06 | this PR | M71 capped search request median 9.49/9.04/9.73 ms → 5.50/5.68/5.44 ms, maxima 43.97/40.70/50.06 ms → 8.42/8.45/8.49 ms (three interleaved rounds of the collector, 200 rows / 207 KB body, shared snapshot of 1086 metas + 163.5 MB active live chat files + 182 triggers dirs, parsed-body digest 915d8ad4d28e identical across all six arms with 3 trigger-bearing rows in every response; manager-level corroboration 7.45 → 2.89 ms; absent-needle shape re-measured 2.69 → 2.92 ms, within noise; 4734-passed suite) | the 200-row cap applied to the sorted name matches before any row work: 200 newer-or-equal matches always outrank a match below the cap line and a content hit can only displace rows from above, so the dropped matches are never copied, stamped, probed, or sorted (the live corpus matched 1034 of 1086 names per request); the route serves the shared cached references through a read-only search returning (rows, resolve_sidebar_state dict) rendered via FastJsonResponse with the derived fields overlaid through the model's own UtcDatetime JSON scheme (a hand-rolled isoformat emitted +00:00 where the old response-model render emitted Z — the review finding the triggers-bearing snapshot corpus caught), the M56 /status shape; M71 definition and healthy range introduced with this PR |
| 2026-09-06 | this PR | M63 /view handler median 4.30/4.11/4.15 ms → 1.22/1.27/1.20 ms, maxima 4.91/5.08/4.71 ms → 1.38/1.99/1.43 ms (three interleaved rounds of the collector, 339-file worst threads corpus of session 3b91d606, scratch CHARLIEBOT_HOME, main checkout before vs branch worktree after back-to-back at load 2.1-2.2, every paired round faster; body 277206 B byte-identical across all six arms; no-regression re-measures: M35 digests identical — events 46d1d509a0d6, view ea2d0c6b27c3, bootstrap 8e4653af40df — M36 full 2.16 ms / conditional 2.09 ms (204, 0 B) and M68 marked rebuild 4.08 ms at their standing readings; 4735-passed suite) | the view handler re-walked every thread metadata.json and rebuilt every row on every call — its own 339-stat scan plus 339 row builds, ~3.6 ms of the 4.3 ms worst-corpus view (cProfile: 340 stats 1.15 ms + row builds incl. 678 isoformats 1.6 ms + 277 KB render 0.75 ms) — while the workers-panel list route already owned a revision-gated row proof; the view's threads array now rides that proof (view_thread_rows: the M36 revision gate with its own sweep counter, one walk-and-parse executor hop on a mark or the sweep, rows served from the shared M68 row memo and sorted newest-first), SessionViewData carries the rows instead of re-fetching metas |
| 2026-09-06 | this PR | M24 steady-state list_triggers median 0.0005/0.0006/0.0005 s, max 0.0006 s → 0.0000 s (< 0.05 ms), max 0.0000 s (three interleaved verbatim-collector rounds, 104-file worst trigger corpus of session a481fbde, live state read-only, main checkout before vs branch worktree after back-to-back at load 2.5-2.6, every paired round faster; served trigger lists identical across arms; corroboration: the view handler on the 104-trigger / 28-thread corpus of the same session 2.28 ms → 1.56 ms median on a shared scratch snapshot; no-regression re-measures: M67 probe trigger scan 432 µs, M36 full 2.08 ms / conditional 1.95 ms (204, 0 B), M68 marked rebuild 4.37 ms, M5 in-process list poll 2.04 ms — all at their standing readings; component split on the same corpus: one dir stat 2.8 µs, the scandir+stat walk 361 µs, its executor round-trip 494 µs, full list_triggers 560 µs; 4740-passed suite) | the per-file memo's steady state still paid the full scandir+stat walk and its executor round-trip on every call — the session view handler, the slack thread-follow arm check, and the workers-list rebuild each call list_triggers per request; every trigger-file write publishes through the atomic rename into the triggers directory, and a rename that creates, replaces, or removes an entry moves the directory's own mtime_ns, so a stored (mtime_ns, size) verdict on the directory serves the sorted memoized list for one on-loop stat, re-walking only when the directory state moved (the M29 root-signature shape, strictly stronger here because rewrites are renames; signature taken before the walk, same pre-read rule as M37) |
| 2026-09-07 | this PR | M54 paint-work median 125.8/148.1/125.1/133.6 ms → 103.7/106.2/97.2/98.6 ms, maxima 218.9/230.0/216.2/224.9 ms → 201.9/205.2/193.0/196.4 ms (four interleaved rounds of the collector's replay, 11.4 KB worst fence-bearing on-disk draft sha1 b155f860788f, 59 deltas at 40 ms virtual cadence, 12 paints, page-pinned marked + hljs 11.9.0 common build (36 languages), main checkout 0cc96919 before vs branch worktree after back-to-back, every paired round faster; final-frame parity true all arms; component attribution on the same replay: highlightAuto 240.3 ms of the 261.0 ms paint work over 18 calls, the growing blocks' partial content re-highlighted per paint on cache misses the (lang, code) key can never serve; mid-stream paints drop 6–22 ms → 0.2–3.5 ms with the completions' deferred one-time highlight riding the next paint; M33 stubbed-hljs replay re-measured 0.405/0.406/0.415 s vs 0.408/0.409/0.403 s interleaved — within noise, an interim walkTokens-recorder shape that cost +60 ms/replay (marked's hook routes the walk through Promise.all) was replaced by a plain-recursion recorder over the lexer's own tokens; M60 repeat-page 0.01 ms — no regression; 4740-passed suite plus 7 new stream-tail tests) | the block still growing at the draft's end renders escaped-plain during a streaming paint instead of re-running highlight per paint: parseStreamDraft lexes once, records the code tokens by plain recursion, and renders those same objects, so renderer.code skips the LAST code token by identity when its raw does not end on a closing fence — marked's own tokens decide, no line-level model of marked's block structure (list/blockquote dedent defeat one, as the review round found); usage.js sets the recorder around the parse with try/finally, and the escape rides an incremental prefix cache (escapeText maps characters independently, so a tail growing by appends re-escapes only the new bytes); the paint where the fence closes, and the committed render after the turn, highlight once and the cache serves every later paint, so the final frame stays byte-identical (parity true) and every completed block keeps today's bytes (cache-first ordering) |
| 2026-09-07 | this PR | M72 listing request median 21.19/20.97/21.06 ms → 11.31/10.92/11.02 ms, maxima 53.21/46.81/50.76 ms → 12.20/11.64/11.81 ms (three interleaved rounds of the collector, 1089-entry sessions root, live state read-only, main checkout before vs branch worktree after back-to-back at load 1.62/1.64/1.03, every paired round faster; served body byte-identical across arms — 239048 B, sha1 b7005c21bf13; 19-entry threads dir 2.41 ms → 2.00 ms; builder call alone 17.83 ms → 7.76 ms; live-before corroboration: verbatim M72-shape curls against the running instance median 35.4 ms over 5 at load 0.50/0.34/0.38, the instance predates this change) | the listing walk left the per-entry Path.iterdir double-stat pattern (the M29 conversion): one os.scandir pass answers is_dir from the directory record and stats each entry once instead of Path construction plus an is_dir and a stat per child, the row list joins once instead of += re-accumulation, time.gmtime renders the UTC mtime text without a per-entry datetime construction, and the route folds the is_dir probe into the listing's single executor hop (four to_thread round-trips per listing before); M72 definition and healthy range introduced with this PR |
| 2026-09-07 | #974 | recap repeat request raw-ASGI median 0.601/0.608/0.596 ms → 0.339/0.346/0.343 ms, maxima 0.855/0.787/0.823 → 0.471/0.478/0.437 ms (three interleaved rounds, 300 timed calls after 20 warm each through the real app stack, worst extract corpus of session aa196b47 — 36.3 MB chat events, divider 5518 — live home read-only, origin/main tree before vs branch after back-to-back at load 0.62/0.95/0.87, every paired round faster; parsed bodies equal across arms, raw bytes differ by design — the \uXXXX escaping, 2493 B → 3381 B; M20 repeat-divider extract re-measured 0.0000 s digest bb99828aa5b6 and M15 torn reads 0 / 14.4M reads over 3000 writes with the new memo-serving reader — no regression; 4749-passed suite) | the chat UI re-requests an open recap panel on every re-materialization, and each repeat paid two executor round-trips (extract + summary lookup) plus FastAPI's jsonable_encoder walk for answers the memos serve on the loop: the extract hit-check exposed (`extract_recap_memo_hit`), the summary cache parsed document memoized on the file's (mtime_ns, size) — the sole writer publishes through the atomic rename so any content change moves the signature, taken before the read, missing file answered (None, False) on-loop — and the route renders through FastJsonResponse |
| 2026-09-07 | this PR | M51 post-write deep probe median 3.63/3.74/3.42 ms → 2.57/2.06/2.26 ms, maxima 3.79/3.94/3.52 → 2.99/2.19/3.11 ms (three interleaved rounds of the collector, 339-file worst threads corpus of session 3b91d606, scratch CHARLIEBOT_HOME, main checkout before vs branch worktree after back-to-back at load 1.1-1.3, every paired round faster; no-regression re-measures: M21 sweep 0.0036 → 0.0034 s, M67 probe trigger scan 434 → 437 µs, M56 /status 2.17 → 2.12 ms with parsed-body digest 53882f56a2b3 identical, M40 starred 0.17 → 0.14 ms and groups 0.11 → 0.10 ms; 5 new single-walk tests) | the post-write probe paid its scandir+stat phase twice — the probe-input signature walk statted all 339 thread metadata files and 106 trigger files, then `has_running_tasks_sync`/`pending_trigger_state_sync` re-took the same walks to validate their parsed-content memos; the signature walk now returns its stat pairs (`_sidebar_probe_walk`) and the probe cores consume them (`walk_thread_meta_stats` homes the probe-side thread-dir walk; the boot recovery scan keeps its lazy inline loop for its short-circuiting consumers), so one post-write poll walks the corpus once — the walked pairs describe the walk's instant and the caller stores the probe result with that same walk's signature, so entry and signature always describe one state |
| 2026-09-07 | this PR | M15 verbatim collector no completion within 240 s (the writer starved mid-stream — a 20 s faulthandler dump shows it inside the write's pathlib path while the four readers hold the GIL in ~28 µs memo-hit iterations, 1000 lookups in 28 ms measured) → completes in 21.1 s: 3000 `_write_cache_entry` calls, 347512 concurrent reads, 0 torn reads (collector on this branch's code, scratch state; the writer-only floor is 2.28 s for the same 3000 writes) | #974's summary-document memo made the memo-hit lookup shorter than the interpreter's 5 ms GIL switch request, so the yield-free reader loop starves the writer of the GIL indefinitely and the torn-read watch can no longer run; the collector's reader loop now yields every 8 reads, keeping the write stream moving with the readers still running throughout |
| 2026-09-07 | this PR | M6 append-round usage resolution median 0.14/0.15/0.16 ms → 0.06/0.06/0.06 ms, maxima 0.40-0.43 → 0.32-0.35 ms (three interleaved verbatim-collector rounds, 20534-event worst live corpus of session d321b9ad, scratch CHARLIEBOT_HOME, main checkout before vs branch worktree after back-to-back at load 2.8-3.3, every paired round faster, parity True every round); unchanged-list warm resolve 98.9 µs → 12.1 µs median, max 226.2 → 42.5 µs over 300 calls (manager level, same corpus); raw-ASGI GET /usage through the real app stack 477 µs → 297 µs median, max 673 → 462 µs over 300 (scratch CHARLIEBOT_HOME, parsed-body digest identical 063b0a5e8966 both arms); no-regression re-measures: M26 advance 0.14 ms parity True digest e94c56635194, standing M6 HTTP collector against the live instance 0.023 s median (the instance predates this change); 4757-passed suite | the usage resolution paid one executor round-trip (~87 µs measured) on every call — even the facts memo's unchanged-list hit, the 3 s usage poll's steady state — the #865 hit-on-loop shape the message projection and worker events took; the memo's unchanged-list hit and appended suffixes within `_ON_LOOP_SUFFIX_CAP` (512 events; the fold measures ~0.4 µs/event so the worst on-loop hold, ~0.2 ms, stays ~25x under the 5 ms ticker floor) now answer on the event loop reading only the cache dict and the locked memo, while a cold cache, a replaced list, or a longer suffix keeps the threaded scan; the on-loop advance keeps the store contract (a copy is fed, never the stored fold) and the no-await window makes the length check and store atomic against other coroutines |
| 2026-09-07 | this PR | M66 merged build median 5.83 s → 5.40 s across six interleaved verbatim-collector rounds (main checkout 5.87/5.53/6.16/5.78/5.48/5.96 vs branch worktree 5.27/5.35/6.04/5.33/5.80/5.44, back-to-back at load 1.6-2.9; every paired round faster except round 5, +0.32 s at load 1.61-1.96; 191.2 MB / 496,099-event worst on-disk trace /home/chaoli/data/stage3_current_traces/221054_trace_rank000_step000110.json, scratch output under /tmp, artifact 15.6 MB.gz both arms; component corroboration: profiled build parse 2.41 s / batched dumps 1.60 s / walk ~1.4 s / gzip 0.53 s, and dumps of 20k real events 0.086 s → 0.078 s under the flag switch) | the batched serializer rendered every batch through the slower ensure_ascii=False encoder path — switched to ensure_ascii=True, the M35 finding (C encoder ~3x faster on CJK-bearing payloads, never slower on ASCII-only ones; both renderings parse to the same trace, pinned by a CJK round-trip test) — and the event walk hoisted the ph fetch (one dict lookup per event instead of the ph/ph/name triple) plus bound batcher/pid-map/flow-seq callables; the 2.25 s stdlib parse of the 191 MB corpus is the remaining floor; 4761-passed suite |
| 2026-09-07 | this PR | M52 save_chat_event append median 3120/3152/2998/3128 µs across four verbatim-collector rounds, maxima 19.1-28.7 ms (20534-event worst live corpus, scratch CHARLIEBOT_HOME, load 2.0-2.7, parity True every round) vs the 176/184 µs standing row; healthy range recalibrated median < 0.0003 s → < 0.005 s (docs-only calibration, no code change) | 36a61bd2 (2026-09-07, outside this loop) made every ndjson append fdatasync-durable before close against hard VM kills — the size-without-data NUL-hole incident on this deployment — a deliberate, test-pinned durability contract the M52 range never priced; the flush floor on this host's storage is ~2.8 ms (component check: open+write+close 22.2 µs, +fdatasync 2854 µs, fsync 2828 µs, held-fd sync 2781 µs, preallocated-file sync 2806 µs, O_DSYNC write 2806 µs, tiny-file sync 2786 µs — the flush itself, not open/close/allocation, is the cost), so per-append durability prices the streamed-turn delta path at ~3 ms per event (a 500-delta turn ~1.5 s on the broadcast-gated append), and the append code is already at that contract's floor: one to_thread hop, O_APPEND, write-all, sync-before-close |
| 2026-09-07 | this PR | M35 events page (the repeat page fetch the collector times) median 4.98/5.45/5.17 ms → 2.63/3.07/2.83 ms, maxima 6.07/5.73/5.94 → 3.22/3.30/3.60 ms (three interleaved rounds of the verbatim collector on the shared M35 snapshot of the 20534-event worst corpus, main checkout before vs branch worktree after back-to-back at load 1.0-2.0; body 633236 B and digest 46d1d509a0d6 identical across all six arms; handler-level corroboration 2.22 ms → < 0.01 ms median over 7 timed calls per arm, same sha; view 3.59-3.73 → 3.62-4.07 ms and bootstrap 2.71-2.79 → 2.71-2.95 ms unchanged — no regression; 4805-passed suite) | the chat UI re-fetches a page whenever it re-enters the viewport or the session reopens, and each repeat re-rendered the page — slice plus a 633 KB dumps, ~2.2 ms of the 2.2 ms handler; the rendered body now memoizes on the projection itself (LRU 4, keyed (before, limit)), whose published objects are immutable and whose every advance is a new object with an empty cache — the invalidation is the M26 swap itself, and the served bytes are the same fast_json_bytes render by construction |
| 2026-09-07 | this PR | M67 steady-state probe trigger scan median 428/437/437 µs → 4/4/4 µs, maxima 444/451/483 → 9 µs (three interleaved rounds of the verbatim collector, 108-file worst trigger corpus of session a481fbde, pending 1, live state read-only, main checkout before vs branch worktree after back-to-back at load 0.70-0.73, every paired round faster; no-regression re-measures on the branch: M21 sweep 0.0036 s, M51 post-write deep probe 2.09 ms, M24 list_triggers < 0.05 ms, M56 /status 2.00 ms — all at their standing readings; healthy range tightened median < 0.0005 s → < 0.00005 s with this PR; 4808-passed suite) | the scan's steady state still paid the scandir+stat walk plus the per-file memo loop on every call while its derived (pending count, earliest fire) state is a pure function of the directory's contents; the verdict keys on the directory's (mtime_ns, size) — every trigger-file write publishes through the atomic rename into the directory, and a rename that creates, replaces, or removes an entry moves the directory's own mtime_ns, the ground #950's list_triggers verdict stands on — so one directory stat serves the repeat scan; the probe's walked shape keys on the walk-instant signature `_sidebar_probe_walk` now carries (`_WalkedProbeInputs.trigger_dir_sig`), so a write landing between the walk and the scan keys the older signature and can never be served for the newer state, and a corrupt trigger file re-reads and re-warns once per proved directory state instead of once per scan |
| 2026-09-07 | this PR | M72 listing request median 10.94/10.94/11.77 ms → 9.60/9.52/9.89 ms, maxima 12.14/11.95/12.89 → 10.81/11.12/11.05 ms (three interleaved rounds of the verbatim collector, 1088-entry sessions root, live state read-only, main checkout before vs branch worktree after back-to-back at load 1.35-1.42; served body byte-identical across all six arms — 238829 B, sha1 f9afb0828dde; builder alone 7.04 → 6.62 ms median over 9 calls; component microbench: per-1088-entry escape+quote 1.13 ms against the fast path's fullmatch check 0.23 ms; 4806-passed suite) | the per-entry rendering paid two html.escape calls (five str.replace invocations each) plus a urllib.parse.quote per entry while session ids (UUIDs) and artifact names draw from characters where both are the identity transform — a name over [A-Za-z0-9_.~-] renders by interpolation and only the rest pay the escaping calls (the byte-identity ground, pinned by the reference-walk test's mixed corpus); the per-entry stat walk (3.2 ms per 1088) is the remaining floor |
| 2026-09-07 | this PR | M4 hung 1 → 0 (collector verbatim against the live home at load 0.67/0.30/0.46: old form "1 running sessions with last event older than 1h", new form 0 with the turn stats unchanged — 27 turns, median 220 s, max 1540 s); scratch-home shape check: an ACTIVE session with a running thread and a 2 h-old chat file still reports 1 hung, the same shape archived reports 0 | docs-only calibration: the collector counted an archived session's stale "running" thread marker as a hung session — session 80507dda (memory-reviewer-dryrun-gemini, archived 2026-09-04 23:43) flagged hung for ~3 days of rounds while its only "running" thread is the marker `_scan_interrupted_runs` deliberately leaves alone (archived sessions' threads are not work to resume, `_session_archived`); the collector now reads the session metadata's status only when a thread claims running and applies the same archived rule, so the watch keeps catching genuine active-session hangs at zero extra scan cost; healthy range unchanged (hung = 0) |
| 2026-09-07 | this PR | M23 8-page scroll steady-state median 0.0007/0.0007/0.0007 s → 0.0003/0.0003/0.0003 s, max 0.0007 → 0.0003 s (three interleaved verbatim-collector rounds, 8.2 MB / 2-file worst archive corpus of session 92db85f7, archive_offset 2799, scratch CHARLIEBOT_HOME, main checkout before vs branch worktree after back-to-back at load 1.4-2.7, every paired round faster; profile attribution: the per-call archives-dir glob's pathlib machinery ~75 µs of the ~84 µs page turn); no-regression re-measures interleaved ×2: M20 repeat-divider extract 0.0000 s both arms with digest bb99828aa5b6 identical, M30 steady-state 0.0002 s / append-round 0.0003 s both arms, M26 advance 0.17-0.21 ms parity True digest e94c56635194, M6 append-round 0.05-0.06 ms; 4830-passed suite plus 2 new multi-file range tests | every archived-session page turn re-ran the archives dir's pathlib glob — scandir plus per-entry Path construction and fnmatch per call — and extended every archive file's whole parsed list only to slice the 200-event page; the file list now memoizes on the archives dir's own (mtime_ns, size) — a membership change creates or removes a directory entry and either moves the dir's mtime_ns, while a same-week append moves only the file's own signature and never invalidates the list — and the range concatenates only files overlapping the requested span, each file's length read off its parsed-list memo |
| 2026-09-09 | this PR | M83 warm-cache revalidation requests per page load 46 → 0 (46 versioned assets; three interleaved verbatim-collector rounds, main checkout before vs branch worktree after back-to-back at load 0.64-1.12; before arm: immutable header on every versioned response False, revalidate-request median 0.45-0.47 ms/asset, cold 200 median 0.80-0.86 ms, page-load serve work at the pre-fix one-request-per-asset shape 20.9-22.1 ms; after arm: header True on all 46, revalidate median 0.45-0.46 ms/asset, cold 200 median 0.83-0.86 ms — the serve path's walls unchanged, the count is the win); live-instance corroboration: revalidation-shaped curl against the running server (which predates this change) median 1.5 ms/asset over 5, ~68 ms of serve work + 46 round trips per dashboard page load at the served asset set; M83 definition and healthy range introduced with this PR | the static mount served default caching, so the browser revalidated every template-referenced asset (?v=<runtime git version>) on every page load — the per-asset stat+etag round trip lands on exactly the files the latency loop edits most often, whose fresh Last-Modified keeps the heuristic cache from ever covering them; the mount now marks a 200 whose request named a version `public, max-age=31536000, immutable` (a response that named its version names its content — the token is the runtime git version plus the served tree's content digest, refreshed per page render, so a working-tree edit between restarts changes the token on the next render), so a warm-cache page load issues zero asset requests; a request without a version parameter keeps default caching because its URL can outlive its content, and a 304 keeps the cached 200's own headers |
| 2026-09-09 | this PR | M56 /status sweep reading median 3.52 ms, max 15.38 ms (40 sidebar ids, body 8832 B, digest 7816ce18efa9) tripped the < 0.0028 s line inside the standing collector sweep at load 4.29-4.60 while the code-health cron's worker ran; three standalone verbatim-collector re-measures minutes apart read 2.06/2.11/2.18 ms medians, maxima 11.23-12.71 ms (digest 75cf9b8f64b8, body 8830 B — the live corpus moved between the arms), each at load 4.13-4.60; the raw-ASGI handler over the same id set reads median 302 µs, max 480 µs over 200 calls — the TestClient request adds the ~1.7 ms harness floor the M63 row documented; every post-fix quiet reading since the 2026-09-04 landing sits at 1.93-2.60 ms | docs-only calibration: healthy range median < 0.0028 s → < 0.004 s — the old line sat below the collision-biased sweep reading while every quiet reading passes it; at the TestClient level a 2x handler regression adds ~0.3 ms to a ~2.1 ms reading and trips neither line, so the change costs no sensitivity the old line had, and the definition row now names the harness floor so a tripped M56 reading is read as host load before code |
| 2026-09-11 | this PR | M30 cold archived-session tail-page read 69.3/70.6/70.8 ms → 2.9/3.0/2.9 ms (~24x; interleaved arms, one fresh process + fresh scratch CHARLIEBOT_HOME copy per run, biggest archived session dfe393f7 — 14.6 MB live file, 7680 total events, archive_offset 1055, 2.5 MB archive, live home read-only, main checkout before vs branch worktree after back-to-back at load 1.71/1.41/1.09; output identical across arms — 200 events, has_more True; live-log corroboration: the one ≥100 ms archived events read in the 12 h server log is this path's cold whole-file build, 528 events requests per 12 h); walk-output parity: 300-trial randomized single-window fuzz (random hole counts, CRLF/CR/LF and unterminated tails, random windows, random archive offsets) and 120-trial scroll/append sequence fuzz (backward-extension chains, appends mid-scroll, warm repeat reads) 0 mismatches; unchanged-file steady state untouched (M30 0.0002/0.0003 s standing row, no-regression suite re-run); 5048-passed suite plus 5 new walk tests | the live-half range read for an archived session still built the whole file's per-physical-line memo (~440 ms/MB of parse+decode measured earlier; 75 ms cold on this corpus) to serve a ≤200-event tail page, and the count's line domain gave the walk a sound ground: count_ndjson_lines counts \n only, the raw scanner is PEP 278, and the writers' machine-written \n-clean files make the two domains agree — the read now walks the requested span from the file's end in 512 KiB chunks and stores a suffix-covering memo entry (the memo tuple grows covered-byte-end, line-start, and byte-start fields), a scroll-back extends the coverage backward page by page instead of re-walking, spans past a 32 MiB byte budget or reaching line 0 fall back to the existing full build, and the stat-bracketed count gate (count → re-stat match, else full build) keeps the line-domain answer provable against the file actually read |
| 2026-09-11 | this PR | M60 highlight flush 0.628/0.611/0.604 s → 0.208/0.205/0.184 s, −67 % to −71 %, restoring the 09-07 standing row (0.174-0.183 s); three interleaved verbatim-collector rounds, 40 largest bodies / 57.7 KB, page-corpus sha1 490505464120, marked + hljs 11.9.0 common build, main checkout before vs branch worktree after back-to-back at load 1.64-2.23, every paired round faster; cold first paint 0.045-0.058 s both arms and repeat-page median 0.01 ms, max ≤ 0.06 ms unchanged, parity true every arm; component corroboration: the vm timer-drain probe over the same 40-record corpus 638 ms → 249 ms; 50-passed frontend suite plus this PR's retry pin, 304-passed stream/render/chat pytest slice | the wide-char 2ch-box commit (1b40fe6c) made every flush pass re-run the settled work: the deferred-highlight flush rebuilds each record's settled block (codeBlockHtml, now paying wrapWideChars) and re-swaps the memo on every pass, and records no sweep finds repeat up to HIGHLIGHT_FLUSH_MAX_ATTEMPTS (120) times — the marker-less vm the collector runs hits that shape on every record; the record now builds its settled block once (rec.settledBlock / rec.highlighted) and the retries only re-sweep for markers — the memo swap rides the first build because plainBlock embeds the record's unique id, so a later pass can never find it again, and a late-attaching marker receives the same stored bytes |
| 2026-09-11 | this PR | M86 blocked-round repeat (the corpus-as-it-stands parity witness, now a steady-state shape) median 1.98/2.95/4.10 → 0.00/0.00/0.00 ms, maxima 2.33/3.25/4.15 → 0.02/0.02/0.02 ms (three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, 20534-event worst live chat file of session d321b9ad, scratch CHARLIEBOT_HOME per round, live home read-only, verdicts identical blocked both arms, every paired round faster at load 2.30-3.36 one-minute); delegation-flow shape unchanged 0.00 ms / max 0.00 ms both arms with the allowed verdict asserted; offline randomized parity: 3600 gate calls across 300 appended histories judged against the verbatim forward walk, 0 verdict mismatches, plus the pre-existing 400-history and 39-gate-test suites; 5280-passed suite plus 7 new memo tests (append-fold across turns, the early-break under-fill corner the memo closes, suffix stamp overrides prefix stamp, no-user suffix keeps the takeoff window, list replacement rebuilds, empty-history appends, and the cold walk's claim-only-the-scanned-span store under a mid-walk append) | every delegation POST re-walked the busiest master session's whole chat file whenever the verdict is blocked — the walk the backward scan kept for blocked misfires, 2.0-4.1 ms at 20,534 events today and growing with the corpus without bound; the verdict's two answers are complete prefix facts of the event list, so an answers memo keyed on the cache list's identity now carries them across calls: a cold or replaced list pays one full backward walk with no early break (the break the scan had could leave the stamp answer unset behind a takeoff-allowed verdict, and the suffix fold needs complete prefix facts), an appended suffix folds by scanning only the suffix (a user message in the suffix is the new file-last one; the prefix's stored stamp answer is the file-older bound the backward continuation would stop at), and the store claims exactly the scanned span so an append landing mid-scan is folded by the next call — the M6 usage-fold memo's identity contract; the cold walk's no-break widening fills stamp answers the early break skipped, which can only add an allowance the takeoff phrase had already granted, so the verdict stays the forward walk's; blocked-round sub-metric added to the M86 definition with this PR |
| 2026-09-11 | this PR | M36 list body 186100/186100 → 164386/164386 B, −11.7 %, and M63 view body 203192/203192 → 181478/181478 B, −10.7 %, byte-identical within each arm (four interleaved rounds of the verbatim collectors — main checkout before vs branch worktree after back-to-back, worst on-disk threads corpus 4551 KB / 517 rows of session dfe393f7, live home read-only, load 3.4-4.7 one-minute; handler medians within noise 2.2-2.7 ms list / 5.6-8.5 ms view at that load — the dumps was never the handler's budget, the bytes are the moved metric); conditional 204/0 B both arms both metrics; 5419-passed suite plus 2 new wire pins (epoch-ms row timestamps, the mixed thread+trigger sort's int-vs-int key) | the shared row builder serialized both timestamp fields as ISO strings (~52 B per field per row) on a body that scales with the session's thread count — the worst corpus grew 499 → 517 rows in the last day and would cross M36's < 200 KB range within the week; the client reads both fields through new Date(), which accepts the epoch-ms integer and the ISO string alike, so the rows now carry ints (the trigger row's fire_at/created_at convert the same way so _list_body's mixed sort stays int-vs-int) — zero client change, −21.7 KB per full body on both payloads, JSON.parse on the client shrinks with it |
| 2026-09-12 | this PR | M94 tail-40 page body median 1.01 MB → 0.20 MB, −80 %, page dumps median 2.0 → 0.5 ms, build unchanged 1.0-1.2 ms (three interleaved rounds of the verbatim collector — origin/main tree extracted to a scratch checkout before vs branch worktree after, back-to-back ×3, worst active live corpus 9313ed43 with the 1.11 MB tool_result, live home read-only, load 0.88-1.07 one-minute); streamed replay serialized median 6.6 → 5.5 MB, dumps wall median 24 → 21-22 ms (the commit frames' tool rows join the stream deltas at the preview bound); M35 events page body 624218 → 347923 B, −44 %, median 2.67 → 2.58 ms, view body 179490 → 135521 B, −25 %, bootstrap body 93303 B both arms (the per-request trim is now identity on pre-trimmed rows; the digest move is the documented mark_read write-once skew of the shared snapshot); trim-contract parity: every message's non-tools fields byte-identical across arms and all 136 trimmed values strict prefixes with their markers set on the worst page; no-regression witnesses interleaved before/after: M26 advance 0.16 → 0.18 ms parity True digest e94c56635194 identical, M33 replay wall 0.035 → 0.037 s parity true, M34 body 251479 B byte-identical both arms, M38 fan-out 5 frames / 186 dumps 5 → 4 ms parity True, M45 loop-lag 0.0064 → 0.0067 s at the 5 ms ticker floor with wall flat (the frame-list digest moves by design — the replayed commit frames now carry preview-shaped rows); 5498-passed suite plus 607-passed node suite with the committed-shape contract tests flipped to the preview bound | the events pages and the WS commit frames were the last chat wire shapes still carrying whole tool rows — 20 KB-capped outputs plus full input values (the M94 worst page read 76 % tool outputs, 16 % inputs, while the renderer displays an output's first 500 characters and reads from an input only a bounded summary, all inside the tools block hidden behind the "N tool calls" toggle) — the trim now lands once at ingestion (tool_preview on every buffered row), so the stream delta, the committed message, and the bootstrap payload share one wire bound and the buffered fold itself stays bounded; TOOL_OUTPUT_RENDER_CAP remains the worker-events projection's cap (src/api/threads.py, the M34 contract, body byte-identical this round); the committed bubble's shape now matches the streaming bubble's (500 chars + the raw-events note) instead of expanding at commit time; full text stays on the persisted event where the raw download, the fork reference, and the review scans already read it |
| 2026-09-12 | this PR | M71 capped search handler wall median 1.996/2.032/1.996/2.017/1.934 → 1.680/1.691/1.683/1.662/1.654 ms, −16 %, every paired round faster (five interleaved rounds of the direct-handler harness — search_sessions(q="e") awaited 30x per arm after one warm call, 200 rows, shared snapshot of the 1090-meta scratch home, main checkout before vs branch worktree after back-to-back at load 0.84-0.85, body 208496 B and sha1 6ac7eb4b99e7388d byte-identical across all ten arms); route-level corroboration through the verbatim TestClient collector, six interleaved rounds: 4.63/4.13/4.14/4.42/4.12/4.09 → 4.20/4.14/3.83/4.35/3.97/4.46 ms medians (digest 631e3b9ba12e identical across all twelve arms; the ~±0.3 ms TestClient floor noise bounds the route reading, the handler wall is the signal); no-regression re-measures interleaved ×2: M56 /status handler 0.12 ms flat, byte-parity suite (the merged-render test) passed, 5509-passed suite | the capped search's per-row splice re-rendered each derived key's wire prefix per row per request (b'"' + name.encode() + b'":', ~1000 encode+concat pairs per 200-row response) and ran the pydantic dump_python call for both datetime fields even when None — the common idle row's shape; the five prefixes are prebuilt module bytes the splice joins directly, and a None datetime field rides its whole prebuilt ``"key":null`` piece so neither the dump_python call nor the scalar render runs for it; the spliced body is byte-identical (pinned by the merged-render reference test) |
| 2026-09-13 | this PR | M62/M82 standing-collector repair: both collectors read their helpers as direct attributes and the getattr(…, None) fallback arms are gone — M62's else-arm re-priced the removed pre-fix three-probe chain through `git_remote_default_branch`, a name no checkout has carried since the M62 landing, and M82's aiofiles arm silently timed the removed pre-fix write+flush pair, the exact fallback the M89/M90 repair removed from its collectors (collector commands only, no product code); repaired commands read, three interleaved rounds at load 1.32-1.34 one-minute: M82 events-log append median 2/2/2 µs, maxima 7/12/8 µs, and M62 base-less base-resolution chain median 0.1896/0.1968/0.1956 s, maxima 0.2226/0.2101/0.2306 s, start_point origin/main round-stable across all rounds — both inside their standing ranges (< 200 µs and < 0.5 s) and consistent with the M91-row and 2026-09-13 M62-row readings | the getattr-plus-fallback dispatch was each landing's before/after A/B form and became a trap once the landing completed: the fallback has no failure mode, so a renamed helper silently re-prices a removed shape (the #1285 vacuous-read class, the M89/M90 repair's stated standard) — a checkout whose module lacks the name now fails the collector loudly instead of timing a shape the code no longer runs |
| 2026-09-13 | this PR | M7 restart-cold collect wall 6.963/7.137/7.046 → 6.035/6.093/5.991 s, −13.3 % to −15.0 %, every paired round faster (three interleaved rounds of the verbatim collector — worktree before vs after back-to-back at load 1.05-1.10 one-minute; rows digest 00ed6e8fc56f and scanned 2062.6 MB identical across all six arms); component attribution (cProfile, pre-fix cold pass): `_parse_lines` ran 214,420 stdlib loads over the marker lines — decode 1.85 s + raw_decode 1.33 s of the 7.2 s wall — plus the per-line replace-decode copy, and the metadata reads joined at 4,808 `json.loads` calls; parse parity: all 1,493,027 marker lines across the live claude/codex/charlie-bot corpora parse orjson-identical to the stdlib replace-decode, 0 divergences, 0 stdlib fallbacks (no invalid UTF-8, no NaN/Infinity, no ≥2^64 ints anywhere in the corpus); no-regression witnesses: M7 changed-round collect median 0.109 s, max 0.113 s over 5, 19 rows, 0.0 MB re-read (standing 0.150 s on the parked pr-1503 main checkout, 0.11 s on post-#1505 code), and the rows-digest jitter isolated from the fix — run-to-run digests move only with live opencode traffic, whose scan rides the SQLite projection this change never touches; 5529-passed suite + 11 skipped unchanged, ruff clean; standing context for the next run: today's live-document restart-cold reads 7.198 s because the live server (started 2026-09-10 12:42) predates #1352 — its hourly /token-usage rewrites carry a version-1 document without the charlie-bot source, so the fresh-code collect re-parses the 2 GB corpus once per reading (the transition shape documented 2026-09-11); the post-deploy shape measured 1.276 s fresh-process against a version-2 document carrying the section, 6354/6355 per-file lookups hit — the M7 restart-cold healthy range (< 1.2 s) will sit marginally tight at ~1.28 s until the next recalibration | the marker-line parse paid the stdlib C decoder plus a Python-level replace-decode copy per line while orjson parses the same bytes ~2x faster per line measured on the live corpora (the ndjson.py precedent); the swap rides orjson on the raw line slice and falls back to the exact stdlib replace-decode on any orjson rejection, so today's tolerance for invalid UTF-8 and NaN/Infinity literals is preserved verbatim, and the metadata.json read joins it under the same ValueError contract its callers already catch |
| 2026-09-13 | this PR | M7 restart-cold healthy range median < 1.2 s → < 2.0 s (docs-only calibration, no code change). Post-deploy-shape evidence, fresh process against a version-2 document (the live cache document copied to scratch, rewritten to the current shape by one fresh-code collect; live home read-only, live corpus never written): zero-movement walls 1.178/1.109/1.142 s over three fresh processes with collect-only 1.157/1.166 s in two more, 19 rows, 0.0 MB corpus re-read, load 1.44-2.21 one-minute across the measurement window; the previous round's one-moved-file post-deploy reading was 1.276 s. The standing collector's same-day reading stays the transition shape — the live server (started 2026-09-10 12:42, its log now 75 h) predates #1352, so its hourly rewrites carry a version-1 document and the fresh-code collect re-parses the 2 GB corpus once per reading (3.677 s today page-cache warm; a colder pass of the same shape measured 6.192 s minutes earlier) — a deploy-skew shape that heals at the next restart, not by code. Component attribution on the fresh-process collect (cProfile, 1.25 s profiled): `_merge_opencode` 0.588 s (the db key pass `_advance_opencode_rows` 0.438 s plus the 16338-record fold), document load 0.217 s (orjson 0.171 s on the 27.1 MB document), `collect_claude` 0.204 s, `_walk_charliebot` 0.163 s (17237 stats over 6369 candidate files), cache save 0.087 s, plus the 0.18 s import floor — every slice grows with its corpus, so the floor rises with the data the metric serves | the 1.2 s line was set 2026-09-10 against a corpus whose post-fix restart-cold floor measured 0.93 s; the corpus has since grown to the 2062 MB / 27 MB-document shape whose zero-movement floor sits at 1.11-1.18 s — zero headroom, so any live corpus movement (the server appends constantly) trips the line on healthy code, as the 1.276 s reading and the #1509 row's margin note predicted. < 2.0 s clears the measured floor with ~1.7x headroom and still trips at the transition shape's 3.7 s+ |
| 2026-09-17 | #1757 (row recorded in this docs-only follow-up per the #1046 precedent, the landing PR shipped without it) | M66 merged build median 4.28/4.18/4.29/4.21/4.20/4.40/4.18/3.98/3.97 → 4.05/4.09/4.17/4.24/3.96/4.29/4.06/3.98/4.01 s over nine interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, 307.3 MB / 1,068,461-event worst on-disk trace, scratch output under /tmp, live home read-only, load 2.6-4.0 one-minute: mean −93 ms (−2.2 %), 7 of 9 paired rounds faster, the two laggards within +40 ms; component attribution, same-process walk-phase probe (read+parse+walk of `_merge_one_trace`, null output, gc off, interleaved ×2 per arm): 3.639/3.682 → 3.573/3.635 s medians (−47/−66 ms) against the unchanged parse+read floor; artifact sha1 78347835299a identical across all four cross-checkout builds (deterministic gzip holds); no-regression witnesses interleaved: M88 direct-pass build 2.93/2.76 → 2.72/2.68 s at settled load 3.9-4.8 (its path runs no walk; a +0.1-0.2 s pair at load 5.2-5.5 tracked the run's own collector load); 5699-passed suite plus 2 new tests (the member count contract, the ≤512 batch bound across a 400-pid corpus whose metadata tail outmasses its events) | every event of a merged build paid a `_EventBatcher.add` bound-method round-trip — attribute reads, append, emitted increment, bound check — on top of the append itself; the walk now appends to one local pending list and flushes through the batcher at the same 512-event bound (every append site checks, the trailing process_ metadata loop included — the review's instrumented 400-pid corpus caught the first cut letting that tail ride one 1455-event batch), `emitted` counts at flush, and the tid read probes once via a sentinel; batch boundaries are byte-invisible so the artifact is unchanged, and the M107 member form rides the same walk |
| 2026-09-24 | this PR | M3 in-server 401 floor median 20.48/19.68/19.96/19.59/19.24/20.64/18.66 → 11.36/11.35/11.33/11.53/11.59/11.45/11.45 µs over seven interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, every paired round faster (−8.1 to −9.3 µs, median −8.2 µs / −42 %), p90 21.4-32.3 → 12.4-19.9 µs, load 5.8-7.0 one-minute (the run's own suite aftermath, both arms alike); component attribution: the renderer round trip (dict merge + sort + field scan + print) ~7.6 µs of the floor, the per-call stamp format ~1.4 µs vs ~0.4 µs with the per-minute prefix memoized, the per-send 401 json.dumps + header build ~1.8 µs; wire bytes unchanged — the composed line equals the lean renderer's output per value shape (tests/test_log_line_renderer.py) and end to end through the middleware (tests/test_request_logging.py), the 401 bodies byte-identical (tests/test_auth_middleware.py); 6017-passed suite plus 11 new tests (the 20 vfork/antigravity/perfetto failures pre-existing on a clean tree in this venv — the compiled _vfkspawn stub and the antigravity CLI are CI-only) | the access line rode the general lean renderer per request — a per-line dict merge, sort, and field scan, plus print's two unbuffered writes (PYTHONUNBUFFERED=1 to the tee pipe) — the largest repo-owned slice left on the 401 floor (measured 40 % of it); the line composes from the fixed column pads and the one shared value rule now, single stdout write; the stamp's year-through-minute prefix memoizes per minute behind an atomically swapped (key, prefix) tuple whose reader builds its own prefix when its minute differs from the memo's (an interleaved rollover never mislabels a line); the auth middleware's 401 bodies and content-length headers are module constants — every unauthenticated request paid a json.dumps and a header build |
| 2026-09-25 | this PR | M71 capped name-match repeat median 144.39/141.98/139.57 → 2.72/2.74/2.56 ms over three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back at load 0.72-0.99 one-minute, same scratch snapshot (1,326 metas, query 'e', 80 matching legacy parents carrying 940 live thread metadata files), identical 1,140-row bodies and digest 91f384b33138 every round (−97 to −98 %, ~52x); regression window pinned by the same A/B against pre-landing 9f77cc47: 1.81/1.95/2.05 ms at 200 rows (the calibration band) on the same snapshot — the fan-out arrived with d29d44aa's projected worker-leaf rows; mechanism: both per-session row memos were capped at 8 sessions while the fan-out walks one view_thread_rows call per legacy row of a response (80-200 sessions), so every request evicted and re-walked, and the rebuilt row dicts broke the projected-row memo's identity checks and the search route's whole-body memo (profile: 9,400 thread-metadata parses + 9,410 file opens per request); no-regression witnesses, branch vs main interleaved rounds: M36 full poll 0.57 ms (main 0.59), M63 /view handler 0.40 ms (main 0.57), M68 marked changed-poll rebuild 1.19 ms (main 1.09, line < 2 ms), M77 re-entry 0.04 ms 0/36 rebuilds (main 0.05); 6467-passed suite including the new fan-out identity test (the 20 vfork/antigravity failures pre-existing in this venv — the compiled _vfkspawn stub and the antigravity CLI are CI-only); the M96 standing collector 404s since the same landing (82 of 116 listed rows are projected worker-leaf rows whose ids are thread ids — no bootstrap fetch exists for them) and its collector now skips them, re-measured live: 34 session rows, median 88,795 B, p90 176,825 B, max 272,491 B (lines hold); the two session-tree preview tests d29d44aa landed fail on CI (no charlie-code launcher on the runner — one calls the in-process check unstubbed, one runs the CLI subprocess whose PATH carries none) and the PR carries the suite's own check_launcher stub plus a fake --session-dir launcher on the subprocess PATH | the sidebar projection's fan-out outgrew the single-session caps the row memos were sized for: a cap below the fan-out's working set turns every capped search and sidebar list into a full re-walk and re-parse of every matched legacy session's threads and a full re-render of the response, and the cost grows with both the matched-session count and the thread corpus; the shared cap now holds the working set, which puts the repeat back on the whole-body memo's identity path the route was built around |
| 2026-09-25 | this PR | Collector scratch leak, fixed in this PR: the M35/M55/M70/M71 pair consumers and the M66/M84 builders left their /tmp scratch on every exit path — measured before the fix: 11 `m71-search-home` dirs × 168 MB (one per round since the collector's introduction), 24 `m66-merge` dirs × 20 MB (the merged artifact itself), 7 `m55` + 7 `m70` homes, 5 `m35` homes × 28 MB, `m*` scratch total 5.3 GB against a root fs at 80 % (154/193 GB) — ~200 MB per hourly round, ~4.8 GB/day, against a tmpfiles reap that only removes 30-day-old files; after the fix the six blocks re-run verbatim with zero scratch left: m35 digests byte-identical (8a0ebe24d65f / bf4371606224 / 15209ed7bd16, medians 0.80/1.31/0.98 ms), m55 digest e9447638231a identical, m70 digest bd0194098a8d identical, m71 rows 1163→1164 with the live corpus's one-session growth and median 2.54→2.59 ms (snapshot corpus, digest follows it), m66 build 2.78→2.78 s / 20.2 MB artifact, m84 parity divergences 0 and tail-follow 5926→6016 ms on the same 2.1 GB corpus (both under the bytes line); the ≤1 MB per-round homes (m105, m102, m10, m14, m15, m16, m41, m43, m48, m50, m53, m79, m82, m89, m90, m100) still leak and stay for a later round | the leak is the loop's own tooling: these blocks are this file's verbatim collectors, their scratch copies land in the host's /tmp, and a disk-full measurement host invalidates every metric this file owns — the removal rides the consuming block's exit path (try/finally around the run) so neither a failed round nor a failed build can skip it |
| 2026-09-25 | this PR | M71 whitespace-query list serve median 5.24/5.87/5.47 → 1.12/1.18/0.86 ms over three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back at load 2.88-3.21 one-minute, same snapshot per round (242 rows, decoded 732251 B, digest 052bca721774 identical across all six arms); the capped name-match shape the fixed builder picks re-measured in the same rounds: q='e' 948 rows, 12.18 → 3.93 ms median (digest 57bb8824ce93 identical, both arms at load 3.21), quiet in-process repeat 1.49 ms; before this PR the capped repeat re-scanned every content-hit file per request (46 probes × one 256 KB chunk on this snapshot's 193.7 MB active chat corpus, ~8-12 ms of overlapped executor scans per drive) because hits were never memoized — only absences were |
the round's verbatim collector tripped its 0.003 s line through a collector bug and a serve gap: the builder's worst-query pick crossed a route branch (a whitespace-only query routes to the list shape, never the capped scan), and the list branch it measured served through response_model with the per-row copy+populate pass instead of the whole-body memo its sibling shapes ride — the branch now feeds the same _serve_search_rows render (identity-keyed whole-body memo, spliced row bodies, parity-pinned to the copy-path render), and the capped shape's content-hit rescans ride a signature-gated hit-root memo beside the absence roots (a stored needle's hit answers its substrings while the inode holds and the file has not shrunk — the append-window convention the miss roots run, inverted); the builder now skips whitespace candidates so the collector measures the capped shape its definition names |
| 2026-09-26 | this PR | M9 steady-state spend rescan 0.6835/0.6672/0.6794 → 0.0023/0.0022/0.0024 s over three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back at load 1.20-1.40 one-minute (maxima 0.684-0.696 → 0.0027-0.0030 s, every paired round faster); spend parity witnessed across arms by the sum digest dac0e06518b4 and the per-file extract digest 7bca2f1f30b6 over all 543 in-window files | the 7-day window's file count outgrew the spend memo's 512-entry cap (539 in-window at the 2026-09-26 sweep, 503 written 09-25 alone), and the cap turned the LRU into a rotating eviction wave: each round's first 27 misses re-record and evict the next round's hits in walk order, so every round re-parsed the whole corpus (0.67 s of parse per 60 s poll round, background work invisible to the HTTP probes) — the cap now sits at 8192, above the window's resident set, and the regression test seeds 600 in-window files to pin the steady round memo-served |
| 2026-09-26 | this PR | M59 thread-detail poll, verbatim collector: full row 0.81/0.84/0.83 → 0.53/0.51/0.50 ms (−35 % to −40 %), maxima 1.14/0.98/1.01 → 0.72/0.60/0.72 ms; attach mode 0.75/0.74/0.73 → 0.41/0.39/0.40 ms (−45 % to −47 %), maxima 0.83/0.77/0.74 → 0.47/0.43/0.42 ms, every paired round faster over three interleaved rounds — main checkout before vs branch worktree after back-to-back at load 1.34-1.47 one-minute, decoded 50206 B wire 22820 B and digest 7184f3458354 identical across all six arms | the 5 s workers-panel detail poll re-read and stdlib-json-parsed the 42 KB sessions/session_aliases.json (205 rows) on every request: `_resolve_v2_run`'s alias probe measured 0.279 ms of the route's 0.78-0.85 ms — the store's `_read` had no memo, and `resolve_thread`'s two lookups (`old_threads` direct, then the canonical-owner retry) parsed the file twice per call; the read now rides the file's stat signature (the `_detail_meta_memo` mechanism), the served value is shared read-only, and `_put` writes a copy so a registration never mutates the entry concurrent resolvers hold |
| 2026-09-26 | this PR | M120 tree-page serve, introduced with this PR: roots page over the invalidated index 58.19/59.32/58.57 → 3.34/3.39/3.47 ms median (−94 %), maxima 60.87-73.14 → 4.03-4.21 ms, every paired round faster over three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, arm order alternating per round, 1404 metadata files plus 71 task-node event corpora in one shared scratch home, live home read-only; page body sha1 23a8c43f258e and tree revision 64c8bda9aab9 identical across arms | the index build re-read and re-parsed every session's metadata.json per rebuild (1404 opens plus pydantic parses, ~41 µs/meta, 58 ms against today's corpus) although the SessionManager already holds every entry behind the shared per-entry stat-signature check — the production log's per-request durations showed it (GET /api/sessions/tree 75-129 ms per request, median 92 ms, under a 2 s index TTL that any metadata write invalidates); the build now snapshots the authoritative entries on the loop (fresh_cached_metas, one _fresh_cached_meta check per entry) and reads a file only where no entry covers the name (cold cache, out-of-band create), keeping the tree's strict unparseable-file contract; the tree index joins get_session and _load_session_metas on the same freshness check instead of carrying its own whole-corpus re-read; 6635-passed suite plus 2 new tests (the shared authoritative entry served zero-copy with the out-of-band past-TTL edit re-read, and the uncached unparseable file failing the build loud) |
| 2026-09-26 | this PR | M121 task-tree index rebuild burst, introduced with this PR: 6-reader burst 27.32 ms → 2.12/2.12 ms median (−92%), max 34.95 → 2.35-2.53 ms, builds per burst 6 → 1, solo invalidated rebuild 2.37 → 2.06-2.09 ms (band parity), revision digest 64c8bda9aab9 identical across arms, interleaved back-to-back verbatim-collector rounds — main checkout before vs branch worktree after, plus the 12-reader probe 166.3-168.2 → 2.2-2.5 ms (−99%) | every reader of one invalidation ran its own full `_build_index_sync`: a delegation's create invalidates the index and the sidebar poll, the tree page, and the delegate's own read each paid the build again — the 2026-09-25 17:10 burst logged POST /api/internal/delegate at 766-1156 ms and GET /api/sessions/ at 765-1339 ms while three builds raced (the live corpus's solo build measures 78-375 ms, so the contended shape, not the 2 ms scratch band, is what production pays); the build now single-flights on the in-flight task keyed to its generation (the token-usage route's rule), the existing generation guard still refusing a mid-build write's stale install; 70-passed task-tree suites plus 2 new tests (one build per burst, mid-build write never installs) |
| 2026-09-26 | this PR | M122 stream event discovery delay, introduced with this PR: append-to-yield median 73.6/77.6/74.6 → 8.2/8.2/10.2 ms (−89% to −90%), maxima 142.0-147.0 → 11.2-19.3 ms, every paired round faster over three interleaved rounds of the new collector — main checkout before vs branch worktree after back-to-back, arm order alternating, at load 0.6-1.2 one-minute; parity witnesses on the same loop: M84 2.1 GB backlog replay tail-follow median 5980.4 → 5922.0 ms and stdout-stream 6591.6 → 6515.6 ms (band parity — the backlog drain is one parse round, the poll never enters it), M118 grown-line drain wall 2.93 → 2.93 s, max tick gap 74 → 89 ms inside the 0.15 s line; the trade: idle follow CPU 1.7-1.9 → 8.8-10.0 ms per 2 s (0.08-0.09% → 0.44-0.50% of one core per followed stream at 7 → 50 wakes/s) | the tail-follow loop's `_TAIL_POLL_INTERVAL` sat at 0.15 s from the coarse-message era (its comment cited a ~54 s median inter-event gap), and that interval is the discovery delay it adds to every event the CLI writes — every assistant message, tool call, and result on the cc-family streams (the master's own charlie-code turns, every worker turn, the re-attach path) waits one poll wake before the server sees it; a turn with k model round-trips pays up to 150 ms per event, and each completion handoff (worker → reviewer, RESULT → finalize) pays it once; the poll now wakes at 0.02 s — one frame's scale — with the idle round still one fstat; 11-passed backend-stream suite plus 1 new test (the default interval is the discovery bound the M122 line prices) |
| 2026-09-26 | this PR | M123 hook-helper import floor, introduced with this PR: registered hook-command wall median 36.4/34.6/35.8 → 23.5/23.2/23.4 ms (−33% to −36%), maxima 35.2-50.4 → 23.6-24.2 ms, every paired round faster over three interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, arm order alternating, at load 1.37-1.83 one-minute; the registered argv is the launch's own (`claude plugin validate --strict` passes on the written plugin), and the strict-parse, fail-closed rc-2, and parent-group-terminate behaviors are pinned by the 37-passed claude-sub suite, one new siteless transport-failure test, and a new -S-head assertion on the registration test | every Claude Code hook event spawns the helper once, and the gate events (UserPromptSubmit per prompt, PreToolUse per tool call, PermissionRequest on demand) block the turn until the bridge answers — so the helper's import floor rode the turn's critical path per event; the registered command now runs `-S` (the helper imports nothing from site-packages, while site's editable finder drags pathlib/glob/re into every event) and the helper drops argparse (a strict three-flag parse of the contract claude_sub itself registers) and the annotations-only typing import, leaving the floor at interpreter base, the json/re chain, and the transport modules |
| 2026-09-26 | this PR | M103 token-render pages, the digest walk's directory record memoized: GET / median 1130/1115/1103/1134/1118 → 1026/1016/1041/1038/1043 µs (−7.2 %), GET /diff 606/595/593/617/598 → 517/510/532/521/540 µs (−12.9 %), every paired round faster over five interleaved rounds of the verbatim collector — main checkout before vs branch worktree after back-to-back, arm order alternating, at load 1.66-1.85 one-minute; GET /api/git/repos band parity (336.0 vs 329.8 µs, no template render, the mechanism witness); the digest call itself 231.8 → 175.1 µs (−24.5 %) over three interleaved 2000-sample rounds, the digest string identical across all six arms and the M99 server import floor at parity (0.567 vs 0.550 s); 6643-passed suite plus one new test (a new file in an already-memoized directory moves the token on the next render) | the ?v= token's digest walk re-scandir'd every static directory on every page render (227 µs, ~20 % of the 1.1 ms GET /) although the per-render freshness contract only needs the files' own (mtime_ns, size) stats — a content edit moves the file's signature, not its directory's — so the per-directory entry record rides the StatSignatureMemo the sibling request-path caches use and a steady render stats each file and directory once, re-scandir-ing only a directory whose own stat moved; the artifact-page injection also called the token twice per body — two full walks per memo miss, and an edit landing between the calls could ship two different tokens in one page — now one |
| 2026-09-26 | this PR | M18 collector repaired: the standing collector's `workers.js` load reads ENOENT since the worker-card panel's removal landed on main (the frontend consolidation deleted the file; this round's sweep reports M18 unmeasured — the one failed collector of 121). The collector retargets the invariant's subject at its new home — the main chat column's transcript poll (`setWorkerTranscriptMode` over `sidebar/session-view.js`, loaded through `sidebar/namespace.js` in the page's script order) — and reads 0 poll fetches per simulated 10 hidden min (1 bootstrap fetch excluded), 5 fetches in the 10 s visible re-check, identical over the branch-worktree and main-checkout arms (the repair touches no page js), at load 0.15-1.93 one-minute; the invariant and the 0-fetches range are unchanged | the poll the collector was following moved with the worker transcript into the main chat column; a collector pinned to a deleted file measures nothing, so the retarget rides the same file the sweep reads its commands from |
