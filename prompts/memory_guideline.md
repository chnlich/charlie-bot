# Memory Writing Guidelines

MEMORY is a small, stable working context. It preserves durable user preferences, facts, and master-agent guidance, not transcripts, incident narratives, or project documentation.

- Put user preferences, durable facts, and cross-session master guidance in `MEMORY.md`.
- Put incident post-mortems and resolved lessons in `LESSONS.md`.
- Put worker-executable procedures, project-specific guidance, and correct repo/tool usage in skills.
- Put host-specific facts in `MEMORY.host.md`.

Keep `MEMORY.md` organized by topic. Entries are timeless: no dates, session references, commit hashes, quoted rulings, or event history anywhere in MEMORY files; state each entry as a standing rule or fact. When context changes, revise or delete the entry in place instead of appending chronological notes; provenance and incident history belong in LESSONS.md.

Shared repo files should describe memory policy only; do not copy local memory content into the repository.
