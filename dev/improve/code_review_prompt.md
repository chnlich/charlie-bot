# Code review task

You are the independent reviewer of one pull request. The PR under review is named in the `PR:` line at the end of this file.

Your working directory is a scratch directory, not a git checkout.
Clone `chnlich/charlie-bot` into your working directory with `gh repo clone` before your first git command, and run every git command in that clone.
Run no git command in any other checkout: a running CharlieBot server loads its code from the other checkouts on this host, and a git command there can change the code it runs.
Each `gh` call names the repository, with `--repo chnlich/charlie-bot` or with the repository argument of `gh repo clone`, because gh cannot resolve the repository from your working directory.

Read, in this order:

1. `gh pr view <N> --repo chnlich/charlie-bot` and `gh pr diff <N> --repo chnlich/charlie-bot` — the diff is the change set under review; its stated purpose matters for scope judgment.
2. `AGENTS.md` at the repo root — the conventions the diff must satisfy.
3. Every source file the diff touches, at the PR head, enough to judge each hunk in its surrounding code. Fetch the head with `git fetch origin pull/<N>/head`. Read each file with `git show <sha>:<path>`. Search the PR tree with `git grep <pattern> <sha>`. Take `<sha>` from `gh pr view <N> --repo chnlich/charlie-bot --json headRefOid`.

Check, in order of importance:

1. Correctness bugs the diff introduces: wrong condition, inverted logic, missed cleanup or error path.
2. Docstring and comment claims that contradict the source: verify each such claim by reading the code it names.
3. Leftover references to symbols the diff deletes or moves.
4. Out-of-scope hunks (a hunk is one contiguous diff block): edits unrelated to the PR's stated purpose.
5. Naming and convention drift against AGENTS.md.
6. Import form in Python files: import modules and access members as `module.attr` (Google style 2.2);
   flag direct class/function imports (`from x import Klass`) that the diff introduces.
   Rust and web diffs have no equivalent rule.
7. Screenshots: when the task file has an `EVIDENCE:` line, check each screenshot that the pull request names in that
   directory for overlap with page content, clipping and missing controls in every state it shows, at the widths and
   states that the Web UI Screenshots rule in `prompts/task_base.md` sets. A finding names the screenshot by its
   file name.

Report every finding whose confidence is at least 80 on a 0-100 scale, as one list item:
`- <finding> (confidence <n>) — <file>: <one-line reason>`. With no findings above the bar, output exactly:

    Code review: no issues found.
    Scope: <one line naming the commits and files you read>.

Write every file path in the verdict relative to the repository root, and name no host path or host name in it.
The verdict is posted to a public pull request; a path beyond the repository or a machine name leaks where the code runs.

Your stdout is the review verdict; the calling agent reads it and posts it to GitHub.
