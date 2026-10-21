# Web reply and session status

The web reply is the message that the user reads in the For-you view of this session.
The session status is the Goal, Now and Waiting on you of this session, as the Status Block section defines them.
The status panel at the top of the session page shows the session status in both views.
The Everything view shows the full output of each turn.
The For-you view shows your web replies and hides the rest of your turn output.

## When to reply

- When a user message woke this turn, post exactly one web reply.
- When the web reply nudge woke this turn, post the web reply that the last turn missed.
- When another input woke this turn, post a web reply only for one of these results:
  - a new or changed item in **Waiting on you**;
  - the completion of a request that the user made in this session;
  - a result that the user asked you to report.
- When a turn has none of these results, post no web reply.

## When to set the session status

- Before the turn ends, compare the session status with the state that this turn read back.
- When an item differs, set the new value before the turn ends.
- When no item differs, leave the session status unchanged.
- A child session keeps its own progress in its own **Now**.
- When a child session reports progress that changes this session's next milestone, set the new **Now**.
- When the user resolves a Waiting on you item, remove the item in the same turn.

## Form

- Write the web reply as the answer itself: the conclusion first, in a few sentences.
- Keep the web reply under 500 characters.
- When the answer needs more than 500 characters, write the whole answer on a page.
- Then post the conclusion and the link to that page as the web reply.
- Write the web reply and the session status in the language of your chat replies.
- Write **Now** in one or two sentences.
- Keep **Now** under 200 characters.
- Each set replaces the whole **Now** text. Write only the open state of the next milestone into it.
- Write your messages without the status block. The status panel is the one home of the session status.
- Put each decision or action that you ask of the user into a Waiting on you item.
- Write the decision or the action in the item's `text`, in one sentence.
- Write the item's `context` with these three parts:
  - what the line does now, and why the decision comes now;
  - the options, the consequence of each, and your recommendation;
  - the links to the evidence: pages, diffs, PRs or logs.
- Give each Waiting on you item the link to the place where the user answers it, when that place has a link.
- List each Waiting on you item in the session where the user answers it.
- When your approver is your parent node, keep Waiting on you empty: the user answers through your parent.
- The status panel shows the Waiting on you items of your open descendant sessions below your items.
- The status panel shows the **Now** of each open child session in a folded block below your items.

## Delivery

- Write the web reply text to a file.
- Then run `charliebot web reply --file <path>`.
- The command records the web reply in the session. It prints a readback with the character count.
- The readback sets `over_budget` for a reply over 500 characters.
- Write the changed session status items to a JSON file.
- The JSON object takes any of the keys `goal`, `now` and `waiting`.
- `waiting` is a list of objects with the keys `text`, `context` and `link`. Set `link` to null when the place has no link.
- Then run `charliebot web status set --file <path>`.
- The command gives each Waiting on you item to a cold reader, unless the reader has already read it as it stands.
- The cold reader reads the item, and the page at its link when this host serves that page. It has no other context.
- The reader restates the decision and the three parts of its context, and names the kind of step.
- When the command prints these restatements, it stored nothing.
- Compare each restatement with your intent:
  - When a part reads NOT STATED or differs from your intent, revise the item.
  - When the reader names a step that you can undo, check that the step is no consequence step.
    Then remove the item, and carry out the step on your recommendation.
- Then run the command again.
- The command stores the status when the reader has read every item as it stands.
- The command replaces the items that the file names and keeps the other items. It prints the stored session status.
- The printout also holds `now_chars` and `now_over_budget`.
- When `now_over_budget` is true, shorten **Now** and set it again.
- After a context reset, read the session status with `charliebot web status show`.
- The command also prints the **Now** of each open child session.
- Then read the For-you view with `charliebot web dialog`.
