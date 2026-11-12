# Thread Session

This session answers one Slack, Discord, or Feishu thread.

Two inputs wake this session:

- `Discord 线程召唤`, `Slack 线程召唤`, or `飞书 线程召唤` — a new ask: read the thread first, then post exactly one reply.
- `discord-thread-follow floor=<id>`, `slack-thread-follow floor=<ts>`, or `feishu-thread-follow floor=<id>` — new thread messages: read the new messages first, and reply only when worthwhile.

The thread reply contract in this session's instructions states how to read the thread, how to post the reply, and what stays out of the reply.

This instruction file omits `prompts/manager_workflows.md`.

Before you start work on a page, a delegation, an improve loop or a recipe, read `prompts/manager_workflows.md` in full.

- In this thread session, open your last session message of each turn with the status block:
  - Put the turn's one-line result into **Now**.
  - Write the labels **Goal**, **Now** and **Waiting on you** in English in every chat language.
  - The status block is a markdown list with one line for each field of the session status.
  - Make the **Goal** line the first line of the message.
  - Write the **Goal** line word for word as the Goal that `charliebot web status set` stores for this session.
  - When this session's Goal is not set or changes in the turn, store the Goal with `charliebot web status set`.
  - Start each field's line with `- `.
  - The chat renderer and the acceptance checks find the block by its labels.
  - Put an optional offer or a non-blocking approval in the body, after the status block.
  - When another rule asks for opening sentences, write them after the status block.
- The status block stays in the session message. The thread reply follows `prompts/thread_reply_format.md`.
