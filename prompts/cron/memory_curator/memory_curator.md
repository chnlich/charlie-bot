Memory curation.

Read `~/workspace/charlie-bot/skills/llm-context-guideline/SKILL.md` first.
Read the Writing Style section of `~/workspace/charlie-bot/prompts/master.md` before writing or
editing any entry prose, and check every added or rewritten line against it, ending with a framing-only
pass over the added lines (each states the working action or standing reality first, and gives the
healthy or working reading before any failure reading).

Step 1: guard against an undecided prior day.
If the memory repo working tree at `~/.charliebot/memory/` is dirty (uncommitted changes from a
prior day's undecided proposal), fail loud: stop, re-present the pending diff, and ask the user
to approve or reject before doing anything else. Do not proceed to staging while the canon has
uncommitted changes.

Step 2: mine cross-session user messages into staging.
Run the digest script and read its output in full:
`python3 ~/workspace/charlie-bot/prompts/cron/memory_curator/user_message_digest.py > /tmp/curator_user_digest.txt`
Each output line is `<YYYY-MM-DD> <session-short-id> [NEW] <text>`: one user message from the
last 7 days across all sessions, artifact comments included, slash commands excluded; NEW marks
messages from the last 24 hours, and the digest caps at 120K characters, oldest lines dropped
first.
A theme becomes a mined candidate when all three hold: it appears in user messages of at least
two distinct sessions in the digest; at least one supporting message carries the NEW flag; and
the store index (`charliebot memory query --index`) has no entry covering it.
Write each qualifying theme as one staging capture via `charliebot memory add --file <tmpfile>`:
the first line `# <theme>` states the theme, and the body states the fact or preference to
record plus its provenance (each supporting session short id, date, and one quoted line). Step 3
curates these captures exactly like every other candidate.

Step 3: curate staging candidates, merge-first.
Read every file in `~/.charliebot/memory/staging/`. Candidates are free-form captures: for
each candidate, first test it against
the admission whitelist in the llm-context-guideline skill and write the proof lines it requires.
For each
passing candidate, decide and finalize its `topic`, `scope`, `audience`, and `title`; the
default action is a merge, not a new entry:
- **revise (merge)**: fold the candidate into the existing entry whose theme covers it,
  normally the entry named in the candidate body when the body expresses a change intent,
  otherwise the thematically-matching entry, editing it in place so `git diff` shows the
  before and after.
- **admit (new entry)**: ONLY when no existing entry's theme covers the candidate AND the
  title honestly describes the whole content. Create `entries/<topic>/<slug>.md` with a
  complete header (scope, topic, audience, title) and the candidate body. Add the topic to
  the `topics` vocabulary if it is not already there.
- **reject**: do not admit; list the candidate in the report with the question it could not
  answer or a one-line reason (wrong home, theme already covered, dishonest title, and so on).

Run `charliebot memory lint`; it must pass before you present. If lint reports violations, fix
the working tree until it is clean.

Step 4: report to the user.
Render the report from `prompts/memory_report_template.html`. Do not commit. Wait for explicit
user approval.

Step 5: land only after approval.
Only after the user explicitly approves: commit the working tree with the prefixed messages
(`admit:` / `revise:` / `migrate:`), then delete the processed staging files, including the
rejected ones. If approval is partial, commit only the approved changes and delete only their
staging files; leave the rest staged for the next day.

Step 6: prevent repeat feedback.
Treat every user comment on a proposal as evidence of a rule gap. After applying the comment,
check whether the llm-context-guideline skill or this prompt would have blocked the commented
content had
the rules been followed; when they would not, present in the same reply the one-line amendment
that would, and land it through the normal repo change flow after approval.

Session mining reads user events only, at daily curation; its findings enter the store only
as staging captures through the same admission test. Never auto-commit. Never edit any file
outside `~/.charliebot/memory/`.
