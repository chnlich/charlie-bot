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
  - Name each step that changes something outside this session, whatever its tier.
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
- The T0 steps that one take off settles form one Waiting on you item.
- When a plan trade-off is T2, take its recommendation and leave it out of Waiting on you.
- Name that default and the reply that changes it in the web reply.
  The panel then lists only the decisions that only the user makes.
- When a decision needs a separate reply, give it its own Waiting on you item.
- A separate reply comes from another person or at another time.
- The user then clears each Waiting on you item with one reply.
- Write the decision or the action in the item's `text`, in one sentence.
- Write in `context` what the line does now and why the decision comes now, in one or two sentences.
  The panel then shows why the item waits, before the user opens the page.
- When a decision needs a page, write each option on its own visible line with its name, action and consequence.
- When a decision needs a page, also write your recommendation and the evidence links on it.
  The user then reads every option, and the evidence for it, on one page.
- Set the item's `link` to that page.
  The user then reads the options with their layout, and comments on the page reach this session.
- When the item decides on a plan page or an understanding page, use that page.
  That page already holds the options, the recommendation and the evidence.
- For an item whose options compare in the item text alone, name each option in `text` and `context`, and set
  `link` to the page that carries the work. The user then decides without a waiting page of its own.
- For any other item, write the page body to a fragment file.
- Then run `charliebot artifact wrap --genre waiting --output artifacts/waiting_<slug>.html <fragment>`.
  The command adds the head and style of the decision-page template.
- On that page, show the options as one `div.fork` block whose option lines sit outside every folded block.
  The user then reads every decision in one layout.
- When you want the failures before the status set, pass the page through the gate (`file-server` skill, Cold-Read Gate).
  The status set then refuses a failing page before any cold read starts.
- When the user acts on another page, such as a login page, link that page from the item's page.
  The item's page then holds the context, and the user reaches the action from it.
- A user-act item stays in Waiting on you only: the web reply does not request its action again.
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
- When the file holds `waiting`, run `charliebot web status show` right before you write the file, and write the
  whole list from it: each item that stays, and each new or changed item. Each set replaces the whole list.
- Then run `charliebot web status set --file <path>`.
- The command refuses a link to an older version of a plan, or to a closed plan, and names the newest file.
- The command cold-reads each remaining Waiting on you item and its linked page, and prints each restatement
  without storing.
- When an item's linked page fails a fork assertion of its genre, fix the page, then run the set again.
  The reader then sees only pages that pass those fork assertions.
  The command takes the genre from the page's file-name prefix: plan, understanding, sitrep, debug, explain or waiting.
  A linked page whose name has no such prefix is not checked.
  When an assertion fails, the command prints each FAIL line and stores nothing.
- Compare each restatement with your intent:
  - When a part reads NOT STATED, reads FOLDED, or differs from your intent, revise the item.
    The user then reads each option where the page shows it.
  - When the cold read tiers an item t1 or t2, remove the item, carry out its step, and record it with
    `charliebot digest add` (Execution section of `skills/plan-approval/SKILL.md`).
    The list then holds only what the next milestone needs from the user.
- Run the command again. It stores the status once the reader has read every item as it stands, and prints the
  stored session status with `now_chars` and `now_over_budget`. Shorten **Now** and set it again when
  `now_over_budget` is true.
- `charliebot web status confirm` records the confirmation and cold reads nothing. It refuses a stale stored plan
  link and names the newest file.
- After a context reset, read the session status with `charliebot web status show`.
- The command also prints the Waiting on you items of each open descendant session.
- The command also prints the **Now** of each open child session.
