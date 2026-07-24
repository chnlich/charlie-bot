# Memory Writing Guidelines

MEMORY is a small, stable working context: durable user preferences, facts, and master-agent guidance.

- Put user preferences, durable facts, and cross-session master guidance in `MEMORY.md`.
- Put incident post-mortems and resolved lessons in `LESSONS.md`.
- Put worker-executable procedures, project-specific guidance, and correct repo/tool usage in skills.
- Put host-specific facts in `MEMORY.host.md`.

Keep `MEMORY.md` organized by topic. Entries are timeless: state each entry as a standing rule or fact. Dates, session references, commit hashes, quoted rulings, event history, illustrative examples, and case enumerations go in LESSONS.md. Line width ≤120 columns. Prefer positive phrasing: state what the thing is, where it belongs, or what to do. When context changes, revise or delete the entry in place instead of appending chronological notes; provenance and incident history belong in LESSONS.md.

Shared repo files describe memory policy only; local memory content stays local.

MEMORY holds master-only taste, identity, and coordination; content in the master prompt or repo files stays there. Say it once, in one language.

When recording a tool/CLI, name it and the workflow it owns; args/flags live in
`--help`. Keep MEMORY entries out when: (1) easily re-fetched from authoritative
source; (2) project-specific (goes in a skill); (3) one-off task or incident
(LESSONS.md); (4) trivial or volatile config; (5) narrow situational caveat
dressed as a general principle; (6) project progress/status (tracker); (7) common
sense a competent engineer already applies.

A user preference must come from the user's own statement or repeated choice.
Drop the entry if the only evidence is an incident (rendering bug, tool
failure, environment glitch).

A "convention" or "fixed rule" that already lives in a config file or source
stays there; MEMORY points to the config path instead of recording the value.

Task workflows live elsewhere (LESSONS.md or session); MEMORY holds standing
rules and facts.

Host-specific facts (hostnames, local paths, hardware specs, internal
endpoints) go in MEMORY.host.md, not MEMORY.md.

## Prompt and Skill Layer Classification

Each type of content has a home:

- Policy/judgment → master.md
- CLI documentation → `--help`
- Workflow guides → a skill
- Harness behavior → code or a skill
- Source/config annotations → the source or config file

After content moves, run a verify worker to audit for orphans.
