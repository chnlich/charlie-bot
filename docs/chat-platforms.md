# Chat Platforms

This document is what a contributor reads before adding a chat platform to CharlieBot. It explains why the thread entrypoint is one shared core plus one thin adapter per platform, what the core owns, what an adapter must supply, how Slack and Discord fill that contract today, and how Telegram would map onto it. It is written from the code in `src/core/thread_entry.py`, `src/core/slack_listener.py`, and `src/core/discord_listener.py`.

---

## Why one core

A summon binds one platform thread to one session: the session is created for (or reattached to) that thread, and every round it runs reads and answers that same thread. That thread behavior is identical on every platform — one session per thread, the follow triggers that wake the session when new thread messages arrive, the read-before-reply gate, reply delivery, the round-end audit, and the lost-summon report after a restart — so it lives once, in `src/core/thread_entry.py`. A platform module supplies only what genuinely differs: its connection (how events reach the server), its event parsing (which events are summons and which are thread traffic), and its API calls (posting, reactions, reading the thread).

The import direction is one way. Platform modules import the core (`src.core.slack_listener` and `src.core.discord_listener` both import `src.core.thread_entry`); the core never imports a platform module. The single seam outside that direction is the round-end hook: `SessionManager.persist_and_broadcast` lazily imports each platform's `deliver_done` wrapper inside the function when a `master_done` event lands — a cycle guard, since both listeners import `SessionManager` at module scope.

---

## The core's flow

The core owns the whole life of a thread-bound session. Each step below names the core function that implements it; the platform side only feeds events in and carries posts out through its adapter.

- **Summon** — `accept_summon` resolves the session (create, unarchive, or reuse), advances the watermark past the mention and cancels armed follow triggers (`consume_mention`), groups the session under its label (`ensure_group`), persists the summon event, and fires the round and the ack eye as logged tasks.
- **Follow** — `follow_message` checks the session-side guards (session exists, is ACTIVE, its origin matches the event's channel, the message id sorts above the watermark) and calls `arm_follow_trigger`, which cancel-then-creates the session's one persisted follow trigger: a 45-second quiet delay from the newest message, capped 300 seconds from the chain's first message, with the chain floor parsed back off the replaced label so a steady trickle still flushes. When the trigger fires, the wake reaches the session as a scheduled trigger labeled by the adapter's `follow_wake_message`; the wake enters the session log with no platform block, so silence stays a legal round outcome outside the audit.
- **Read before reply** — `assert_thread_fresh` refuses the reply with 412 while eligible thread messages sit above the session's watermark; `ack_messages` advances the watermark over the given ids (every eligible unread id at or below the newest must be included) and persists a small ack event for the audit trail.
- **Reply** — `post_reply` rewrites the reply's file links (`rewrite_file_links` through the adapter's swap), splits the text at the platform's per-message limit (`chunk_text`), posts chunk by chunk through `post_with_retry` (two retries, 1 s then 4 s, before a 502), persists the platform's reply event naming the summon it answers, and clears the answered summon's ack.
- **Round end** — `deliver_done` runs as a fire-and-forget task on every `master_done`; for a round that answered a summon or a nudge it hands over to `audit_round`, which nudges a summon round without a reply and posts the no-reply notice to a nudged round without one, persisting a marker each time so both fire once per summon.
- **Boot** — `backfill_lost_summons` runs once per boot after crash recovery: it reports every summon that nothing will ever answer (queued when the process died, no done, no marker) and re-runs the round-end audit over every finished round, closing the crash windows between a done and its nudge and between a nudge round's done and its notice.
- **Reconnect** — `backfill_followed_threads` runs on every (re)connection and arms the follow trigger of every ACTIVE thread-bound session whose thread holds unread messages, covering the events no live connection delivered.

---

## The adapter contract

---

## Adding a platform

---

## Telegram mapping
