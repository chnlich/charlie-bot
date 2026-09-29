# Chat Platforms

This document is what a contributor reads before adding a chat platform to CharlieBot. It explains why the thread entrypoint is one shared core plus one thin adapter per platform, what the core owns, what an adapter must supply, how Slack and Discord fill that contract today, and how Telegram would map onto it. It is written from the code in `src/core/thread_entry.py`, `src/core/slack_listener.py`, and `src/core/discord_listener.py`.

---

## Why one core

A summon binds one platform thread to one session: the session is created for (or reattached to) that thread, and every round it runs reads and answers that same thread. That thread behavior is identical on every platform — one session per thread, the follow triggers that wake the session when new thread messages arrive, the read-before-reply gate, reply delivery, the round-end audit, and the lost-summon report after a restart — so it lives once, in `src/core/thread_entry.py`. A platform module supplies only what genuinely differs: its connection (how events reach the server), its event parsing (which events are summons and which are thread traffic), and its API calls (posting, reactions, reading the thread).

The import direction is one way. Platform modules import the core (`src.core.slack_listener` and `src.core.discord_listener` both import `src.core.thread_entry`); the core never imports a platform module. The single seam outside that direction is the round-end hook: `SessionManager.persist_and_broadcast` lazily imports each platform's `deliver_done` wrapper inside the function when a `master_done` event lands — a cycle guard, since both listeners import `SessionManager` at module scope.

---

## The core's flow

---

## The adapter contract

---

## Adding a platform

---

## Telegram mapping
