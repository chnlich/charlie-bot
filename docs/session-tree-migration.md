# Session tree migration

`charliebot session-tree migrate` converts one CharlieBot home's legacy
sessions (schema v1) and threads to the session task tree (schema v2 tasks and
Runs). It implements approved plan 1 v4 sections 4.1/4.2 — the eleven-row
migration table — as a reviewed manifest pipeline.

## Invocation

```bash
# 1. Reviewable manifest from the current home (read-only; never mutates it)
CHARLIEBOT_HOME=/path/to/home charliebot session-tree migrate --dry-run --output manifest.json

# 2. Apply one reviewed manifest (refuses unless everything checks out)
CHARLIEBOT_HOME=/path/to/home charliebot session-tree migrate --apply --manifest manifest.json

# 3. Roll an applied manifest back (only while no new-system write happened)
CHARLIEBOT_HOME=/path/to/home charliebot session-tree migrate --rollback --manifest manifest.json
```

Exit codes: `0` success, `1` refusal/conflict (JSON diagnostic on stderr),
`2` usage. The manifest is the review artifact: it binds every conversion
input by content hash (`source_files`, `source_sha`), accounts for every
source item (`mappings` with per-item `disposition`), names what a human must
judge (`unresolved`), and after apply carries per-path `receipts` plus
`rollback_refs`. `--dry-run` on unchanged evidence is deterministic: the same
source-derived task/Run identities and the same `source_sha`.

## What apply guarantees

- **Stopped-writer boundary.** Apply takes the home's writer fence
  (`state/home_writer.lock`, an exclusive `flock`; the server holds the same
  fence for its whole lifetime) and scans `/proc` for live processes bound to
  the home, recorded worker/master/loop process identities, mixed v2 Run
  activity, leftover raw-log holders, and pending triggers' watch targets.
  The scan covers three supported bindings: an explicit `CHARLIEBOT_HOME`
  naming the home, the default-home fallback (a process without an explicit
  profile writes the default home only when its command line names a writer
  entrypoint — `server.py`, `uvicorn`, the `charliebot` CLI), and writer
  ancestors: a server or controller that launched the CLI is never excluded
  merely because it launched it, while shells, multiplexers and test runners
  that merely forwarded the environment are. An unreadable process identity is
  reported as unknown ownership, never read as death. Anything live or of
  unknown ownership is a named blocker; nothing is signalled. A server
  starting during an apply refuses to start. The live-process scan re-runs
  under the fence at the mutation boundary, so a legacy writer that appeared
  between the preflight check and fence acquisition also blocks the apply.
- **No silent drift.** Apply refuses when the source no longer matches the
  manifest's hash binding, when the converter code changed since the manifest
  was built, or when a rebuilt plan would differ — and re-checks at the
  mutation boundary under the fence. The manifest binds the whole home, not
  only the records the converter parses: a newly added record (a new v2 Run
  record, a new task node) or an unrelated file refuses apply even when every
  recorded hash still matches.
- **No unresolved apply.** Every uncertain item (ambiguous loop association,
  cyclic or orphaned reviews, unattributable input handling, unreadable
  records, conflicting targets or aliases, ambiguous PM prose) blocks apply
  until a human resolves it and the manifest is rebuilt.
- **Exact interrupted-apply recognition.** A chat log that differs from its
  manifest hash is accepted only with proof: the manifest's original bytes
  must be an intact prefix of the current file (same length, same hash), and
  every appended line must be one of this plan's facts carrying the planned
  complete content. Fields the writer mints at write time (a control event's
  `timestamp`, a `run_finished` fact's `id`) are checked by a stable rule —
  parseable UTC time not before the manifest's creation — never silently
  excluded. An unchanged id or `run_id` with altered type, outcome, input
  acknowledgements, pending-input list, summary, or evidence refs is foreign
  content and refuses with zero mutation. Changed history, an unrelated
  appended input, or a torn append refuses; no receipt is minted over any of
  them. An intact interrupted append resumes idempotently, and a landed
  terminal fact with a different outcome refuses the same way.
- **Authoritative backups.** Originals are backed up and hash-verified before
  the first replacement (atomically, so a crash never leaves a torn backup),
  the backup copy of an interrupted run is kept only when it still matches the
  manifest's pre-apply hash, and every receipted backup is re-verified on
  resume. The pre-apply bytes are authoritative; apply never replaces them
  with the file's current content.
- **Confinement.** Every source, product, state, receipt, backup, and lock
  path is confined to the selected home: a malformed `source_sha`, an absolute
  or traversal receipt/backup reference, a symlinked state/backup directory,
  or a symlinked fence path refuses before anything is written. A manifest is
  bound to the home it inventoried — a transplanted manifest refuses. The
  receipt journal is created only after every guard has passed, so a refused
  apply leaves no source-side state behind. `--dry-run --output` refuses an
  output path inside the home: the inventory must not overwrite a source or
  plant a file in its own input set.
- **Recoverable partial apply.** Every product is idempotent and receipted
  (`state/session_tree_migration/<source_sha>/receipts.ndjson`); an interrupted
  apply resumes from the same manifest in a fresh process without duplicating
  nodes, facts, or aliases.
- **Fenced, guarded rollback.** Rollback re-validates the whole protected home
  after acquiring the writer fence and before its first replacement: every
  file must still be the manifest's bound input, a receipted product, or an
  already-restored original, under the same quiescence requirements as apply.
  A writer that lands a change between the precheck and the fence acquisition
  is refused there, with every new byte preserved. A new task node, an
  unrelated file, a changed untouched source record, or a deleted file refuses
  rollback — receipt hashes of the migration's own product paths alone prove
  nothing about the rest of the home. Every backup is hash-verified before the
  first restore — a missing or corrupted backup aborts with nothing restored.
  Restores and removals journal durable per-path completion evidence
  (`rollback.ndjson`), so an interrupted rollback resumes from the same
  manifest instead of stranding a partial v1/v2 conversion.
- **Fence ownership survives failures.** A failed identity publication
  releases the lock and its fd; server startup and shutdown exceptions release
  the fence in the still-live process; a holder's release unlinks the identity
  record while it still owns the lock, so it can never erase a successor's
  identity. The identity record stays diagnostic; the flock is the exclusion.

## Input handling and delivery evidence

Historical conversion proves each disposition from the legacy producers' own
retained facts; nothing is inferred from similar text, compatible names, or
metadata alone.

**Round structure.** The old chat log carries the old system's own round
structure (the same rule its stable-history projection applies): a run-start
adoption marker — a `session_attached` event or the bare `session_id`-only
pre-typed spelling — opens one turn's interval, and the round's MASTER_DONE
closes it. A MASTER_DONE names the exact input its round consumed.

**Successful handling.** A MASTER_DONE acknowledges its input only when the
round it closes provably succeeded: `exit_code == 0` without the zero-output
flag. The old consumer emits MASTER_DONE for failed and zero-output rounds
too, and it applies the zero-output guard before writing the done, so the
done outranks the raw stream's terminal shape (the migrated manager-turn Run
inherits the round's verdict the same way). A recorded in-flight turn
(`master_run`) naming an input acknowledges it only with a provably
successful final result in its own raw log; a retained failed result is
preserved as a failed execution, not an acknowledgement.

**Run binding is a separate proof question.** An input is written into a
specific run's `input_event_ids` only through retained identity: the round
marker's backend session id must appear in that raw log's own session
adoption (top-level `session_id`/`thread_id`), and the producers' write
ordering must hold (input before launch, the marker after the launch, the
log's completion before the round's MASTER_DONE). Exactly one surviving
candidate binds; zero or several bind nothing and the input stays a
proven-but-unbound handling fact — visible in the manager mapping's
`input_disposition` summary, never invented into a run. Identical text in a
later transcript, an assistant quote, or a second request transfers nothing.

**Failed attempts.** A failed or zero-output named round imports as a failed
`manager_turn` Run and keeps the exact input it attempted. The attempted
input re-enters `pending_inputs` only while that standing failed Run blocks
fresh dispatch (the fold's own failed-turn policy); a failed round whose
execution log is missing is unresolved, because re-admitting the input there
would auto-execute it on ambiguous evidence. A later proven successful retry
confirms the input without erasing the failed attempt.

**Scheduled inputs.** The old producer admits a scheduled wake without a
named input, so a wake is proven handled only by an identity-backed launch:
the first completed unnamed round after the trigger whose bound raw log
echoes the wake text as its launch prompt. A wake whose round never started
is a proven unhandled input and enters `pending_inputs` (the trigger file
itself is moved with its status); an interrupted round or an unprovable
unnamed round leaves the wake unresolved. Old worker summaries stay
historical delivery evidence; they are never task inputs.

**Pending vs unresolved.** Only inputs the old system's own replay rule
(`unanswered_user_events`: no MASTER_DONE after them, positionally) proves
unhandled enter `pending_inputs` — a rule about log order, so missing or
unparseable input timestamps never decide a disposition. Pending entries
keep the original event id, type and timestamp; imported inputs never create
a new takeoff window. Everything else is unresolved with source references
and a precise reason, and blocks apply until a human judges it.

**Completed import evidence.** A worker imports completed only with the full
evidence set, each fact derived from the legacy source that really
establishes it: a proven successful work run (metadata outcome AND the
retained result event), a required review whose latest provably accepted
attempt succeeded (the reviewer-retry policy; earlier failed attempts stay
failed Runs, and contradictory completed metadata versus a failed retained
result cannot close), and — for implement work — the pinned result commit
landing on the actual target lineage. The commit is pinned from the run's
own retained state (the recorded worktree's HEAD when it survives, else the
recorded work branch's tip) and must be a product of the run's own execution
window (committer date within `[started_at, completed_at]`), so a branch
moved or reused after the run never passes as this run's output; landing is
then read-only `git merge-base --is-ancestor` in the recorded repo. Missing
branches, missing repos, and unpinnable results stay explicitly unproven.
The converter-written `task_closed` and `child_report` are import-time
delivery records: they carry the converter's write time, not a manufactured
historical success or receipt time; the old completion time stays on the
Run's `ended_at`, in the summary, and in the mapping detail, and the close's
`result_refs` name the pinned `landed:<branch>@<commit>` when delivery was
proven. The report is written before the node's `task_imported` boundary, so
it stays outside replay of old pending input.

**Improve iteration association.** An iteration thread belongs to a loop only
through the controller's own launch identity: the recorded repo, work
branch, and shared worktree must all match the loop's state, the loop
directory must hold the iteration's report, the thread must postdate the
loop's creation, and — when the raw log survives — the launch's own report
path must appear in it. Goal text, description patterns, compatible branch
names, and mtime coincidences are not identity. Missing evidence and
multiple plausible loops are unresolved.

## What it does not do (remaining integration boundaries)

- **No production claim.** The converter is exercised on synthetic homes only.
  The real-data offline rehearsal, the production apply, and any cutover
  decision are separate follow-up work; this CLI must not be run against
  production data until that rehearsal has happened.
- **No interactive preview.** The `session-tree preview` command (a separate
  trial instance) is a following task and does not exist here.
- **No runtime cutover behavior change.** Default creation for ordinary
  runtime sessions is unchanged; wiring migrated homes into normal startup
  behavior (e.g. scheduling of migrated cron bindings, pending-trigger
  admission policy at cutover) belongs to the next integration task.
- **No firing.** Migration never fires a trigger, never starts an execution,
  sends no external message, and reissues no old USER event; recovery reads
  the normal fold/dispatcher after import.
- **Offline copies.** References that point outside the migrated home (repo
  paths, prompt files) stay source-qualified; an offline copy never writes
  through such a path to its original. Landing evidence checks read the
  recorded repo read-only.
