Memory curation review.

Read `~/workspace/charlie-bot/skills/llm-context-guideline/SKILL.md` first.
Read the Writing Style section of `~/workspace/charlie-bot/prompts/master.md` before judging any
entry line.

Your inputs are the working-tree diff of the memory store and the selector's handoff sheet,
delivered above under the heading `## Result of the previous step (selector)`:

- The diff: `git -C ~/.charliebot/memory diff`.
- The handoff sheet: one line per staging file with its disposition (`revise <entry path>`,
  `admit <entry path>`, or `reject: <one-sentence reason>`) followed by that entry's three
  proof lines. An `admit <entry path>` file does not appear in the diff (it is untracked); read
  it in place.

Step 1: gate every changed line.
For every added or rewritten line, ask whether it still holds after the model, the fix, and the
next run are replaced, and check it against the guideline's Entry form (narrative, length,
phrasing). A line that fails is deleted or trimmed; a file whose whole change fails is restored
with `git checkout -- <file>`, and an admitted file that wholly fails is deleted. You write no
new entry prose. Every reversal becomes a reject row in the report with a one-sentence reason.

Step 2: lint.
Run `charliebot memory lint` and fix violations by removal until it is clean.

Step 3: render the report.
Render the report from `~/workspace/charlie-bot/prompts/memory_report_template.html`: copy it,
replace every placeholder, and delete the BLOCK comments, in Chinese as the template prescribes.
The reject table lists every candidate the selector rejected, beside the rows for your
reversals. The session's artifacts directory sits at `../../artifacts/` relative to your working
directory; write `memory_report_<YYYY-MM-DD>.html` there.

Step 4: report.
Your final message is the report path and the disposition counts (`admit N, revise N, reject N`).
An empty diff ends the step with a one-sentence final message instead.

Never commit. Never edit any file outside `~/.charliebot/memory/`, the rendered report excepted.
