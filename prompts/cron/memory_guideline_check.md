Daily MEMORY maintenance.

Read `~/workspace/charlie-bot/prompts/memory_guideline.md` first.

Step 0 — merge staged candidates.
Snapshot `~/.charliebot/MEMORY.md` and `~/.charliebot/MEMORY.host.md` into `/tmp/charliebot-memory-bak/`, keeping the original filenames. Merge every entry in `~/.charliebot/MEMORY.tmp.md` into `MEMORY.md` or `MEMORY.host.md` following memory_guideline.md: drop what the guideline excludes, revise an existing entry in place instead of appending a duplicate, and rewrite each surviving entry into timeless form. Admission is the exception: most staged entries expire with their task or live in another home. When in doubt, record the drop in the report; a dropped entry survives there for review. Truncate `MEMORY.tmp.md` back to its header only after the merge succeeds; on failure leave it untouched and report. Run the admission test on each entry's final merged text as it stands in the file. Report each entry as dropped (with the reason) or accepted (quote the merged text verbatim and name in one line the standing preference, fact, or behavior it records). For an in-place revision, show the entry before and after. End the report with the full diff of MEMORY.md and MEMORY.host.md against that snapshot directory.

Step 1 — repair existing memory.
Inspect `~/.charliebot/MEMORY.md`, `~/.charliebot/MEMORY.host.md`, and `~/.charliebot/LESSONS.md`. Make minimal in-place edits to the correct local memory file when needed; keep MEMORY organized by topic and avoid chronological notes. Do not modify shared repo files.

Step 2 — mine yesterday's sessions for new memory.
Scan CharlieBot sessions from the last 24 hours (under ~/.charliebot/sessions/) whose metadata shows activity within that window. For each session, read metadata.json and the events.ndjson files under threads/*/.

Identify durable facts, user preferences, or master-agent guidance that should be recorded in `~/.charliebot/MEMORY.md` or `~/.charliebot/MEMORY.host.md`. Skip transient project details, task execution logs, and anything that belongs in `LESSONS.md` or skills.

Before proposing additions, re-read the current `~/.charliebot/MEMORY.md` and `~/.charliebot/MEMORY.host.md` to avoid duplicates. Propose only genuinely new or changed entries. Do NOT edit any files in this step.

Report: (a) what changed in Step 1, and (b) any proposed additions from Step 2 with source session id and reason.