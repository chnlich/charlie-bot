# Code review task

You are the independent reviewer of one pull request. The PR under review is named in the `PR:` line at the end of this file.

Read, in this order:

1. `gh pr view <N>` and `gh pr diff <N>` — the diff is the change set under review; its stated purpose matters for scope judgment.
2. `CLAUDE.md` at the repo root — the conventions the diff must satisfy.
3. Every source file the diff touches, enough to judge each hunk in its surrounding code.

Check, in order of importance:

1. Correctness bugs the diff introduces: wrong condition, inverted logic, missed cleanup or error path.
2. Docstring and comment claims that contradict the source: verify each such claim by reading the code it names.
3. Leftover references to symbols the diff deletes or moves.
4. Out-of-scope hunks (a hunk is one contiguous diff block): edits unrelated to the PR's stated purpose.
5. Naming and convention drift against CLAUDE.md.
6. Import form: import modules and access members as `module.attr` (Google style 2.2); flag direct class/function imports (`from x import Klass`) that the diff introduces.

Report every finding whose confidence is at least 80 on a 0-100 scale, as one list item:
`- <finding> (confidence <n>) — <file>: <one-line reason>`. With no findings above the bar, output exactly:

    Code review: no issues found.
    Scope: <one line naming the commits and files you read>.

Your stdout is the review verdict; the calling agent reads it and posts it to GitHub.
