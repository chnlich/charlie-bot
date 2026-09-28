Memory curation review.

Read `~/workspace/charlie-bot/skills/llm-context-guideline/SKILL.md` first.
Read the Writing Style section of `~/workspace/charlie-bot/prompts/master.md` before judging any
entry line.

Your inputs are the selector's handoff sheet, delivered above under the heading
`## Result of the previous step (selector)`, the output of `charliebot memory proposal status`,
and the uncommitted delta of the PR worktree named by the status output's `worktree` line:

- The delta: `git -C <worktree> status --porcelain` and `git -C <worktree> diff`, with untracked
  files read in place.
- The handoff sheet: one line per staging file with its disposition (`revise <entry path>`,
  `admit <entry path>`, or `reject: <one-sentence reason>`), one block per entry holding that
  entry's three proof lines, and the `Staging:` lines naming the files each entry consumed.

Your edits stay inside the files of that delta, and every PR commit goes through
`charliebot memory proposal commit`.

Step 1: distill and align to standing standards (Condenser & Gatekeeper).
Your core duty is distilling drafts into concise, durable knowledge and aligning every line
to the writing standards:

a. Align admission to canonical homes:
   Verify every entry against `skills/llm-context-guideline/SKILL.md`. Retain durable mechanisms
   and standing rules in the store. Route ephemeral execution identifiers to session logs or
   `LESSONS.md`, route procedures and documentation to owning repository guides or skills, and
   leave discoverable tool facts to runtime query. Restore a file whose content belongs in an
   external home (`git -C <worktree> checkout -- <file>`, or delete it when the PR created it).

b. Distill to category-level conclusions:
   Condense drafts into concise category-level conclusions, bounded to one to three lines per
   bullet. Retain the essential mechanism while pruning background narratives, incident
   timelines, and intermediate deductions.

c. Apply the skill's "Phrasing for model context" rule:
   Check every line the PR adds or rewrites against that rule, so each rule sentence states the
   action to take or the standing reality and each example shows the practice itself.

Record every substantive condensation, routing restoration, disposition reversal, and phrasing
rewrite as a row in your report with its reason.

Step 2: lint.
Run `charliebot memory lint --dir <worktree>` and resolve every reported violation before
Step 3.

Step 3: commit entry by entry.
Write each commit message to a file and run
`charliebot memory proposal commit <store-relative path> --message-file F`, one commit per
changed file. The subject follows the store's convention
(`admit:` / `revise:` / `migrate:` / `remove:` / `scaffold:` plus `<topic>/<slug> (<title>)`),
and the body holds the entry's three proof lines plus one `Staging: <file>` line per consumed
candidate. When the command refuses, restore the lines it lists to the PR's wording, record the
conflict as a report row for the user (the candidate's intent beside the PR line it meets), and
run the commit again.

Step 4: render the PR page.
Render the page from `~/workspace/charlie-bot/prompts/memory_report_template.html`: copy it,
replace every placeholder, and delete the BLOCK comments, in Chinese as the template prescribes.
The page names every PR commit (`git log <base>..<head>` subjects with their proof lines),
today's rewrites, the recorded conflicts, and the candidates rejected since the PR opened. Run
`charliebot memory proposal status` again after the commits and take the page's head SHA, the
`opened` date, and the diff link from its `head`, `opened`, and `diff_path` lines. The session's
artifacts directory sits at `../../artifacts/` relative to your working directory; write
`memory_report_<YYYY-MM-DD>.html` there, with the header's diff link pointing at `diff_path`
root-relatively.

Your final message gives the page path, the head SHA, `diff_path`, and the day's disposition
counts (`admit N, revise N, reject N`). An empty delta ends the step with a one-sentence final
message.
