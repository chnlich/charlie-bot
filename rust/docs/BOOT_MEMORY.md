# Boot recovery memory structures

## What the measurements show

The original same-snapshot API-parity run used 5,608 sessions and reported a Rust high-water RSS of 28,604,800 kB and Python high-water RSS of 15,506,300 kB. Rust reached `server_ready` in 159,358 ms; Python took 115,638 ms. These are whole-process measurements from the supplied run, not per-object measurements.

Two later structure captures show where Rust held parsed history and derived facts. They are different snapshots, so the byte totals are structural evidence rather than a controlled before/after delta.

| Rust structure | Earlier capture, `50be2944` | Shared-fact capture, `b843ace6` | Python owner |
| --- | ---: | ---: | --- |
| Parsed chat events | 328,947 events; 1,041,897,555 B event payload capacity; 38,848,176 B event-vector capacity | 771,425 events; 3,032,241,934 B event payload capacity; included in 64,873,392 B event-cache capacity | `ChatEventStore._events_cache`, a session-to-list map; the loader creates one parsed list and returns it on later reads ([chat_events.py:166-168](../../src/runtime/chat_events.py#L166), [chat_events.py:226-232](../../src/runtime/chat_events.py#L226)) |
| Task facts and event index | 575 nodes; 649,032 indexed events; 2,573,175,465 B estimated capacity | 943 nodes; 640,623 indexed events; 46,235,521 B estimated capacity | `_TaskFacts` keeps derived maps, sets, and event references; `_facts_of` memoizes by the live event-list object and extends its fold on append ([task_sessions.py:87-122](../../src/runtime/task_sessions.py#L87), [task_sessions.py:710-746](../../src/runtime/task_sessions.py#L710)) |
| Node-state memo | 927 entries; 109,073 event IDs; 12,689,156 B estimated capacity | No node-state entry total was emitted by this capture | `_state_tail_memo` stores one state and file signature per session ([task_sessions.py:389-391](../../src/runtime/task_sessions.py#L389)) |
| Run-facts memo | Not reported | 64 entries; 691 outcomes; 3 stop-request runs; 114,840 B estimated facts capacity | `_RunFactsMemo` is bounded at 64 and keys by `id(events)`, validates the retained list by identity, and keeps the list strongly ([runs.py:433-449](../../src/runtime/runs.py#L433)) |

The shared-fact capture reported 5,784 tree nodes, 10,109,228 kB peak RSS, and 99,375 ms to ready. Its event cache, task facts, and run-facts facts together account for 3,143,465,687 estimated bytes of retained capacities. The earlier capture reported 5,823 tree nodes and 13,424,644 kB peak RSS. The task-facts capacity fell from 2,573,175,465 B to 46,235,521 B while indexed event counts were similar; task facts now hold shared event references instead of cloned payloads. Event payload capacity is counted once in the event cache.

The capacity estimates count container slots and owned string/JSON buffers. They are not allocator accounting and omit allocator metadata, fragmentation, stacks, code pages, and other process structures. The Python reports expose process RSS but do not expose per-object byte counts. The Python source references above identify the matching owners and retention rules; no Python object sizes are inferred here.

## Run-facts memo correction

The initial Rust memo used session, archive extent, and file identity as its key and retained facts without retaining the source list. That did not match Python's `id(events)` key or strong list reference. Rust now keys each entry by the cached list's stable identity token, retains the current `Arc<Vec<SharedChatEvent>>`, refreshes that list after an append, and gives a cache clear/reload a new identity. The memo evicts the oldest entry after 64, matching Python's bound. `run_facts_memo_keeps_the_python_sixty_four_list_bound` inserts 65 lists and asserts 64 retained entries and release of the evicted list. `task_facts_share_cached_event_payloads_until_cache_invalidation` checks shared payload identity, append refresh, and replacement-list misses.

The corrected structure run for this key change did not reach boot. Its report is `20261023T131117726464Z-rust-a0c3cde5/report.txt`; config preflight rejected a production `cc-anthropic-compatible` option field named `headers`. The Python 78e0d10e2 option model also forbids unknown fields (`src/infra/backend_models.py:20-24` and `src/backends/anthropic_compatible/options.py:9-14`), so accepting it only in Rust would change config behavior. That run has no ready time, RSS, or current-key structure counts.

## Recovery stop checks and boot work

The recovery pass now checks cancellation while reading each run-directory entry and during event folds. Review facts are built in one cancellable pass over run records instead of rescanning the history for each followup. Count tests assert the stop-check count and the number of records read; `run_record_recovery_walk_checks_each_entry_before_reading_it` stops on the third entry and proves only two records were read. `replay_review_scan_checks_each_run_and_stops_at_the_requested_count` and `replay_review_index_checks_stop_during_the_unique_run_pass` assert checks at each visited record.

The most recent successful standalone boot before the memo-key correction was commit `91762411f135ac99d4c20384f9259954c02afc9c`: 5,811 nodes, 82,546 ms ready, and 10,976,144 kB peak RSS. It does not measure the final memo key or prove the Python comparison. The acceptance measurements must be repeated on the final commit with a home whose config validates against the pinned Python baseline.

## Read-phase census of the candidate `957fe3890`

A measurement-only census commit (`ff0c28e694d457db80181418e311780e89e42c2d`, never landed; jemalloc experiment on top: `73ee4bc812c13e5a78e703da6a7129b8e2186c2b`) logs every structure group's capacity and `VmHWM`/`VmRSS` every 15 seconds, so one api-parity run's server log shows the whole peak.

The api-parity run `20261023T161259708501Z-api-parity-957fe389` (warm page cache) measured `rust_peak_rss_kb=20616620` against `python_peak_rss_kb=15003468`: the memory acceptance fails. The census in the follow-up runs pins the composition.

At the end of boot (recovery done), the process holds 9.0 GB RSS while the census accounts for about 4.4 GB: 812 sessions and 808,950 events in the event cache (4.27 GB payload capacity), 794 node-facts entries (49 MB), 64 run-facts memo entries. Recovery evicts each passed node's parsed events, so most sessions' parsed events are freed.

During the web-UI read phase the event cache regrows to every session: 5,869 sessions, 2,216,548 events, 12.15 GB payload capacity, while RSS tracks it from 9.0 GB to the 20.6-20.9 GB peak. Everything else stays small at peak: node facts 156 MB, run-facts memo at most 64 lists (0.1-2.0 GB as its window slides), node states, metadata and activity entries in the tens of megabytes.

## Retention parity and the representation gap

The retained data matches Python. Python's `_events_cache` is an unbounded read-through map (`chat_events.py:166-168`) and so is the Rust event cache; Python's message-projection memo is an LRU of 64 (`session_events.py:44`) and so is Rust's (`PROJECTION_LRU_LIMIT`); the run-facts memo is 64 on both sides (`runs.py:449`). Neither side evicts during the read phase, so both hold every session's parsed live events at their peak.

What differs is the per-event cost of the parsed representation. A tracemalloc sample of 20 production `chat_events.jsonl` files parses to 3,107 heap bytes per event on Python (raw text 2,040 bytes per event), which puts Python's whole parsed corpus near 7 GB of its 15.0 GB peak. The Rust census reports 5,480 capacity bytes per event as `serde_json::Value`, and the measured RSS gap says the real number is higher: every JSON object is a `BTreeMap<String, Value>` whose keys are fresh `String`s even though event keys repeat millions of times, and every string is a separate allocation. At 2.2 million events that representation gap is the whole 5.6 GB by which Rust exceeds Python.

Swapping the global allocator to jemalloc moved the peak only from 20.6 GB to 20.0 GB (`20261023T181456093331Z-api-parity-73ee4bc8`): allocator retention is not the cause. The same run, on the eviction page-cache policy, measured `rust_ready_in_ms=81175` against `python_ready_in_ms=57542`, so the cold-cache ready-time gate is also unproven for the candidate.

Closing the memory acceptance therefore needs a cheaper parsed-event representation — for example interned object keys, or retaining each event's raw line and parsing on demand — not a retention change: both servers already keep exactly the same data.

## Parsed-event form: interned compact events

A census compared the three candidate in-memory forms for the cached chat events. The census binary parsed 20 production `chat_events.jsonl` files (5,018 events, the seed-7 sample of the same production home the acceptance runs use) in a release build on host3 and reported each form's retained bytes and per-event times:

| form | retained bytes/event | parse µs/event | notes |
| --- | --- | --- | --- |
| 1. `serde_json::Value` (the pre-change form) | 4,153 | 3.89 | 22.68 µs in a debug build |
| 2. raw JSON line, parsed when a reader asks | 1,676 | deferred | the 3.89 µs parse moves to every read; the whole-corpus average is 2,040 bytes/event |
| 3. interned compact events (chosen) | 1,779 | 3.55 | 25.40 µs in a debug build; `to_value` at a response boundary costs 0.96 µs/event (6.02 µs debug) |

Form 3 keeps each event as an enum whose object keys are interned process-wide (the sample's 5,018 events share 550 distinct keys, a 52,775-byte table), whose objects, arrays and strings are exact-fit allocations with no growth slack, and whose numbers keep serde_json's three forms so round trips stay byte-identical.

The choice is form 3. It retains only ~6% more bytes than the raw-line floor while keeping Python's parse-once read semantics; form 2 re-parses on every facts fold and display-state query, and Python's facts structures hold shared event references, so form 2 would either re-parse per query or keep unbounded parsed copies outside the cache. Parse time is lower than the Value form's (3.55 vs 3.89 µs/event) and the convert-back runs only at response and aggregator boundaries, so the request-time gate is paid for by the cheaper parse. Retention, keys, invalidation and bounds are unchanged; only the representation is new. The count tests are `object_keys_are_shared_across_values` (one interned id per distinct key) and `a_chat_event_is_exact_fit_with_no_growth_slack` (capacity equals entry slots plus exact string bytes).

Census source: the measurement-only binary `event_forms.rs` over `fixtures/event_census_sample/`, both never landed; the binary is archived at `/home/user/scripts/20261024_event_form_census/event_forms.rs` and the sample is regenerable from the production home with the archived `make_sample.py` alongside it.
