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
  the home, recorded worker/master/loop process identities, leftover raw-log
  holders, and pending triggers' watch targets. Anything live or of unknown
  ownership is a named blocker; nothing is signalled. A server starting during
  an apply refuses to start.
- **No silent drift.** Apply refuses when the source no longer matches the
  manifest's hash binding, when the converter code changed since the manifest
  was built, or when a rebuilt plan would differ — and re-checks at the
  mutation boundary under the fence.
- **No unresolved apply.** Every uncertain item (ambiguous loop association,
  cyclic or orphaned reviews, unattributable input handling, unreadable
  records, conflicting targets or aliases, ambiguous PM prose) blocks apply
  until a human resolves it and the manifest is rebuilt.
- **Recoverable partial apply.** Every product is idempotent and receipted
  (`state/session_tree_migration/<source_sha>/receipts.ndjson`); an interrupted
  apply resumes from the same manifest in a fresh process without duplicating
  nodes, facts, or aliases. Originals are backed up and hash-verified before
  the first replacement.
- **Receipt-guarded rollback.** Rollback restores originals and removes only
  migration-owned, unchanged products. Once any product changed (a
  new-system write, an edit, added unrelated data) rollback refuses instead of
  erasing it.

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
