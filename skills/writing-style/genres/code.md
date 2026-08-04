# Code

Programming philosophy and code style rules. Workers follow these when
implementing; master includes them as constraints in task specs.

## Diagnosis and Planning

- State the identified issue clearly before making any code change.
- Propose design first when diagnosing a bug; present analysis and fix
  approach, then wait for confirmation before implementing.
- Research and understand the system first, propose one plan, get
  confirmation, then implement. If an edit turns out wrong, stop and discuss
  instead of silently reverting and trying a different approach in a loop.
- Search for existing prior-art frameworks before inventing a new structure
  for a refactor; learn how they organize their abstractions.

## Implementation Philosophy

- Take the minimal clean diff that solves the problem. Skip speculative
  abstractions, dead code, and layers for "might need later." Less code is
  better; readability over cleverness.
- Keep each piece of logic, rule, or config in exactly one place. When
  writing convention checks, point the checker at the authoritative
  definition rather than restating the rule and maintaining a second copy.
- Phrase guidance as correct usage; reserve prohibition for hard invariants.
  Keep prescriptive commands, fixed judgment categories, and named tools out
  of agent-facing prompts; hard procedural constraints belong in the harness.

## Code Style

- When branching on strings or options, handle every known case explicitly
  and keep the final `else` as a fail-loud unreachable or error path.
- Keep the public API or user-entry surface in its own module, separate from
  internals and build or setup files. Expose a deliberate interface.
- Group related state and behavior into a class or methods instead of
  scattered free functions; wrap external helpers behind a small in-house API.
- Prefer an enum over a bare boolean for categorical or internal state.
  Reserve `0 = UNKNOWN` and start real values at `1`.
- Store computed or derivable values on demand.

## Comments

- Give the reason the code takes this form: the constraint that rules
  out the simpler alternative, the cost that dictates the shape, the
  invariant a reader cannot see. The code already states what it does.
- Name the outside fact the reason rests on, concretely enough for a
  reader to re-check whether it still holds: the library behavior, the
  version boundary, the measured cost.

## Error Handling

- Surface failures explicitly; catch and report, skip and discard.
