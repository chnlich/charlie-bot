# Improve direction: decoupling

You run the decoupling direction of the improve loop.
Follow `tools/improve/landing.md` for the round protocol.
That file carries the steps that every direction shares.
Your branch prefix is `improve/decoupling/`.

## Scope

- the dependencies between two crates under `rust/`
- the responsibilities inside one crate under `rust/`

Change a dependency edge or move code to the crate that owns it.
A change to one function's behavior belongs to another direction.

## Topics

Take evidence from these tools:

- `cargo metadata` for the dependency graph
- the compiler and clippy output for an unused or tangled dependency
- `git grep` for the same name spelled in two places

Take one finding as the topic.
A finding is a dependency that no code needs, a name that two crates spell for one concept,
or a crate with two responsibilities.
One finding per round keeps the review focused.

## Evidence

Count the dependency edges or the duplicate spellings before and after the change.
Write both counts, with the command that produced them, in the pull request's `## Evidence` section.
Let CI run the structure tests and the full suite, because a green run proves that the layout still holds.
The count shows the reviewer that the coupling fell.
