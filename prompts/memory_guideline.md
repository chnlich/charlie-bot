# Memory Writing Guidelines

MEMORY is a small, stable working context. It admits three kinds of entry: a preference the user stated or
repeatedly chose, a durable fact whose only home is here, and master guidance that holds whatever task is
running. Everything else has a home below.

- Put user preferences, durable facts, and cross-session master guidance in `MEMORY.md`.
- Put host-specific facts — hostnames, local paths, hardware specs, internal endpoints — in `MEMORY.host.md`.
- Put incident post-mortems, forensics, and resolved lessons in `LESSONS.md`.
- Put worker-executable procedures, project-specific guidance, and correct repo/tool usage in skills.
- Leave policy and judgment in master.md, CLI arguments in `--help`, harness behavior in code, and values in
  the source, config file, or tracker that owns them.

## Admission test

Every write clears four properties first — staging a candidate, merging one, adding an entry, repairing one:

1. **Subject** — standing reality, true across tasks, runs, plans, and incidents alike, whether one is
   finished, active, or planned.
2. **Life** — a month from now it reads unchanged and is still wanted.
3. **Home** — MEMORY is the only home for it.
4. **Effect** — the master acts differently for knowing it.

Write what clears all four and leave the rest to its home. Most user messages yield no entry. An entry that
stops clearing the test leaves MEMORY: deleted, or moved to the home that owns it.

Pointers carry content: an entry names the doc, tracker, skill, or config that owns something and leaves the
content there. Name a tool by the workflow it owns and leave its arguments in `--help`. A rule that holds
inside one situation lives with that situation.

A user preference comes from the user's own statement or repeated choice. Drop the entry when the only
evidence is an incident — a rendering bug, tool failure, or environment glitch.

## Form

One topic per section, one standing rule or fact per entry, positively phrased, said once, in one language,
lines ≤120 columns. Dates, session ids, commit hashes, quoted rulings, event history, illustrative examples,
and case enumerations belong in LESSONS.md. When context changes, revise the entry in place.

Shared repo files describe memory policy only; local memory content stays local. After content moves between
layers, run a verify worker to audit for orphans.
