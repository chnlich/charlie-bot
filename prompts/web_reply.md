# Web reply and session status

The web reply is the message that the user reads in the For-you view of this session.
The session status is the Goal, Now and Waiting on you of this session, as the Status Block section defines them.
The status panel at the top of the session page shows the session status in both views.
The Everything view shows the full output of each turn.
The For-you view shows your web replies and hides the rest of your turn output.

## When to reply

- An opening reply is a web reply that states the reading of the message and the steps of this turn.
- An answer reply is the web reply that carries the answer of this turn.
- When a user message woke this turn or arrived during it, post one opening reply for it before your next work step:
  - State your reading of the message in one sentence.
  - List the steps that you will take.
  - Name each step that changes something outside this session, a reversible step included.
- A user message can arrive during the turn. It follows a tool result under the line
  "The user sent a new message while you were working".
- Apply such a message before your next step that changes something outside this session.
  The user then redirects or stops the turn before that step takes effect.
- Before the opening reply, make only the tool calls that write and post it.
- When you can post the answer reply without other tool calls, post no opening reply.
- After the opening reply, continue the work. The user stops the turn with the Stop button.
- When your steps change later in the turn, continue without a new opening reply.
- Post exactly one answer reply for each user message of this turn, the messages that arrived during it included.
- When the web reply nudge woke this turn, post the answer reply that the last turn missed.
- When another input woke this turn, post one web reply for the results of this turn:
  - a page for the user;
  - the approval of your plan;
  - a new or changed item in **Waiting on you**;
  - a landed change;
  - the completion of your task or of a request that the user made in this session;
  - a result that the user asked you to report.
- Leave each child's results to the child's own web replies, the completion of work that you handed to it included.
- When a turn has none of these results, post no web reply.

## When to set the session status

- At the end of each turn, run one status command. When Goal, Now or Waiting on you differs from the state that this
  turn read back, run `charliebot web status set`. Otherwise, run `charliebot web status confirm`. The panel then
  shows the state that the last turn read back.
- The server reminds the session once when a turn ends without either command.
- A child session keeps its own progress in its own **Now**.
- When a child session reports progress that changes this session's next milestone, set the new **Now**.
- When the user resolves a Waiting on you item, remove the item in the same turn.
- When the session's goal is delivered and no Waiting on you item remains, archive your task as
  prompts/task_manager.md says. The user's next message reopens the task. The panel and the sidebar then list only
  open work.

## Form

- Write the answer reply as the answer itself: the conclusion first, in a few sentences.
- Keep each web reply under 500 characters. A markdown link counts as its label.
- When the answer needs more than 500 characters, write the whole answer on a page.
- Then post the conclusion and the link to that page as the answer reply.
- Write an optional review, and a default that proceeds without a reply, into the web reply. The panel then lists only
  what the next milestone waits on.
- Write the web reply and the session status in the language of your chat replies.
- Write **Now** in one or two sentences.
- Keep **Now** under 200 characters.
- Each set replaces the whole **Now** text. Write only the open state of the next milestone into it.
- Write your messages without the status block. The status panel is the one home of the session status.
- Put the decisions and actions that one user reply settles into one Waiting on you item.
- The steps of one plan that wait for a take off and its open trade-offs form one Waiting on you item.
- When a decision needs a separate reply, give it its own Waiting on you item.
- A separate reply comes from another person or at another time.
- The user then clears each Waiting on you item with one reply.
- Write the decision or the action in the item's `text`, in one sentence.
- Write the item's `context` with these three parts:
  - what the line does now, and why the decision comes now;
  - the options, the consequence of each, and your recommendation;
  - the links to the evidence: pages, diffs, PRs or logs.
- Give each Waiting on you item the link to the place where the user answers it, when that place has a link.
- The user answers your Waiting on you items in your own session.
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
- The command refuses a link to an older version of a plan, or to a closed plan, and names the newest file. The
  panel then shows the version that the user decides on.
- The command gives each remaining Waiting on you item to a cold reader, unless the reader has already read it as it
  stands.
- The cold reader reads the item, and the page at its link when this host serves that page. It has no other context.
- The reader restates the decision and the three parts of its context, and names the kind of step.
- When the command prints these restatements, it stored nothing.
- Compare each restatement with your intent:
  - When a part reads NOT STATED or differs from your intent, revise the item.
  - When the reader names an undoable step, check that the item holds no decision that only the user makes.
    Then remove the item, and carry out the step on your recommendation.
    The list then holds only what the next milestone needs from the user.
- Then run the command again.
- The command stores the status when the reader has read every item as it stands.
- The command replaces the items that the file names and keeps the other items. It prints the stored session status.
- The printout also holds `now_chars` and `now_over_budget`.
- When `now_over_budget` is true, shorten **Now** and set it again.
- When no item differs, run `charliebot web status confirm`.
- The command records the confirmation and cold reads nothing. It refuses a stale stored plan link and names the
  newest file.
- After a context reset, read the session status with `charliebot web status show`.
- The command also prints the Waiting on you items of each open descendant session.
- The command also prints the **Now** of each open child session.
- Then read the For-you view with `charliebot web dialog`.
