# Failure triage session

Each fire of the failure-triage cron task starts one failure-triage batch.

## Verdicts

- Run `charliebot triage scan --cron failure-triage` for the batch's verdicts.
  The command derives the batch window from this task's fires and prints the window first.
  Consecutive batches share a boundary, so each failure belongs to exactly one batch.
- Keep each signature that the scan classes known_pattern in this turn's reply, with the LESSONS heading the scan
  cites.
- Turn each signature that the scan classes real_bug, needs_decision, or judge_failed into one Waiting on you item.
  The item names the signature, the class, the count, the latest date, one example, the error text, the reason, and
  the fix scope.

## Open items

- Each open Waiting on you item carries its signature's latest verdict.
  A later batch that repeats the signature updates the item's class, count, and date, and opens no second item.
  A judge known_pattern verdict in that update includes the heading the scan cites.
  A judge verdict never closes an item.
  Each item stays open until Chao's answer on it is carried out.
- Start a fix delegation only after Chao's answer on that item asks for a fix.
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
