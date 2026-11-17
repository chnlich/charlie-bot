Outcome: each round, find the user's own work that passes every selection question, and start each qualifying item in its own session under this task.

Post one reply per round that states:
- each session opened, the work it finished, and what waits on the user, each with a link;
- one line with a link for each item that passes question 2 but another session holds or someone else is assigned to;
- each source this round could not read.

### Sources

Find work sources through this instance's OKR skill, credentials, and memory. Have a read-only worker read the sources, so this session keeps only their conclusions. The worker reads every result of each source for the last 7 days, following the source's next-page marker until none remains. A source that cannot be read is reported as unread; an unread source never counts as evidence that no work exists.

### Selection Questions

An item qualifies only when every answer is yes:
1. Is it the user's work? The user's work is an item assigned to the user, a request that names the user as its addressee, or an unassigned item under a KR that the user owns. A request addressed to a group does not count, even when the user belongs to that group.
2. Does it move an active key result (KR) that this instance's `okr` skill lists, or does a week of waiting cost something (a deadline passes, or someone stays blocked)? An instance without an `okr` skill reports its goals as an unread source.
3. Does nobody hold it? An open session holds its item; a session where the user stopped this item keeps holding it; an item that waits on the user's own answer does not count.
An item an open session holds gets its one reply line and nothing else: send no message to that session and change nothing in it.

### Escalation

Work estimated above about 8 hours, or whose result reaches other people, goes to a manager session whose approver is the user and that starts with an understanding page. Every other qualifying item goes to a worker.

### Boundaries

- Open sessions only under this task.
- Send no message to another session, and change no session outside this task's subtree.
- A message in the user's name is a draft that waits for the user's send action.
- The approval rules of `prompts/master.md` (Action Boundaries) apply unchanged.
