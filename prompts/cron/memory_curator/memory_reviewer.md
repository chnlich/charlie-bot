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

Step 1: distill and align to standing standards (Condenser & Gatekeeper).
Your core duty is distilling drafts into concise, durable knowledge and aligning every line
to the writing standards:

a. Align admission to canonical homes:
   Verify every entry against `skills/llm-context-guideline/SKILL.md`. Retain durable mechanisms
   and standing rules in the store. Route ephemeral execution identifiers to session logs or
   `LESSONS.md`, route procedures and documentation to owning repository guides or skills, and
   leave discoverable tool facts to runtime query. Restore files whose content belongs in
   external homes (`git checkout -- <file>` or remove if newly created).

b. Distill to category-level conclusions:
   Condense drafts into concise category-level conclusions, bounded to one to three lines per
   bullet. Retain the essential mechanism while pruning background narratives, incident timelines,
   and intermediate deductions.

c. Enforce positive framing:
   Align every line to positive framing (`prompts/master.md` Writing Style): state the working
   action or standing reality, and lead any guidance with the alternative that works.

Record every substantive condensation, routing restoration, and positive-framing rewrite as a row
in your report with its rationale.

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
