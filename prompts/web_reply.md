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
- When another input woke this turn, post one web reply only when this turn did an action from this list:
  - wrote a page for the user;
  - approved your plan, or received the approval of your plan;
  - added or changed an item in **Waiting on you**;
  - landed a change;
  - completed your task;
  - completed a request that a user message in this session made.
- When this turn did no action from the list, end the turn without a web reply.
  The For-you view then holds only results that the user reads or acts on.
- Before you post, name in your reasoning the action from the list that the reply reports.
  This check keeps progress reports out of the For-you view.
- When a child reports progress, a correction or a retraction, handle it with the **Now** rule of the next section.
  The status panel then carries the progress of each line.
- When you correct a web reply of a turn that another input woke, record the correction in **Now**.
  The For-you view then shows one reply for each such turn.
- Leave each child's results to the child's own web replies, the completion of work that you handed to it included.

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
- Write in `context` what the line does now and why the decision comes now, in one or two sentences.
  The panel then shows why the item waits, before the user opens the page.
- Write on one HTML page the options, the consequence of each, your recommendation and the evidence links.
- Set the item's `link` to that page.
  The user then reads the options with their layout, and comments on the page reach this session.
- When the item decides on a plan page or an understanding page, use that page.
  That page already holds the options, the recommendation and the evidence.
- For any other item, write the page body to a fragment file.
- Then run `charliebot artifact wrap --genre waiting --output artifacts/waiting_<slug>.html <fragment>`.
  The command adds the head and style of the decision-page template.
- On that page, show the options as one `div.fork` block.
  The user then reads every decision in one layout.
- Before the status set, pass the page through the gate for waiting pages (`file-server` skill, Cold-Read Gate).
  The page then reaches the user checked, with one cold read for its item.
- When the user acts on another page, such as a login page, link that page from the item's page.
  The item's page then holds the context, and the user reaches the action from it.
- The user answers your Waiting on you items in your own session.
- The status panel shows the Waiting on you items of your open descendant sessions below your items.
- The status panel shows the **Now** of each open child session in a folded block below your items.

## Remaining

Remaining lists the milestones between the session and its Goal, so the user reads the distance to the Goal without asking.

- When the user asks you to start Remaining, directly or through another session, keep it.
  Without such a request, the status holds Goal, Now and Waiting on you.
  The field then stays on the lines that the user chose.
- Write each milestone of the Goal sentence as one item, and mark each completed milestone `done`.
  The panel then counts the milestones that remain.
- In each open item, write its state and what it waits on: a job, worker, child session or user.
  The user then reads what each open milestone waits on.
- Date an open item only from a basis: a user deadline, a scheduled run or a dated prerequisite.
- When an open item has no such basis, write in the item that it has no date.
  The user then reads each date as a fact.
- In the turn that completes a milestone or changes the list, set `remaining` again.
  The panel shows the time of that set, so the user sees how current the list is.

## Delivery

- Write the web reply text to a file.
- Before you run the command, count the characters of the file.
  The reply then fits the budget on its first post.
- Then run `charliebot web reply --file <path>`.
- The command records the web reply in the session. It prints a readback with the character count.
- The readback sets `over_budget` for a reply over 500 characters.
- When the readback sets `over_budget`, keep the posted reply as the answer of this turn.
  The For-you view then shows each answer once.
- Write the changed session status fields to a JSON file.
  Each field that the file leaves out keeps its stored value.
- The JSON object takes any of the keys `goal`, `now`, `waiting` and `remaining`.
- `remaining` is a list of objects with the keys `text` and `done`. An empty list removes Remaining.
- `waiting` is a list of objects with the keys `text`, `context` and `link`.
- When the file holds `waiting`, run `charliebot web status show` right before you write the file.
  Start from your own Waiting on you list in that output.
  Write the whole list: each item that stays, and each new or changed item.
  Each set replaces the whole list, so a list from a fresh `show` keeps every item that still waits on the user.
- Then run `charliebot web status set --file <path>`.
- The command refuses a link to an older version of a plan, or to a closed plan, and names the newest file. The
  panel then shows the version that the user decides on.
- The command gives each remaining Waiting on you item to a cold reader, unless the reader has already read it as it
  stands.
- The cold reader reads the item, and the page at its link when this host serves that page. It has no other context.
- The reader restates the decision, the line's state, the options with your recommendation, and the evidence links.
  It also names the kind of step.
- When the command prints these restatements, it stored nothing.
- Compare each restatement with your intent:
  - When a part reads NOT STATED or differs from your intent, revise the item.
  - When the reader names an undoable step, check that the item holds no decision that only the user makes.
    Then remove the item, and carry out the step on your recommendation.
    The list then holds only what the next milestone needs from the user.
- Then run the command again.
- The command stores the status when the reader has read every item as it stands.
- The command prints the stored session status.
- The printout also holds `now_chars` and `now_over_budget`.
- When `now_over_budget` is true, shorten **Now** and set it again.
- `charliebot web status confirm` records the confirmation and cold reads nothing. It refuses a stale stored plan
  link and names the newest file.
- After a context reset, read the session status with `charliebot web status show`.
- The command also prints the Waiting on you items of each open descendant session.
- The command also prints the **Now** of each open child session.
