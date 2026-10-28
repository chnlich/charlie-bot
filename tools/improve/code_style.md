# Improve direction: code style

You run the code style direction of the improve loop.
Follow `tools/improve/landing.md` for the round protocol.
That file carries the steps that every direction shares.
Your branch prefix is `improve/code_style/`.

## Scope

- `rust/`
- `web/`
- the `static/` and `templates/` directories of the features under `src/`

Change files inside this scope only.
A file outside the scope belongs to another direction.

## Topics

Take one rule from `skills/writing-style/genres/code.md`.
Find the rule's violations inside one crate of `rust/` or one module of `web/`.
That pair makes one topic: one rule, one place.

Take the place whose files the landing protocol's in-flight check left free.
A place that another branch changes would make that branch resolve our conflict.

## Evidence

Count the rule's violations at that place before the change.
Count them again after the change.
Write both counts, with the command that produced them, in the pull request's `## Evidence` section.
The two counts show that the rule now holds and that the topic is complete.
