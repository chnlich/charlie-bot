---
name: llm-context-guideline
description: Placement and admission policy for content that enters LLM context;
  its chapters currently govern the memory store (admission whitelist, the
  three proof questions, entry form and labeling, canon change rules,
  model-context phrasing, the proposal review and landing flow). Reload in
  full before touching memory content.
user-invocable: false
---

# Memory Curation Policy

The store manages session context, and context is capability: a model reasons worse as its
window fills, so curation optimizes for the smallest store whose entries still change future
actions. Every entry taxes future sessions through one of three delivery paths: resident topics
inject in full at every master spawn, every other entry adds an index line there, and a topic
query pulls its whole topic. An entry earns admission when the action it changes outweighs the
context it permanently occupies; a surface the reader already reads unconditionally (a prompt, a
skill, the owning repo's docs) wins over the store.

The memory store (`~/.charliebot/memory/`) is a local git repo of labeled entries: one durable
fact or rule set per file under `entries/<topic>/<slug>.md`, with front matter restricted to
`scope`, `topic`, `audience`, `title` and a pure-markdown body.
Sessions only stage candidates (`charliebot memory add` writes to `staging/`): a staging
candidate is a free-form capture whose labels are assigned at curation and whose change intent
lives in the body; the canon changes only by landing a user-approved proposal version with
`charliebot memory proposal land <sha>`.

This file governs the daily curator AND ad-hoc user-directed promotions: the same admission test
and labeling rules apply in both flows.

## Phrasing for model context

Text that enters model context follows the "Text a model reads" subsection of the Writing
Style section in prompts/master.md.

## Admission test

Admission is judged at curation time, with evidence, over staged candidates. The store admits three
kinds of entry, and only these:

1. A ruling or preference the user stated, when a capable model does not follow it by default.
2. A mechanism or fact whose rediscovery would cost a real investigation and that still reads
   true a month from now.
3. A host, cluster, or account level pointer that cannot be guessed and has no owning document.

Everything else stays out by default; when in doubt, reject the candidate and name it in the PR
page. The model itself is the canonical home for general engineering and statistical
reasoning. An entry records what holds across models, fixes, and runs: rules, mechanisms, and
contracts. Entries hold durable mechanisms and standing policies: execution logs and `LESSONS.md`
retain ephemeral identifiers, transient instance names, and active incident telemetry; repository
guides and skills retain documentation and reference procedures, with the store carrying at most
a pointer line; and runtime inspection resolves on-demand system facts.

State each fact at the category level in its most concise form, bounded to one to three lines of
core conclusion, keeping narrative deduction and case histories in session records. Phrase each
line by the `## Phrasing for model context` rule above.

Every admit and every revise carries three proof lines in the PR page and its commit body, each
headed by the question it answers; a question that finds no answer is the signal to rethink
whether the entry belongs in the store at all, and such a candidate is rejected:

- **"When will this be used again, and what will it change?"** (the Action line): the concrete
  future action this entry changes, named as work that recurs or is already planned in a named
  project or stack.
- **"Why is the store the cheapest home?"** (the Home line): answered by checking the others:
  repo-scoped knowledge lives in that repo's own CLAUDE.md or docs; charlie-bot behavior lives
  in the master prompt, a skill, config, or the source; incidents and event history live in
  `LESSONS.md`; run results, live state, and receipts live in the run dir or owning session; a
  project's experiment verdicts live in its tracker (Linear).
  A project-scoped finding lives under that project's topic or its repo docs; a cluster or host
  entry holds only what binds every project there. The Home line also names the reader and the
  delivery path that reaches them at the moment the entry changes their action, and the named
  path is one the reader already travels.
  Residency is the costliest slot, full text in every master spawn: process rules governing every
  session hold it; domain conventions live with their domain. When the natural home is obstructed,
  fix the obstruction or take another tracked path inside that home; another home's content stays
  in that home.
- **"Is this the most concise expression?"** (the Brevity line): the curator trims the
  presented text (the new body, or the whole entry after a merge) to the Entry form brevity
  bar before presenting, and the answer names what the trim removed, or states the body's
  line count when the draft already sat at the bar.

A candidate's text is a claim: verify its figures against the live system before presenting them.
A revise re-verifies the surviving claims of the entry it edits.

A trap claim about shared infrastructure enters only with its root cause named and reproduced
outside the originating session; a fixable obstruction is fixed instead of recorded.

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
reference files) live in the owning skill: reject procedure-shaped candidates and name the
owning skill in the PR page. Live state under active investigation stays with its owning
system. One home per item: when a rule is admitted to the store, the store entry replaces the
skill's copy of it.

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
quoted rulings, event history, and case enumerations belong in `LESSONS.md`.
Said once, in Chinese, lines 120 columns or fewer; code, paths, identifiers, and commands stay
verbatim. Entry prose follows the Writing Style section of prompts/master.md, its "Text a model
reads" subsection included. Apply the
admission test line by line as well as entry by entry. Keep each line whose removal changes what a
capable model does.
Brevity is part of the admission bar: lead with the action and keep the mechanism the action
is unintelligible without; session reports and run dirs hold the receipts, verification notes,
and secondary effects. Hold a
bullet to about three lines and an entry body to about a dozen; a merge that would grow past that
re-trims the whole entry by the same test.
A measured figure lives in its canonical source (run dir, canon table, ticket); an entry states
the rule and points there.
Environment composition and version facts (package pins, toolchain and interpreter versions,
build and model numbers) live in the manifest or config that pins them: the entry states the
rule and points there.
Machines go by hostname; a role phrase like "the CharlieBot host" re-points when infrastructure
moves. When context changes, revise the entry in place (a capture whose body states the change
stages the proposed new text).
A retired or archived system's content leaves the store outright, entries and lines alike: the
store states standing reality, and `LESSONS.md` holds the retirement event when it matters.

## Commit message prefixes

Curation commits use one of five prefixes so `git log` enumerates the canon's history:

- `admit: <topic>/<slug> (<title>)`: a new entry promoted from staging.
- `revise: <topic>/<slug> (<title>)`: an in-place edit of an existing entry (honoring a `revises`
  candidate, including merge-ins).
- `migrate: <topic>/<slug> (<title>)`: a format-only rewrite to entry format v2 (moving the
  title to frontmatter, splitting `both`, dropping `created`/`source`) with no content change.
- `remove: <topic>/<slug> (<title>)`: an entry's removal from the store.
- `scaffold: <description>`: a `topics` vocabulary change, adding or retiring a topic.

A commit body carries the entry's three proof lines, each headed by the question it answers,
plus one `Staging: <file>` line per staging candidate the commit consumed.

## Proposal review and landing

The day's curation reaches the user as one pull request: the `proposal` branch of the memory
repo, drafted in the git worktree `~/.charliebot/memory-proposal/`, where the selector writes
the day's entry and `topics` edits uncommitted and the reviewer commits them entry by entry
through `charliebot memory proposal commit <store-relative path> --message-file F`.
The user reviews the PR at a pinned version through the diff page (`diff_path`) and the PR page,
which lists every PR commit with its proof lines, the reviewer's rewrites of the day, the
conflicts between new candidates and existing PR lines, and the candidates rejected since the
PR opened.
On the user's approval of version `<sha>`, the session that received the approval runs
`charliebot memory proposal land <sha>`, which fast-forwards the live store
`~/.charliebot/memory/` to exactly that version.
On partial approval, remove the unwanted commits from the PR first, re-link the new version in
the reply, and collect the user's approval of that version.
`charliebot memory proposal open` rebases the PR onto the live base when the base moved, and a
rebase voids earlier SHAs, so the session re-links the current head after every `open`.
A diff-page comment fix is a delegated worker's edit: the worker edits only the commented lines
in the PR worktree and commits one entry per commit.
Every adjudication round ends by re-linking the current version in the reply.
Each user comment on a proposal is evidence of a rule gap. After applying the comment, check
whether the rules above would have kept the commented content out of the store on their own; a
revealed gap gets its one-line amendment in the same reply, and the amendment lands through the
normal repo change flow after approval.
A change the user approves inside a session commits directly to the live store's base branch,
and the next `open` rebases the PR onto it.
