Daily memory curation.

Read `~/workspace/charlie-bot/prompts/memory_guideline.md` first.

Step 0 — guard against an undecided prior day.
If the memory repo working tree at `~/.charliebot/memory/` is dirty (uncommitted changes from a
prior day's undecided proposal), fail loud: stop, re-present the pending diff, and ask the user to
approve or reject before doing anything else. Do not proceed to staging while the canon has
uncommitted changes.

Step 1 — curate staging candidates.
Read every file in `~/.charliebot/memory/staging/`. For each candidate, build a working-tree diff
against the canon:
- **admit**: a new entry — create `entries/<topic>/<slug>.md` with a complete header (scope,
  topic, audience, created, source) and the candidate body. Add the topic to the `topics`
  vocabulary if it is not already there.
- **revise**: an in-place edit of an existing entry — when the candidate carries `revises: <slug>`,
  edit `entries/<topic>/<revises>.md` in place so `git diff` shows the before and after.
- **reject**: do not admit — list the candidate in the report with a one-line reason (failed the
  admission test, wrong home, and so on).

Apply the admission test from memory_guideline.md to each candidate with evidence. Run
`charliebot memory lint`; it must pass before you present. If lint reports violations, fix the
working tree until it is clean.

Step 2 — propose evictions.
Run `charliebot memory usage --idle-days 60`. For each non-resident entry that is over the idle
threshold and older than 60 days by its `created` date, propose `git rm entries/<topic>/<slug>.md`.
Resident topics and entries younger than 60 days are exempt.

Step 3 — report to the user.
Present, in one report: the full working-tree diff (admits, revises, evictions), the rejected
candidates with reasons, and the usage table. Do not commit. Wait for explicit user approval.

Step 4 — land only after approval.
Only after the user explicitly approves: commit the working tree with the prefixed messages
(`admit:` / `revise:` / `evict:`), then delete the processed staging files — including the rejected
ones. If approval is partial, commit only the approved changes and delete only their staging files;
leave the rest staged for the next day.

Never mine sessions for memory. Never auto-commit. Never edit any file outside
`~/.charliebot/memory/`.
