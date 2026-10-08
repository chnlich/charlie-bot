Memory curation.

Read `~/workspace/charlie-bot/skills/llm-context-guideline/SKILL.md` first.
Read the Writing Style section of `~/workspace/charlie-bot/prompts/master.md` next, its "Text a
model reads" subsection included. Before you write or edit entry prose, check every added or
rewritten line against that section. Name each concept as the existing entries name it.

Step 1: open the day's PR worktree.
Run `charliebot memory proposal open` and read the worktree path from the `worktree` line of its
output. On a refusal, end the step with the command's reason as the final message.

Step 2: mine cross-session user messages into staging.
Run the digest script and read its output in full:
`python3 ~/workspace/charlie-bot/prompts/cron/memory_curator/user_message_digest.py > /tmp/curator_user_digest.txt`
Each output line is `<YYYY-MM-DD> <session-short-id> [NEW] <text>`: one user message from the
last 7 days across all sessions, artifact comments included, messages starting with `/` excluded; NEW marks
messages from the last 24 hours, and the digest caps at 120K characters, oldest lines dropped
first.
A theme becomes a mined candidate when all three hold: it appears in user messages of at least
two distinct sessions in the digest; at least one supporting message carries the NEW flag; and
the store index (`charliebot memory query --index`) has no entry covering it.
Write each qualifying theme as one staging capture via `charliebot memory add --file <tmpfile>`:
the first line `# <theme>` states the theme, and the body states the fact or preference to
record plus its provenance (each supporting session short id, date, and one quoted line). Step 3
curates these captures exactly like every other candidate.

Step 3: curate staging candidates into the PR worktree, merge-first.
Read every file in `~/.charliebot/memory/staging/`, and read the PR's coverage state with
`charliebot memory query --index --dir <worktree>`. Candidates are free-form captures: for each
candidate, first test it against the admission whitelist and home routing in the
llm-context-guideline skill (retaining durable mechanisms and routing execution artifacts,
procedures, or discoverable facts to their canonical homes), and write the proof lines it
requires. Draft each bullet at the category level, in one to three lines. Write every sentence
by the "Text a model reads" subsection. When an entry that you edit has more than 12 body lines,
trim the whole entry.
For each passing candidate, decide and finalize its `topic`, `scope`, `audience`, and `title`;
the default action is a merge:
- **revise (merge)**: fold the candidate into the existing entry whose theme covers it,
  normally the entry named in the candidate body when the body expresses a change intent,
  otherwise the thematically-matching entry, editing it in place inside the worktree so
  `git -C <worktree> diff` shows the before and after.
- **admit (new entry)**: the index shows zero entries covering the candidate's theme, and the
  title honestly describes the whole content. Create `entries/<topic>/<slug>.md` with a
  complete header (scope, topic, audience, title) and the candidate body. Add the topic to
  the `topics` vocabulary when it is new.
- **reject**: list the candidate in the handoff sheet with the question left unanswered or a
  one-line reason (wrong home, theme already covered, dishonest title, and so on).
Every entry and `topics` edit is written inside the worktree and stays uncommitted; the
reviewer performs the commits through `charliebot memory proposal commit`.
Move every processed candidate (admitted, revised, or rejected) from
`~/.charliebot/memory/staging/` into `~/.charliebot/memory-archive/staging-curation-<YYYYMMDD>/`,
in the format of the existing archive directories: one `README.md` row per file naming the
file, its disposition, and the entry path or the rejection reason, plus an updated `SHA256SUMS`
over the moved files.
Run `charliebot memory lint --dir <worktree>` and resolve every reported violation before you
end the step.

Step 4: deliver the handoff sheet.
The final message is the handoff sheet: one line per staging file with its disposition
(`revise <entry path>`, `admit <entry path>`, or `reject: <one-sentence reason>`), and one
block per entry holding that entry's three proof lines plus a `Staging:` line naming each
staging file the entry consumed. The selector's output is the handoff sheet alone.

Session mining reads user messages at daily curation, and its findings reach curation as
staging captures judged by the same admission test. The day's canon edits live in the PR
worktree, and the llm-context-guideline skill's "Proposal review and landing" chapter owns the
approval and landing flow.
