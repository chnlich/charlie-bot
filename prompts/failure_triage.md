# Failure triage session

Each fire of the failure-triage cron task starts one failure-triage batch.

## Verdicts

- Run `charliebot triage scan --cron failure-triage` for the batch's verdicts.
  The command derives the batch window from this task's fires and prints the window first.
  Consecutive batches share a boundary, so each failure belongs to exactly one batch.
- Keep each signature that the scan classes known_pattern in this turn's reply, with the LESSONS heading the scan
  cites.
- Turn each signature that the scan classes real_bug, needs_decision, or judge_failed into one open item.
  The item names the signature, the class, the count, the latest date, one example, the error text, the reason, and
  the recommended action: a fix with its scope, or a LESSONS.md entry.

## Open items

- Each open item is one signature that awaits Chao's answer. It carries the signature's latest verdict and its
  recommended action.
- While any open item remains, the triage session keeps one Waiting on you item for all of them.
- That item asks Chao for one reply, 「按推荐」, and links the page that lists each open item with its recommended
  action.
- A 「按推荐」 reply answers every open item on the linked page with that item's recommended action.
- A per-item answer overrides the recommended action of the item it names.
- On the first batch after this rule takes effect, move every existing open item and its recommended action onto the
  linked page, then remove each item's separate Waiting on you item.
- A later batch that repeats the signature updates the item's class, count, date, and recommended action, and opens no
  second item.
  A judge known_pattern verdict in that update includes the heading the scan cites.
  A judge verdict never closes an item.
  Each item stays open until Chao's answer on it is carried out.
- Start a fix delegation only after Chao's answer, the 「按推荐」 reply or a per-item answer, asks for a fix on that item.
- When Chao's answer settles an item without a fix, append a LESSONS.md entry that quotes the signature key
  verbatim.
  Later scans class that signature known_pattern in code and cite that entry.
- Close a fix item when the live release contains the fix commit.
  The live release is the commit that the release `current` selects.
  After that close, an occurrence of the signature opens a new item that names the fix commit only when the
  occurrence postdates the switch to that release.
  The switch log, `switch_<YYYYMMDD_HHMMSS>.log` in the server log directory, dates the switch: its file name holds
  the switch time, and its `SWITCH COMPLETE` line names the commit that the server serves.

## Turn end

- End each batch turn with the scan's verdicts for its window, or with one Waiting on you item that names the window
  and the scan error.
  The next batch starts at its own fire, so a window without verdicts is scanned again only when Chao asks.
