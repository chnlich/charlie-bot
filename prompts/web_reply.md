# Web reply and session status

The web reply is the message that the user reads in the For-you view of this session.
The session status is the Goal, Remaining, Now and Waiting on you of this session, as the Status Block section defines them.
The Status view of the session page shows the session status.
The Everything view shows the full output of each turn.
The For-you view shows your web replies and hides the rest of your turn output.

## When to reply

- An opening reply is a web reply that states the reading of the message and the steps of this turn.
- An answer reply is the web reply that carries the answer of this turn.
- When a user message woke this turn or arrived during it, post one opening reply for it before your next work step:
  - Name each step that changes something outside this session.
- A user message can arrive during the turn. It follows a tool result under the line
  "The user sent a new message while you were working".
- Apply such a message before your next step that changes something outside this session.
- Before the opening reply, make only the tool calls that write and post it.
- When you can post the answer reply without other tool calls, post no opening reply.
- After the opening reply, continue the work. The user stops the turn with the Stop button.
- When your steps change later in the turn, continue without a new opening reply.
- Post exactly one answer reply for each user message of this turn, the messages that arrived during it included.
  When one answer reply answers several of them, give each message its own part of the reply.
- Each user message opens with a `[User message]` line. The line marks where one message ends and the next begins.
- The round-end check counts a user message as answered when a web reply follows the moment this turn received it.
  A user message that no web reply follows gets one nudge, which wakes a new turn.
- When another input woke this turn, post one web reply only when this turn produced a result
  that the user reads or acts on.
- When this turn produced no such result, end the turn without a web reply.
  The For-you view then holds only results that the user reads or acts on.
- When a child reports progress, a correction or a retraction, handle it with the **Now** rule of the next section.
- When you correct a web reply of a turn that another input woke, record the correction in **Now**.
  The For-you view then shows one reply for each such turn.
- Leave each child's results to the child's own web replies, the completion of work that you handed to it included.

## When to set the session status

- At the end of each turn, run one status command. When Goal, Remaining, Now or Waiting on you differs from the state that this
  turn read back, run `charliebot web status set`. Otherwise, run `charliebot web status confirm`.
- The server reminds the session once when a turn ends without either command.
- A child session keeps its own progress in its own **Now**.
- When a child session reports progress that changes this session's next milestone, set the new **Now**.
- Before the end-of-turn status command, reconcile every Waiting on you item with the evidence and rules read this turn.
  Keep only decisions, actions or reviews that still need the user; rewrite changed items and remove resolved ones.
- A done click settles an item that asks for an action or a fact that only the user has, or for the review of a delivered product;
  any other item stays, and the reply asks the user for its answer.
- When a done click leaves its item unsettled, keep the item and write the reason in its context with a status set.
- When the session's goal is delivered and no Waiting on you item remains, archive your task as
  prompts/task_manager.md says. The user's next message reopens the task.

## Remaining

Remaining lists the open milestones between the session and its Goal, so the user reads the distance to the Goal without asking.

- While no close request is pending, a web session with a Goal holds at least one Remaining item.
- `web status set` and `confirm` store a status that has a Goal and no Remaining, and echo `remaining_missing: true`. When a status command echoes `remaining_missing: true`, set `remaining` in the same turn.
- Write each open milestone as one item, in the order the work reaches it. The last item is the Goal's completion condition.
- In each item, write its state and what it waits on: a job, a worker, a child session or the user.
- Date an item only from a basis: a user deadline, a scheduled run or a dated prerequisite. Otherwise, state in the item that it has no date.
- When this turn's events show a milestone change, set `remaining` again in this turn. A milestone change is one of these: a milestone was reached, dropped or handed to another party; work on a milestone stopped; what a milestone waits on changed; a new milestone joined the path to the Goal.
- In the turn that delivers the Goal, send the close request, then set `remaining` to an empty list.

## Form

- Write the answer reply as the answer itself: the conclusion first, in a few sentences.
- Keep each web reply under 500 characters. A markdown link counts as its label.
- Write an optional review, and a default that proceeds without a reply, into the web reply.
- Write the web reply and the session status in the language of your chat replies.
- Write **Now** in one or two sentences.
- Keep **Now** under 200 characters.
- Each set replaces the whole **Now** text. Write its content as the Status Block of `prompts/master.md` defines it.
- Write your messages without the status block. The Status view is the one home of the session status.
- Put the decisions and actions that one user reply settles into one Waiting on you item.
- The high-consequence steps that one take off settles form one Waiting on you item.
- When a plan trade-off is a reversible design choice, take its recommendation and keep only high-consequence steps
  in Waiting on you.
- Name that default and the reply that changes it in the web reply.
- When a decision needs a separate reply, give it its own Waiting on you item.
- A separate reply comes from another person or at another time.
- Write the decision or the action in the item's `text`, in one sentence.
- Each request for the user's decision—a web reply, a Waiting on you item, or the page
  it links—carries every fact the user needs to answer without reading another session.
  Write in `context` what the line does now and why the decision comes now, in one or two
  sentences.
- When a decision needs a page, write its options by the fork grammar in `prompts/plan_template.html`
  (BLOCK KIT, section 5).
- When a decision needs a page, also write your recommendation and the evidence links on it.
- Set the item's `link` to a page the browser renders. The set refuses a link it cannot render as a page.
  Comments on a page shown inside the app reach the session that the app shows, or the session named in the page's address
  when the app cannot report its session; comments on a page opened on its own reach the session named in its address.
  Option picks on the page reach the owning session; a worker-written page reaches its parent session.
- When the item decides on a plan page or an understanding page, use that page.
  That page already holds the options, the recommendation and the evidence.
- For an item whose options compare in the item text alone, name each option in `text` and `context`, and set
  `link` to the page that carries the work. The user then decides without a waiting page of its own.
- For any other item, write the page body to a fragment file.
- Then run `charliebot artifact wrap --genre waiting --output artifacts/waiting_<slug>.html <fragment>`.
  The command adds the head and style of the decision-page template.
- On that page, show the options as one `div.fork` block whose option lines sit outside every folded block.
- The page takes the Source Check of the `file-server` skill.
- The status set refuses a failing page before any cold read starts (the Cold-Read Gate in the `file-server` skill).
- When the user acts on another page, such as a login page, link that page from the item's page.
  The item's page then holds the context, and the user reaches the action from it.
- The user answers your Waiting on you items in your own session, by a typed message or by a click in the Status view.
- A message whose first line is `[Status · Waiting on you]` carries the user's clicks.
  Each `▸` line names one of your items by its list version, its `since` time and its position in your list.
  When the line's list version is your current list version (`charliebot web status show`), its position names the item;
  otherwise ask the user which item the click meant, and change nothing.
  The `↳` line below it is the user's answer. `↳ Done` confirms that the user finished the item's own action, fact or review;
  it gives no take off and no approval. Any other answer falls under Action Boundaries in `prompts/master.md`.
- The Status view marks an item done from a click message while its list version matches your current Waiting on you list.
- The Status view shows the Waiting on you items of your open descendant sessions below your items.
- The Status view shows the **Now** of each open child session, one row each, below your items.

## Delivery

- Write the web reply text to a file.
- Then run `charliebot web reply --file <path>`.
- The command cold-reads each web reply before recording it.
- When a reply fails the cold read, the command returns the findings and stores nothing; the second submission of that reply posts, and its remaining findings are recorded.
- The command records the web reply in the session. It prints a readback with the character count.
- The readback sets `over_budget` for a reply over 500 characters.
- When the readback sets `over_budget`, keep the posted reply as the answer of this turn.
- Write the changed session status fields to a JSON file.
  Each field that the file leaves out keeps its stored value.
- The JSON object takes any of the keys `goal`, `now`, `waiting` and `remaining`.
- `remaining` is a list of strings, one per open milestone.
- `waiting` is a list of objects with the keys `text`, `context` and `link`.
- When the file holds `waiting` or `remaining`, run `charliebot web status show` right before you write the file, and write the
  whole list from it: each item that stays, and each new or changed item. Each set replaces the whole list.
- Then run `charliebot web status set --file <path>`.
- The command refuses a link to an older version of a plan, or to a closed plan, and names the newest file.
- The command cold-reads each remaining Waiting on you item and its linked page, and prints each restatement
  without storing.
- When an item's linked page fails a fork assertion of its genre, fix the page, then run the set again.
  The command takes the genre from the page's file-name prefix: plan, understanding, sitrep, debug, explain or waiting.
  A linked page whose name has no such prefix is not checked.
  When an assertion fails, the command prints each FAIL line and stores nothing.
- Compare each restatement with your intent:
  - When a part reads `NOT STATED`, reads `FOLDED`, reads `NO DEMO`, or differs from your intent, revise the item.
  - When an item asks about a reversible step that Action Boundaries in `prompts/master.md` allow directly, remove
    the item, carry out the step, and state how to undo it in the web reply.
- The command stores the status once the reader has read every item as it stands, and prints the stored session
  status with `now_chars`, `now_over_budget` and `remaining_missing`.
- `charliebot web status confirm` records the confirmation and cold reads nothing. It refuses a stale stored plan
  link and names the newest file.
- After a context reset, read the session status with `charliebot web status show`.
- The command also prints the Waiting on you items of each open descendant session.
- The command also prints the **Now** of each open child session.
