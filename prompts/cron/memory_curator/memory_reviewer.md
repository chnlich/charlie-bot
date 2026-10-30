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
   Condense drafts into concise category-level conclusions within the entry brevity bar in
   `skills/llm-context-guideline/SKILL.md`. Retain the essential mechanism while pruning
   background narratives, incident timelines, and intermediate deductions.

c. Check each sentence that the PR adds or rewrites against the "Text a model reads" subsection.
   Apply its bullets one at a time. Rewrite each sentence that falls short.

Record every substantive condensation, routing restoration, disposition reversal, and phrasing
rewrite as a row in your report with its reason.

Step 2: lint.
Run `charliebot memory lint --dir <worktree>` and resolve every reported violation before
Step 3.

Step 3: commit entry by entry.
Write each commit message to a file and run
`charliebot memory proposal commit <store-relative path> --message-file F`, one commit per
changed file. The subject follows the store's convention as the skill's "Commit message
prefixes" chapter words it (`admit:` / `revise:` / `remove:` / `scaffold:`),
and the body holds the entry's three proof lines plus one `Staging: <file>` line per consumed
candidate. When the command lists PR lines that the worktree lost, restore their PR wording. Record the
conflict as a report row: the candidate's intent beside the PR line it meets. When the command reports a
prose violation, rewrite the listed text, earlier PR lines included. Then run the commit again. When the
command prints a negation warning, rewrite the listed sentence the same way, and commit the file again.

Step 4: land the PR.
Run `charliebot memory proposal status`, and record its `head`, `opened`, and `diff_path` lines.
This pins the PR head that the land step names.
Run `charliebot memory proposal land <head>` with that head.
This moves the live store to exactly the version that this run committed and linted.
Use these fields in the landed-day header:
- `landed`: the landed range
- `commits`: the commit count
- `diff_path`: the diff link
The header names this run's landed range and links to its diff.
When the command refuses, keep the PR as it stands.
Quote the refusal on the page and in your final message.
The next run's land carries these commits, and the user learns why the day did not land.

Step 5: render the PR page.
Render the page from `~/workspace/charlie-bot/prompts/memory_report_template.html`.
Copy the template, replace every placeholder, and delete the BLOCK comments.
Write the filled prose in Chinese, as the template prescribes.
The template fixes the page layout and language, so every PR page reads alike.
The page names every PR commit (`git log <base>..<head>` subjects with their proof lines), today's
rewrites, the recorded conflicts, and the candidates rejected since the PR opened.
Fill the header from the `land` output:
- the `landed` range
- the `commits` count
- the `diff_path` link
After a refusal, fill the header with:
- the `head` SHA and `diff_path` link from the Step 4 `status` output
- the quoted refusal from the `land` output
The refusal header links to the pending version and explains why it stayed pending.
Start the rejected-candidates block at the Step 4 `opened` date.
This block lists only the rejections that this PR has held.
Write `memory_report_<YYYY-MM-DD>.html` to the session's artifacts directory, `../../artifacts/`
relative to your working directory.
Point the header's diff link at `diff_path` root-relatively.
The user opens the page from that directory, and the root-relative link opens the diff page from it.

For a landed day, use the `land` output's `landed` and `diff_path` fields.
After a refusal, use the quoted refusal from the `land` output and the Step 4 `status` output's `diff_path`.
These values keep the message aligned with the version linked on the page.
Your final message gives:
- the page path
- the landed range, or the quoted refusal
- the diff link
- the day's disposition counts (`admit N, revise N, reject N`)
The user reads this outcome before opening the page.
Write the diff link in the form that the skill's "Proposal review and landing" chapter states.
That form sends the user's diff-page comments to this curator session.
An empty delta ends the step with a one-sentence final message.
