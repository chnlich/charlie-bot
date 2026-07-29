# Memory Curation Policy

The memory store (`~/.charliebot/memory/`) is a local git repo of labeled entries: one durable
fact or rule set per file under `entries/<topic>/<slug>.md`, with front matter restricted to
`scope`, `topic`, `audience`, `title` (plus `revises` in staging only) and a pure-markdown body
with no first-line requirement. Sessions only stage candidates (`charliebot memory add` writes
to `staging/`); the canon changes only through user-approved diffs.

This file governs the daily curator AND ad-hoc user-directed promotions: the same admission test
and labeling rules apply in both flows.

## Admission test

Admission is judged at curation time, with evidence — never mid-session. Every candidate is gated
by all four tests:

1. **Future behavior** — it changes what the user or an agent does next, across tasks, runs, and
   sessions. A fact that never steers a decision does not belong in the store.
2. **Only home** — the store is the only home for it. Live state that has an authoritative home
   elsewhere is rejected: in-flight plans, loops, and jobs live in their owning systems; run
   results and receipts live in the run dir, owning session, or a results doc.
3. **Stable** — a month from now it reads unchanged and is still wanted.
4. **Cost of loss** — losing it costs real work: a correction the user already made gets repeated,
   or discovery gets redone.

An incident (a rendering bug, tool failure, environment glitch) is not evidence of a preference;
its home is `LESSONS.md` or the owning system, not the store. A measurement enters only as a
characteristic of standing infrastructure that steers method choice, not as a run outcome.

## Admission is merge-first and strict

The default action for a passing candidate is **merging into an existing entry** — a `revise`
that extends the entry whose theme already covers it, so `git diff` shows the before and after.
Creating a **NEW** entry additionally requires both:

a. **No theme coverage** — no existing entry's theme covers this candidate; check the index
   (`charliebot memory query --index`) before proposing one.
b. **Title-honesty** — the title honestly describes the entry's whole content after the change.
   A title that over- or under-states the body is a reject reason.

## Labeling — the three axes plus title

Every entry carries `scope`, `topic`, `audience`, and `title` in its front matter:

- **scope** in `user` | `host` — `user` follows the human across machines; `host` is tied to this
  machine (hostnames, local paths, hardware, internal endpoints).
- **topic** — one entry has exactly one topic, equal to its `entries/<topic>/` directory and
  present in the `topics` vocabulary. Cross-topic content belongs in a resident topic
  (`workflow`, `rulings`). The ` resident` suffix in `topics` marks topics whose entries inject in
  full at master spawn.
- **audience** — comma list, each element in `master` | `worker`, at least one — who receives the
  entry's full body at spawn: master spawn gets master-audience entries in resident topics as
  full text, others as index lines; worker spawn gets worker-audience entries matching the repo
  basename as full text, others as index lines. Both roles: `audience: master, worker`.
- **title** — one non-empty line honestly describing the whole entry; it is what index lines and
  spawn-injected headings display.

## Entry form

One coherent fact or rule set per entry. The title lives in frontmatter; the body is pure
content with no first-line requirement. Timeless phrasing: state the standing reality, not the
moment it was learned. Dates, session ids, commit hashes, quoted rulings, event history, and
case enumerations belong in `LESSONS.md`, not the store. Positively phrased, said once, in one
language, lines 120 columns or fewer. When context changes, revise the entry in place (a
`revises` candidate stages the proposed new text).

## Commit message prefixes

Curation commits use one of three prefixes so `git log` enumerates the canon's history:

- `admit: <topic>/<slug> — <title>` — a new entry promoted from staging.
- `revise: <topic>/<slug> — <title>` — an in-place edit of an existing entry (honoring a `revises`
  candidate, including merge-ins).
- `migrate: <topic>/<slug> — <title>` — a format-only rewrite to entry format v2 (moving the
  title to frontmatter, splitting `both`, dropping `created`/`source`) with no content change.
