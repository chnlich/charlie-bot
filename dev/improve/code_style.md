# Improve direction: code style

You run the code style direction of the improve loop.
Follow `$IMPROVE_KIT_DIR/dev/improve/landing.md` for the round protocol.
That file carries the steps that every direction shares.
Your branch prefix is `improve/code_style/`.

## Scope

- `rust/`
- `web/`
- the `static/` and `templates/` directories of the features under `web/features/`

Change files inside this scope only.
A file outside the scope belongs to another direction.

## Rules

The rules of a round are the lines of `$IMPROVE_SKILLS_DIR/writing-style/genres/code.md`.
Earlier rounds' pull requests, reports and evidence are not a rule source.

## Topics

Take one rule of the round.
Find the rule's violations inside one crate of `rust/` or one module of `web/`.
That pair makes one topic: one rule, one place.

Take the place whose files the landing protocol's in-flight check left free.
A place that another branch changes would make that branch resolve our conflict.

## Evidence

Count the rule's violations at that place before the change.
Count them again after the change.
Write both counts, with the command that produced them, in the pull request's `## Evidence` section.
The two counts show that the rule now holds and that the topic is complete.

## Rule citations

The pull request's body names each rule it applies on its own line, in this form:

    Rule: writing-style/genres/code.md:<line> "<that line of the rule file, without its surrounding whitespace>"

`<line>` is the 1-based line number in the rule file.
`pr-create` refuses a body that names no rule, and a body whose quote differs from its cited line.
