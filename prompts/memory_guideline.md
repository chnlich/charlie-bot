# Memory Curation Policy

The memory store (`~/.charliebot/memory/`) is a local git repo of labeled entries: one durable
fact or rule set per file under `entries/<topic>/<slug>.md`, with front matter restricted to
`scope`, `topic`, `audience`, `title` (plus `revises` in staging only) and a pure-markdown body.
Sessions only stage candidates (`charliebot memory add` writes to `staging/`); the canon
changes only through user-approved diffs.

This file governs the daily curator AND ad-hoc user-directed promotions: the same admission test
and labeling rules apply in both flows.

## Admission test

Admission is judged at curation time, with evidence, never mid-session. The store admits three
kinds of entry, and only these:

1. A ruling or preference the user stated.
2. A mechanism or fact whose rediscovery would cost a real investigation and that still reads
   true a month from now.
3. A host, cluster, or account level pointer that cannot be guessed and has no owning document.

Everything else stays out by default; when in doubt, reject and name the candidate in the
report.

Every admit and every revise carries two proof lines in the report, and a candidate whose
Action or Home line cannot be written is rejected:

- **Action**: the concrete future action this entry changes, named as work that recurs or is
  already planned in a named project or stack; a constructed possibility fails the line, and a
  version- or build-specific fact is scoped to the stack that pins it.
- **Home**: why the store is the only home, checked against the others: repo-scoped knowledge
  lives in that repo's own CLAUDE.md or docs; charlie-bot behavior lives in the master prompt,
  a skill, config, or the source; incidents and event history live in `LESSONS.md`; run
  results, live state, and receipts live in the run dir or owning session. A project-scoped
  finding lives under that project's topic or its repo docs; a cluster or host entry holds
  only what binds every project there.

A candidate's text is a claim: verify version, build, and account figures against the live
system before presenting them.

Data cheap to re-obtain on demand lives in session reports and run dirs; the store keeps the
takeaway that tells the reader where to look.

## Admission is merge-first and strict

The default action for a passing candidate is **merging into an existing entry**, a `revise`
that extends the entry whose theme already covers it, so `git diff` shows the before and after.
Creating a **NEW** entry additionally requires both:

a. **No theme coverage**: no existing entry's theme covers this candidate; check the index
   (`charliebot memory query --index`) before proposing one.
b. **Title-honesty**: the title honestly describes the entry's whole content after the change.
   A title that over- or under-states the body is a reject reason.

## The skills boundary

The store holds decision knowledge: facts, rules, and conventions that change what the
reader does next. Step-by-step operating procedures (command sequences, recipes, and their
reference files) live in the owning skill, never the store: reject procedure-shaped
candidates and name the owning skill in the report. Live state under active investigation
stays with its owning system. One home per item: when a rule is admitted to the store, no
skill keeps a duplicate of it.

## Labeling: the three axes plus title

Every entry carries `scope`, `topic`, `audience`, and `title` in its front matter:

- **scope** in `user` | `host`: `user` follows the human across machines; `host` is tied to this
  machine (hostnames, local paths, hardware, internal endpoints).
- **topic**: one entry has exactly one topic, equal to its `entries/<topic>/` directory and
  present in the `topics` vocabulary. The `topics` vocabulary grows only by user ruling.
  Cross-topic content belongs in a resident topic
  (`workflow`, `rulings`). The ` resident` suffix in `topics` marks topics whose entries inject in
  full at master spawn. Reads are topic-granularity: a query returns the whole topic (audience-filtered), and agents see
  topics only, split entries for curation and audience separation, with the topic as the sole retrieval unit.
- **audience**: comma list, each element in `master` | `worker`, at least one, who receives the
  entry's full body at spawn: master spawn gets master-audience entries in resident topics as
  full text, others as index lines; worker spawn gets worker-audience entries matching the repo
  basename as full text, others as index lines. Both roles: `audience: master, worker`.
- **title**: one non-empty line honestly describing the whole entry; it is what index lines and
  spawn-injected headings display.

## Entry form

One coherent fact or rule set per entry. The title lives in frontmatter; the body is pure
content. Timeless phrasing: state the standing reality. Dates, session ids, commit hashes,
quoted rulings, event history, and case enumerations belong in `LESSONS.md`. Said once, in one
language, lines 120 columns or fewer. Entry prose follows the writing-style skill. Apply the
admission test line by line as well as entry by entry: a line that changes no future action
leaves.
Machines go by hostname; a role phrase like "the CharlieBot host" re-points when infrastructure
moves. When context changes, revise the entry in place (a `revises` candidate stages the proposed
new text).

## Commit message prefixes

Curation commits use one of three prefixes so `git log` enumerates the canon's history:

- `admit: <topic>/<slug> (<title>)`: a new entry promoted from staging.
- `revise: <topic>/<slug> (<title>)`: an in-place edit of an existing entry (honoring a `revises`
  candidate, including merge-ins).
- `migrate: <topic>/<slug> (<title>)`: a format-only rewrite to entry format v2 (moving the
  title to frontmatter, splitting `both`, dropping `created`/`source`) with no content change.
