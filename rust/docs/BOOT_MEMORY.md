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
