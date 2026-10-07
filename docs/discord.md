# Discord entrypoint

The Discord entrypoint lets an allowed user summon CharlieBot from a Discord server and keep talking to it inside one thread. This guide is the operator path: what the entrypoint does, how to set the application up once, how to verify it, and how to operate it afterwards. The Discord half lives in `src/features/discord/discord_listener.py`, `src/features/discord/discord_commands.py`, and `src/features/discord/discord_client.py`; the platform-neutral half it shares with Slack lives in `src/features/chat_threads/thread_entry.py`.

## What it does and why

The point of the entrypoint is to run a CharlieBot session from where the conversation already is: an allowed user mentions the bot in a server channel, thread, or forum post, and a session bound to that thread answers in place. The shared thread core keeps one session bound to one thread across mentions, follow-up messages, and server restarts, so a thread behaves like one long conversation.

The flow:

- An allowed user mentions the bot in a server channel, thread, or forum post. The gateway listener (`run_listener` in `src/features/discord/discord_listener.py`) receives the `MESSAGE_CREATE` event and applies Discord's drop rules first: non-human senders (a `bot` author flag or a `webhook_id`), message types that are neither plain messages nor replies (type 0 or 19), and authors whose id is not in the account map never reach the summon path.
- In a text or announcement channel the bot opens a thread from the mention message, named from the mention's stripped content; in a thread or forum post it uses the thread the mention already sits in. One session is bound per thread — the session id is derived from the guild id and the thread id, so mentioning the bot in the same thread again always reaches the same session (created once, unarchived, or reused).
- The bot lights the 👀 reaction on the mention message and starts a master round. The master reads the thread server-side with `charliebot discord read` (the readback returns the messages the bot can see and marks the unread ones read) and answers through `charliebot discord reply`, which posts the reply into the thread.
- Later unmentioned messages from allowed users in that thread wake the same session: the wake fires about 45 seconds after the last message of a batch, and 300 seconds at most after the first message of the chain, so a steady trickle still flushes.
- Linked pages reach readers as published links, published by the round before it replies: it runs `charliebot publish <page>` (the path Slack replies use) and writes the printed URL into the reply text, because thread readers may not reach this server. Each published copy sits under a fresh unguessable directory, so anyone holding the link opens the rendered page in the browser, and nobody can guess a link from the page name. The reply path posts the text as written; a reply that still contains a CharlieBot file-server link (the password-protected file browser) is refused with a 422 naming the link and the publish command that fixes it.
- A round that ends without a reply gets one nudge; if the nudge round also posts nothing, the thread gets a one-line notice pointing to the session log.
- A summon still queued when the server restarts gets a notice in its thread after the restart: the boot backfill reports it as lost and tells the thread to mention the bot again.
- A mention in a DM cannot bind a guild thread. An allowed user's DM mention gets a one-line redirect to a server channel and nothing else — no session is touched.

The 👀 reaction tracks the open question: lit at the summon, cleared when a reply answering it lands, or when the notice or the lost-summon report closes the question.

## Setup checklist

The checklist below is the whole Discord-side setup; run it once per application, in order.

1. **Create the application and the bot user.** In the [Discord Developer Portal](https://discord.com/developers/applications), create an application and add a bot user to it (the Bot page). The application id from its OAuth2 page is the `client_id` the invite URL below needs.
2. **Give the server the bot token.** Copy the bot token (Bot page, Reset Token) into `credentials.yaml` under a `discord:` section as `bot_token`. The server and every Discord call read `discord.bot_token` from that file; the token never reaches the CLI or a log line.
3. **Enable the Message Content intent.** On the application's Bot page under Privileged Gateway Intents, turn on Message Content Intent. Without it every message arrives over the gateway with its text stripped, so there is nothing to listen with: the listener logs `discord_listener_message_content_intent_off` and does not start.
4. **Invite the bot to the server.** Open the invite URL with the application's id filled in:

   ```
   https://discord.com/oauth2/authorize?client_id=<application id>&scope=bot&permissions=309237713984
   ```

   The `permissions` integer is the sum of `REQUIRED_PERMISSIONS` in `src/features/discord/discord_client.py` — the six permission bits the entrypoint exercises:

   | Permission | Why the entrypoint needs it |
   |---|---|
   | `VIEW_CHANNEL` | See the channels, threads, and forum posts it is mentioned in: without it their gateway events never arrive and reads there are refused. |
   | `SEND_MESSAGES` | Post messages in the server: every outbound line — a reply, a notice, the DM redirect — goes out as a message post. |
   | `SEND_MESSAGES_IN_THREADS` | Post inside threads: the reply and the notices land in the session's thread. |
   | `CREATE_PUBLIC_THREADS` | Start a thread from the mention message when the mention lands in a text or announcement channel. |
   | `READ_MESSAGE_HISTORY` | Read a thread's past messages: the server-side read, the round's readback, and the reply's freshness gate all page through history. |
   | `ADD_REACTIONS` | Light and clear the 👀 reaction on the mention message. |

5. **Map each allowed account to its person.** Under `discord.allowed_users` in `config.yaml`, list one entry per Discord user id allowed to summon the bot, mapped to the person that account belongs to:

   ```yaml
   discord:
     allowed_users:
       "<user id one>": person-one
       "<user id two>": person-two
   ```

   With Developer Mode on (User Settings, Advanced), right-click a user and Copy User ID. The map is who the bot knows people by: every readback message names its author's entry as `person`, and the thread reply rules (`prompts/discord_reply_scope.md`) judge personal information by that name, so a changed display name cannot impersonate anyone. A message from an id outside the map is dropped before it can summon anything, and an empty map starts nothing: the entrypoint only starts when the token and at least one mapped account are set.
6. **Restart the server.** Startup logs `discord_entrypoint_started` and connects the gateway listener; with the token or the account map missing it logs `discord_entrypoint_off` instead and runs without Discord.

## Verify

Verification has two layers: the setup check reads what the Discord application grants today, and the server log shows the listener actually running.

`charliebot discord check` (from a host that reaches the server) posts to the internal check endpoint and prints one JSON readback:

```json
{"ok": true, "bot_user": {"id": "...", "username": "..."}, "application_id": "...", "message_content_intent": true, "guilds": [{"id": "...", "name": "...", "missing_permissions": []}]}
```

- `ok` is true when the Message Content intent is on and no guild is missing a required permission; the command exits 0 in that case and 1 otherwise (after printing, so the readback is always visible).
- `bot_user` names the bot the token belongs to, `application_id` the application the invite URL needs, `message_content_intent` the intent state read from the application's flags.
- `guilds` lists every guild the bot is in with the `REQUIRED_PERMISSIONS` names it lacks there (`missing_permissions`); an empty list means that guild is fully granted.
- With `discord.bot_token` unset the check refuses with a 409; a failed Discord call is a 502 (an HTTP 401 underneath means the token is invalid).

The server log completes the picture:

- `discord_entrypoint_started` — startup found the token and a non-empty account map and launched the listener.
- `discord_listener_connected` — the gateway accepted the IDENTIFY and answered READY; the line appears once per (re)connection.
- `discord_entrypoint_off` — startup found no token or an empty account map; the server runs without Discord.
- `discord_listener_stopped` with the close code and its reason — the gateway closed with a code retrying cannot fix (for example `4004` authentication failed, or `4014` disallowed intents, which points back to the Message Content intent). The listener exits; fix the named cause and restart.
- `discord_listener_missing_permissions` per guild — the listener's preflight found a guild missing required permissions; the listener keeps running so the granted guilds still work.

## End-to-end drill

Run this drill after every new deployment. The setup check reads static state from the Discord API, so it cannot prove the live path: only a real mention exercises the gateway intent, the channel permissions, and the round together.

1. In a server channel, send a message that mentions the bot. Within seconds a thread opens from that message, the 👀 reaction appears on it, and a session named `Discord #<channel> <time>` shows up in the web UI under that group. The master round starts; when it answers, `charliebot discord reply` posts the reply into the thread and the 👀 clears.
2. In that thread, send a message that does not mention the bot. The same session wakes about 45 seconds later: it reads the thread (the read marks the new messages read), and answers when there is something worth saying. This is the follow wake — the same session, not a new one.

A deployment that passes step 1 but not step 2 is missing a thread permission; a deployment that passes neither usually has the Message Content intent off (the log line `discord_listener_message_content_intent_off` names it) or the bot cannot see the channel.

## Operations

Day-to-day operation runs through the two session-bound verbs the master itself uses; both resolve the session the way every session-bound CLI verb does, so they run from inside a Discord-summoned session without arguments.

`charliebot discord read` reads a thread server-side through the bot:

- Without `--url` it reads the session's own thread, oldest first (the thread's starter message rides first when one exists in the parent channel). The window is `--limit` messages (1 to 100, default 50) starting at the oldest unread one, or the newest `--limit` when nothing is unread. The unread messages the readback returns are marked read — Discord has no separate ack verb, so the read is the ack — and the readback carries `watermark_id` (after the ack) and `more_unread` (unread messages left outside the window; repeat the read until it reads 0).
- With `--url <discord.com channel link>` it reads that channel's newest `--limit` messages instead, all reported with `unread: false`, and marks nothing. A url that is not a discord.com link refuses with a 422; a channel the bot cannot see with a 404; any other Discord refusal with a 502.
- Each message in the `messages` readback carries `id`, `author_id`, `author`, `person`, `timestamp`, `content`, `attachments`, and `unread`. `person` is the name `discord.allowed_users` gives the author's id, and is null when the author's id is not in the map; the thread reply rules judge who is asking by it.

`charliebot discord reply --file <path>` (`-` reads the reply text from stdin) posts the reply into the session's thread. Two refusals matter in operation:

- **Stale thread (412).** The reply refuses with a `stale_thread` payload while eligible thread messages sit above the session's watermark — each unread message is named with its id, author, and a text preview. Run `charliebot discord read` to mark the new messages read, then reply again. Nothing posts while the refusal stands.
- **File-server link refused (422).** The reply posts its text as written, and the text must not contain a CharlieBot file-server link — the `/absolute_filepath/` file browser sits behind the access key, so a thread reader who clicks it lands on the password page. When one is present the whole reply refuses with a 422 naming the link, and nothing posts. Run the `charliebot publish <path>` command the detail gives and write the URL it prints into the reply in place of the file-server link, then reply again.
