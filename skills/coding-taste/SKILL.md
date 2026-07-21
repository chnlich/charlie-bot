---
name: coding-taste
description: Programming philosophy and code style rules for Chao's codebases. Use when writing or reviewing code, designing refactors, or delegating implementation tasks.
version: 1.0.0
---

# Coding Taste

Programming philosophy and code style rules that apply across all of Chao's
codebases. Workers should follow these when implementing; master should include
them as constraints in task specs.

## Diagnosis & Planning

- **Always diagnose before fixing.** State the identified issue clearly before
  making any code change. Silent fixes are not allowed.
- **Propose design first, don't jump to implementation.** When diagnosing a bug,
  present the analysis and proposed fix approach, then wait for user
  confirmation before implementing. The user may have a different design
  direction in mind.
- **No flip-flopping edits.** Research and understand the system first, propose
  one plan, get confirmation, then implement. If an edit turns out wrong, stop
  and discuss instead of silently reverting and trying a different approach in a
  loop.
- **Check for prior-art before designing a restructure.** Before inventing a new
  structure for a refactor, search for existing prior-art frameworks and learn
  how they organize their abstractions, rather than building from scratch.

## Implementation Philosophy

- **Code is debt, not asset; prefer concise and readable.** Take the minimal
  clean diff that solves the problem: no speculative abstractions, no dead code,
  no layers for "might need later." Less code is better; readability over
  cleverness.
- **DRY — one place for one thing.** Keep each piece of logic, rule, or config
  in exactly one place. When writing convention checks, point the checker at
  the authoritative definition rather than restating the rule and maintaining a
  second copy.
- **Don't over-specify model behavior in prompts.** No prescriptive commands,
  fixed judgment categories, or named tools. Models get stronger over time;
  rigid rules cap their judgment. Hard procedural constraints belong in the
  harness, not in agent-facing prompts. Phrase guidance as correct usage, not
  prohibition; reserve prohibition for hard invariants.

## Code Style

- **Dispatch must be explicit.** When branching on strings or options, handle
  every known case explicitly and keep the final `else` as a fail-loud
  unreachable or error path, not as a catch-all implementation branch.
- **Public API / user-entry surface stays in its own module**, separate from
  internals and build or setup files. Minimize what is exposed; expose a
  deliberate interface, not everything.
- **Organize code for the reader.** Group related state and behavior into a
  class or methods instead of scattered free functions; wrap external helpers
  behind a small in-house API. This lowers reading friction for code we write,
  distinct from "code is debt" which is about not reimplementing what already
  exists.
- **Prefer an enum over a bare boolean** for categorical or internal state.
  When defining one, reserve `0 = UNKNOWN` and start real values at `1`.
- **Persist only what can't be derived.** Store computed or derivable values
  on demand, not in the database.

## Error Handling

- **Errors must be visible, never silently swallowed.** Surface failures
  explicitly; do not catch and discard.
