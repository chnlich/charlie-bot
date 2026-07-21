# Memory Writing Guidelines

MEMORY is a small, stable working context. It preserves durable user preferences, facts, and master-agent guidance, not transcripts, incident narratives, or project documentation.

- Put user preferences, durable facts, and cross-session master guidance in `MEMORY.md`.
- Put incident post-mortems and resolved lessons in `LESSONS.md`.
- Put worker-executable procedures, project-specific guidance, and correct repo/tool usage in skills.
- Put host-specific facts in `MEMORY.host.md`.

Keep `MEMORY.md` organized by topic. Entries are timeless: no dates, session references, commit hashes, quoted rulings, or event history anywhere in MEMORY files; state each entry as a standing rule or fact. No illustrative examples or case enumerations; line width ≤120 columns. Prefer positive phrasing (state what to do) over negative (state what to avoid). When context changes, revise or delete the entry in place instead of appending chronological notes; provenance and incident history belong in LESSONS.md.

Shared repo files should describe memory policy only; do not copy local memory content into the repository.

Don't duplicate content already in the master prompt or repo files; MEMORY holds master-only taste, identity, and coordination, not rules re-stated from elsewhere. Don't restate the same content bilingually; say it once.

When recording a tool/CLI, name it and the workflow it owns; args/flags live in
`--help`. Keep MEMORY entries out when: (1) easily re-fetched from authoritative
source; (2) project-specific (goes in a skill); (3) one-off task or incident
(LESSONS.md); (4) trivial or volatile config; (5) narrow situational caveat
dressed as a general principle; (6) project progress/status (tracker); (7) common
sense a competent engineer already applies.
