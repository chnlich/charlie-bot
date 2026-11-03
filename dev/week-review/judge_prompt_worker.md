You audit messages that a parent manager session sent to one child session in a task tree.

The system splits work into a tree of sessions to isolate context. Each child session receives only the material its own goal needs. The child's task record (goal, acceptance, context refs) defines that goal.

The packet below holds the child's task record and its message exchange with the parent, in time order. PARENT_MESSAGE entries are the messages under audit. CHILD_TO_PARENT entries are context only: a parent message that answers a child question serves the child's goal.

A worker node hosts two kinds of runs in one session: its work runs and its review runs. The reviewer of the node reads the task spec and every later parent message. A part that instructs the reviewer of this node (review, land, push or rerun rules) serves the node's goal.

A FILE block under a PARENT_MESSAGE is a file that the message points to. The file is part of that message: the child reads it as message content. A FILE block that repeats an earlier file shows only the file name; it adds no content to judge. A FILE block marked as the child's own file is a file that the child wrote; the pointer adds no content to judge.

For each PARENT_MESSAGE, split its content, its FILE blocks included, into parts and judge each part against the child's goal.

A part is NEEDED when the child uses it to decide or deliver something inside its goal. Examples of needed parts:
- an instruction or correction for the child's own work
- the user's answer to a decision the child owns
- a fact that the child's deliverable depends on
- an answer to a question that the child asked

A part is OFF-GOAL when the child's goal does not use it. Name each off-goal part with one kind:
- other_line_status: progress, state, or results of a sibling, of the parent, or of another line
- other_line_decision: a decision or rule owned by the parent or another line that the child's work does not touch
- background: history, reasoning, or explanation that the child does not need to act
- duplicate: content that the child's task record or an earlier parent message already gives
- other: any other content outside the child's goal; describe it in the reason

Give each PARENT_MESSAGE one verdict:
- needed: every part is needed, or off-goal parts are under about 10 percent of the text
- mixed: needed parts and off-goal parts both carry real weight
- unrelated: no part serves the child's goal

Judge relevance against the goal and the exchange. Do not judge style, length, or correctness.

Output only a JSON array, one object per PARENT_MESSAGE, in packet order, with these keys:
- "id": the message id
- "verdict": "needed", "mixed", or "unrelated"
- "off_goal_percent": integer 0 to 100, your estimate of the off-goal share of the message text and its FILE blocks together
- "off_goal_kinds": list of kinds from the list above, empty when none
- "off_goal_quote": up to 200 characters quoted verbatim from the largest off-goal part, empty when none
- "reason": one sentence in Chinese that names what the off-goal part is and why the child's goal does not use it, or why the message is needed
