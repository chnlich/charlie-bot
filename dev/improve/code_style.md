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
A file outside the scope belongs to another direction, except a reference that landing.md's moved-code rule covers.
A change to a web page takes its screenshots and serve trial from landing.md's Web page evidence section.

## Rules

The rules of a round are Google's style guide for the language of the files it changes.
The kit pins that text in `$IMPROVE_KIT_DIR/dev/improve/google-styleguide/`, a copy of google/styleguide at the
revision its `REVISION` file names.
The pinned guide alone is the round's rule source, and a guide revision changes only by a kit change.

- `.js` and `.mjs` files: `google-styleguide/jsguide.md`
- `.html` and `.css` files: `google-styleguide/htmlcssguide.md`
- `.sh` files: `google-styleguide/shellguide.md`

A file whose language has no guide in that list gets no topic.
Rust files get no code-style topic: rustfmt and clippy hold the repository's Rust code in CI, and the pinned set
carries no Rust guide. `rust/` stays in the Scope for the direction's file ownership.
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

    Rule: google-styleguide/<guide file>:<first line>[-<last line>] "<the exact words of those lines>"

`<guide file>` is one of the files in the Rules list.
`<first line>` and `<last line>` are 1-based line numbers in that pinned file.
The words are the cited lines joined with one space, with each run of whitespace collapsed to one space and both ends
trimmed.
`pr-create` refuses a body that names no rule, and a body whose quote differs from the cited lines.
