Daily memory curation.

Read `~/workspace/charlie-bot/prompts/memory_guideline.md` first.

Step 0 — guard against an undecided prior day.
If the memory repo working tree at `~/.charliebot/memory/` is dirty (uncommitted changes from a
prior day's undecided proposal), fail loud: stop, re-present the pending diff, and ask the user to
approve or reject before doing anything else. Do not proceed to staging while the canon has
uncommitted changes.

Step 1 — curate staging candidates, merge-first.
Read every file in `~/.charliebot/memory/staging/`. For each candidate, first apply the admission
test from memory_guideline.md with evidence. For each passing candidate, the default action is a
merge, not a new entry:
- **revise (merge)**: fold the candidate into the existing entry whose theme covers it — normally
  `entries/<topic>/<revises>.md` when the candidate carries `revises: <slug>`, otherwise the
  thematically-matching entry — editing it in place so `git diff` shows the before and after.
- **admit (new entry)**: ONLY when no existing entry's theme covers the candidate AND the title
  honestly describes the whole content. Create `entries/<topic>/<slug>.md` with a complete header
  (scope, topic, audience, title) and the candidate body. Add the topic to the `topics`
  vocabulary if it is not already there.
- **reject**: do not admit — list the candidate in the report with a one-line reason (failed the
  admission test, wrong home, theme already covered, dishonest title, and so on).

Run `charliebot memory lint`; it must pass before you present. If lint reports violations, fix the
working tree until it is clean.

Step 2 — report to the user.
Present, in one report: the full working-tree diff (admits and revises) and the rejected
candidates with reasons. Do not commit. Wait for explicit user approval.

Step 3 — land only after approval.
Only after the user explicitly approves: commit the working tree with the prefixed messages
(`admit:` / `revise:` / `migrate:`), then delete the processed staging files — including the
rejected ones. If approval is partial, commit only the approved changes and delete only their
staging files; leave the rest staged for the next day.

Never mine sessions for memory. Never auto-commit. Never edit any file outside
`~/.charliebot/memory/`.
