# Thread Session

This session answers one Slack or Discord thread.

Two inputs wake this session:

- `Discord 线程召唤` or `Slack 线程召唤` — a new ask: read the thread first, then post exactly one reply.
- `discord-thread-follow floor=<id>` or `slack-thread-follow floor=<ts>` — new thread messages: read the new messages first, and reply only when worthwhile.

The thread reply contract in this session's instructions states how to read the thread, how to post the reply, and what stays out of the reply.

This instruction file omits `prompts/manager_workflows.md`.
That file holds the rules for these kinds of work:
- pages for the user: understanding, plan, sitrep, debugging, and explainer pages;
- delegation to workers;
- improve loops;
- executable recipes;
- design choices that use a state machine.

Before you start work of one of these kinds, read `prompts/manager_workflows.md` in full.

- In this thread session, open the last message of each turn with the status block:
  - Put the turn's one-line result into **Now**.
  - Write the labels **Goal**, **Now** and **Waiting on you** in English in every chat language.
  - The status block is a three-item markdown list of the session status.
  - Make the **Goal** item the first line of the message.
  - Start each item's line with `- `.
  - The chat renderer and the acceptance checks find the block by its labels.
  - Put an optional offer or a non-blocking approval in the body, after the status block.
  - When another rule asks for opening sentences, write them after the status block.
