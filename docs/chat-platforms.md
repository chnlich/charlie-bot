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

A platform describes itself to the core with one `ThreadPlatform` value and one `ThreadAdapter` subclass; the core reads everything it needs off the two. The Slack values below are the `SLACK` instance in `src/core/slack_listener.py`, and the Discord values are the `DISCORD` instance in `src/core/discord_listener.py`. `ThreadMessage` is the shape both sides exchange for one thread message: `id` is the platform's message id (a Slack ts, a Discord snowflake), `user` its author (None when the platform names none), and `text` its text (empty when the platform names none).

### `ThreadPlatform` fields

| Field | Meaning | Slack | Discord |
|-------|---------|-------|---------|
| `name` | Summon event-block key and prefix of every persisted marker; the derived keys spell themselves from it | `slack` | `discord` |
| `display_name` | Human-facing platform name in prompts, notices, and logs | `Slack` | `Discord` |
| `reply_event_type` | Wire type of the persisted reply event | `ET.SLACK_REPLY` (`slack_reply`) | `ET.DISCORD_REPLY` (`discord_reply`) |
| `reply_command` | The reply command the prompt contract states and the round-end audit enforces | `charliebot slack reply` | `charliebot discord reply` |
| `max_post_chars` | Per-message limit `chunk_text` splits at | `40000` | `2000` |
| `scope_doc` | The platform's scope doc under prompts/ (what personal information may enter its threads): read fresh into every summon prompt, named by every follow wake | `slack_reply_scope.md` | `discord_reply_scope.md` |
| `follow_trigger_prefix` | Trigger-label prefix identifying the session's armed follow record | `slack-thread-follow` | `discord-thread-follow` |
| `id_key` | Maps one message id to its ordering key | `str` (dotted ts strings sort as strings) | `snowflake_key` (snowflakes sort as integers) |
| `origin_field` | `SessionMetadata` attribute holding the thread origin | `slack_origin` | `discord_origin` |
| `watermark_field` | `SessionMetadata` attribute holding the newest consumed message id | `slack_watermark_ts` | `discord_watermark_id` |
| `id_label` | Key naming a message id in readbacks and refusals (the 412 payload's per-message key) | `ts` | `id` |
| `mention_key` | Summon-block key of the mention message; a block without it carries no ack to clear | `mention_ts` | `mention_id` |
| `block_keys` | Summon-block keys a nudge copies from the summon it re-asks | `channel_id`, `thread_ts`, `mention_ts` | `guild_id`, `channel_id`, `thread_id`, `mention_id` |
| `thread_fallback` | Formatted with the summon block when a summon prompt carries no link | `(channel {channel_id}, thread {thread_ts})` | `(guild {guild_id}, thread {thread_id})` |
| `attaches_files` | True when linked pages are uploaded as attachments instead of published | `False` (publishes and swaps the URLs) | `True` (uploads on the last chunk) |

Three marker keys derive from `name` and so carry no field of their own: `notice_key` and `backfill_key` name the marker payloads the audit predicates and the boot backfill read (the `slack_notice` and `slack_backfill` payloads), and `ack_event_type` names the ack audit record's wire type (`slack_ack`, `discord_ack`).

### `ThreadAdapter` methods

| Method | What the core uses it for | Slack (`SlackThreadAdapter`) | Discord (`DiscordThreadAdapter`) |
|--------|---------------------------|------------------------------|----------------------------------|
| `post(address, text, files)` | `post_with_retry` posts every reply chunk and every notice through it | `SlackClient.post_message` (`chat.postMessage` with `thread_ts`); takes no files | `DiscordClient.create_message` on the thread; the files ride as attachments |
| `add_ack(block)` | Lights the summon ack when the summon round fires | `reactions.add` of `eyes` (`_ACCEPTANCE_REACTION`) on the mention ts | `add_reaction` of `👀` (`_ACCEPTANCE_EMOJI`) on the mention id |
| `remove_ack(block)` | Closes the summon ack when a reply, notice, or lost-summon report answers it | `reactions.remove`; a `no_reaction` error is the end state, so the clear is idempotent | `remove_own_reaction` of `👀` |
| `read_eligible(origin, cfg)` | The eligible thread read behind the freshness gate, the ack check, and the reconnect backfill | one `conversations.replies` call; keeps plain (subtype-absent) human messages from allowed users | `get_messages` paged oldest-first, 100 per call from id `0` until a short page; keeps human-authored, non-webhook messages of type 0 or 19 from allowed users |
| `address_of(origin)` | Builds the address dict `post` accepts | `{"channel_id", "thread_ts"}` from the `SlackOrigin` | `{"guild_id", "thread_id"}` from the `DiscordOrigin` |
| `link_swap(cfg)` | The rewrite swap `rewrite_file_links` applies to the reply's file links, plus the list of files to attach | `_publish_swap`: publishes each linked page and returns its published URL; appends to nothing | keeps the file's bare name for the URL and collects each distinct path once |
| `log_fields(address)` | Log fields naming the thread on every core log line | `channel`, `thread_ts` | `guild`, `thread` |
| `thread_link(origin)` | The permalink the follow wake label names | `chat.getPermalink` | `message_link` built from the guild and thread ids (no API call) |
| `follow_wake_message(floor, link)` | The armed follow trigger's label | `_build_follow_wake_message`: the prefix, `floor=<ts>`, the permalink, the slack-skill read instruction, the scope, red-line, and reply-format doc re-reads, and the ack and reply commands | `_build_follow_wake_message`: the prefix, `floor=<id>`, the message link, the server-side read command first, then the scope, red-line, and reply-format doc re-reads, then the reply commands |

### What a platform module owns outside the adapter

The adapter is not the whole platform module. Each platform also owns:

- **The connection loop.** Slack: `run_listener` in `src/core/slack_listener.py` — a Socket Mode websocket whose backoff doubles from 1 s to 30 s. Discord: `run_listener` in `src/core/discord_listener.py` — a gateway websocket with identify and heartbeat, the stop close codes in `_STOP_CLOSE_CODES` (retrying cannot fix them), and a `_preflight` that refuses to start without the Message Content intent.
- **Event parsing and drop rules.** Slack: `handle_app_mention` (summons: type `app_mention`, sender on the allowed list) and `handle_thread_message` (follows: drop subtypes, drop non-thread messages, drop bots and non-allowed senders before the core's session guards run). Discord: `handle_message_create` — one guard chain over human sender, message type, allowed author, the DM notice (`_DM_NOTICE`, a DM binds no guild thread), mention versus follow traffic, and channel type (a mention already in a thread binds that thread; a channel mention starts a thread named from the stripped mention content, `_thread_name`).
- **The session-id namespace.** Slack: `SLACK_NS` and `summon_session_id(team_id, channel_id, thread_ts)` in `src/core/slack_listener.py`; Discord: `DISCORD_NS` and `summon_session_id(guild_id, thread_id)` in `src/core/discord_listener.py`. Both derive a stable uuid5 from the thread's coordinates.
- **The session label.** Slack: `Slack #<channel name>`, resolved once in `handle_app_mention` (channel id when the name lookup fails); Discord: `Discord #<parent channel name>`, resolved in `handle_message_create`.
- **The origin model and its metadata fields.** `SlackOrigin` (`team_id`, `channel_id`, `thread_ts`) and `DiscordOrigin` (`guild_id`, `parent_channel_id`, `thread_id`) in `src/core/models.py`; each platform's `SessionMetadata` and `CreateSessionRequest` fields are its `origin_field` and `watermark_field` (`slack_origin` / `slack_watermark_ts`, `discord_origin` / `discord_watermark_id`).
- **The summon block keys.** The platform decides them (`block_keys` in the table above): the Slack block is built in `handle_app_mention`, the Discord block in `handle_message_create`.
- **The summon prompt and the platform line.** Slack: `_build_summon_prompt` and `_PLATFORM_LINE` in `src/core/slack_listener.py` (the permalink plus the slack-skill read hint); Discord: the same two names in `src/core/discord_listener.py` (the message link plus the server-side read command). Both end at the shared tail `summon_prompt_tail`: the platform's scope doc (`slack_reply_scope.md`, `discord_reply_scope.md`, the rules on what personal information may enter that platform's threads), the PII red line, and the reply-format contract, each read fresh from prompts/ per summon.
- **The follow wake label.** Slack: `_build_follow_wake_message` in `src/core/slack_listener.py`; Discord: `_build_follow_wake_message` in `src/core/discord_listener.py`. Both labels order the session to re-read the platform's scope doc plus the shared red line (`prompts/thread_reply_redline.md`) and the reply format (`prompts/thread_reply_format.md`) before any reply.
- **The server start and stop.** `server.py` starts each platform's listener task and its boot-backfill task in the lifespan when the platform's credentials and allowed users are set, and cancels both on shutdown.
- **The round-end hook.** `SessionManager.persist_and_broadcast` in `src/core/sessions.py` fires each platform's `deliver_done` as its own logged task on every `master_done` event.
- **The CLI and the internal endpoints.** Slack: `src/cli/slack.py` (`charliebot slack reply`, `charliebot slack ack`) over `POST /api/internal/slack/reply` and `POST /api/internal/slack/ack`; Discord: `src/cli/discord.py` (`charliebot discord reply`, `charliebot discord read`, `charliebot discord check`) over the matching endpoints in `src/api/internal.py`.
- **The reply event type and its session-view row.** The constants `ET.SLACK_REPLY` and `ET.DISCORD_REPLY` live in `src/core/event_types.py`; `src/core/message_aggregator.py` maps each to the system row the session view renders ("Posted to Slack: ..." / "Posted to Discord: ...").

---

## Adding a platform

A new platform repeats the shape Slack and Discord already fill. Work down this checklist in order; each step instantiates one item of the adapter contract above.

1. Describe the platform with one `ThreadPlatform` value — every field of the first table — built from the module's own constants, so each value keeps one home.
2. Subclass `ThreadAdapter` over the platform's client, implementing every method of the second table against the same platform value.
3. Add the origin model to `src/core/models.py`: the origin type, the `origin_field` on `SessionMetadata` and `CreateSessionRequest`, and the `watermark_field` on `SessionMetadata`.
4. Write the event parsing and drop rules: the summon handler and the follow handler, with the eligibility rule shared between the follow guard and the adapter's readback (gate eligibility equals read eligibility, so nothing is demanded of an ack the session would never consume).
5. Write the connection loop (`run_listener`): connect, reconnect with backoff, and run `backfill_followed_threads` on every (re)connection.
6. Pick the session-id namespace and `summon_session_id`, and resolve the session label once per accepted summon.
7. Write the summon prompt and its platform line (reusing `summon_prompt_tail` unchanged), the summon block keys, and the follow wake label.
8. Wire the server start and stop in `server.py`: the listener task plus the boot-backfill task, behind the platform's credentials and allowed users.
9. Hook the round end: a `deliver_done` task in `SessionManager.persist_and_broadcast`.
10. Add the CLI verbs and the internal endpoints for reply and ack (or the platform's read equivalent, as Discord's `read` is).
11. Add the reply event type constant in `src/core/event_types.py` and its session-view row in `src/core/message_aggregator.py`.
12. Add the tests. The shared core is already covered platform-neutrally by the synthetic platform in `tests/core/test_thread_entry.py` ("fakechat", an integer id sort that is not the string sort); the platform's own tests mirror `tests/core/test_discord_listener.py`.

---

## Telegram mapping

This section is design only: no Telegram code exists in the repository. Every point below states what the Bot API documentation (https://core.telegram.org/bots/api) documents today; re-check each against it before building.

- **Thread.** A forum topic in a supergroup with topics enabled (`message_thread_id`), or a reply chain in a plain group (its root message id). The session id derives from the chat id plus the thread id, in the pattern of the two `summon_session_id` functions above.
- **Message ids.** Integers unique per chat, so `id_key=int` — the Discord row's ordering, not Slack's string sort.
- **Summon.** A `mention` entity naming the bot's username, or a reply to one of the bot's messages.
- **Ack.** `setMessageReaction` with 👀 (Bot API 7.0 and later) — the same lit-at-summon, cleared-when-answered contract both adapters implement.
- **Post.** `sendMessage` with `message_thread_id`; 4096 characters per message sets `max_post_chars` for `chunk_text`.
- **Attachments.** `sendDocument` (50 MB per file, one file per call; `sendMediaGroup` groups 2 to 10) — an `attaches_files` platform like Discord.
- **Reading the thread.** The Bot API has no history read, so `read_eligible` needs a local store of the messages the bot received, kept per followed thread. This is the main divergence from Slack and Discord, whose adapters read the platform's history API.
- **Follow visibility.** With privacy mode on (the default), a bot in a group receives only messages that mention it, reply to it, or are commands (https://core.telegram.org/bots/features#privacy-mode). Following a thread needs privacy mode off or the bot as a group admin — the analog of Discord's Message Content intent, which `_preflight` refuses to start without.
- **Connection.** `getUpdates` long polling or a webhook, instead of the websocket both current listeners hold open.
