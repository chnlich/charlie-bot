Outcome: each morning the user opens one page and sees the work that the user needs to do and
has not started, together with what this run already did about it.

Read one picture in one pass, and name from it the work that the user needs to do and that
nobody has started:
- the user's work sources: the OKR and the Linear issues assigned to the user, where the user is
  the viewer of the `charliebot linear request` credential; GitHub review requests and
  assignments; the user's queued and running cluster jobs; and, since the date of the previous
  briefing page (the last seven days on the first run), the user's cluster jobs that ended, Slack
  mentions and direct messages, Gmail and Google Calendar;
- the work already under way: open CharlieBot sessions by name, goal, status, and Waiting on you items, Linear issues
  in a started state, and threads, mails and pull requests where the user already answered;
- the user's answers after earlier briefing pages, and this session's taste rows.
Work under way, work that the user already answered, and work of a kind that the taste rows
record as 不做 never become page items. 想问 items carry no recommendation.
Each source read is bounded in time, so the run finishes when a read waits: an ssh read to a
cluster can wait up to 30 minutes for a Tailscale SSH check-mode approval. A source whose read
fails or times out appears on the page as unread, with its error.

The page has two columns:
- 已做: work that this run finished, and work that the standing session dispatches on this run's
  report. A finished item keeps its product on the page or inside CharlieBot. A dispatched item
  is under one worker-day and changes nothing beyond a task branch or a draft pull request; at
  most three items are dispatched a day. Each 已做 item states its undo on the page.
- 想问: every other item. Each one names the high-consequence boundary that sends it to the user, or the fact
  that the run needs, and lets the user answer 做 or 不做. At most ten items stay visible, ranked
  by deadline and OKR impact; the others appear as counts per source.
While the standing session holds no user message, the delegation gate refuses dispatch, so work
that would be dispatched goes to 想问.
An unanswered 想问 item carries over to the next page for seven days, then leaves the page.
Every item links its source.

The page is a waiting-genre artifact in the standing session's artifacts directory, so the user's
comments on it reach the standing session. It passes the waiting-genre assertions.

Boundary: this run writes to no external system. A message in the user's name stays a draft.
Lookups read session names, goals and status files, so each lookup returns within seconds.

The standing session keeps exactly one Waiting on you item, which links the newest page. On each
answer, it records one taste row of kind choice with the option 做 or 不做. On the user's 做, it
starts the item under Action Boundaries in `prompts/master.md`.
