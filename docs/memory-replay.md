# Memory replay

The offline curation pipeline behind the approved memory-curation redesign's
first delivery. One command turns a frozen manifest — candidate material,
current complete entries, owning-document evidence, the admission guideline,
and a pool of prior user comments with approved before/after texts — into a
complete-entry proposal bundle. It is the core the upcoming controlled
historical evaluation runs on; production approval wiring and daily scheduling
are later deliveries and are deliberately untouched.

## Invocation

```bash
charliebot memory replay \
  --input <manifest.yaml> \
  --output-dir <directory> \
  --backend <configured-id> \
  --mode editor-only|editor-review
```

A recorded `editor-review` run can then be compared against its own editor-only
alternative (below).

- `--backend` names a `backends.options` entry in the profile's `config.yaml`.
  Replay supports `charlie-code` and `cc-openai-compatible` entries (the
  content-only chat-completions transports); any other type, or an unknown id,
  fails visibly instead of routing to a substitute. Backend ids, model names,
  endpoints, and credentials stay runtime configuration — none belong in
  example files.
- `--mode editor-only` runs the editor stage only. `--mode editor-review` runs
  the reviewer over the same evidence, the same selected feedback, and the
  editor's proposed entries — never the editor's admission justifications. The
  editor request is byte-identical across the two modes, which is what makes
  editor-only a usable control for the historical comparison.
- Argument and manifest validation complete before any model call: a missing or
  malformed input exits nonzero without touching an endpoint.

The pipeline lives in `src/core/memory_replay/` (manifest, retrieval, exchange,
validation, transport, report, runner); `src/cli/memory.py` only parses
arguments and prints the outcome.

## Manifest format

Schema v1, YAML, documented in full by the module docstring of
`src/core/memory_replay/manifest.py` and by the complete synthetic fixture at
[`examples/memory-replay-manifest.yaml`](examples/memory-replay-manifest.yaml).
Sections:

- `base_commit` — an opaque label of the frozen official-memory base revision
  the diff is computed against. Replay never verifies it against the live
  store; the later approval integration re-binds it at approval time.
- `topics` — the frozen topics vocabulary. A proposed entry outside it fails
  mechanical validation, so a replay cannot grow the vocabulary.
- `sources` — the frozen evidence, one entry each with a unique `ref` and a
  `kind`:
  - `candidate`: staged capture material awaiting curation;
  - `entry`: a current complete entry; `path` (shape
    `entries/<topic>/<slug>.md`) is its store path and the diff path;
  - `document`: evidence from an owning document;
  - `guideline`: the admission policy; global, sent to both stages.

  Text is inline (`text:`) or an external frozen file (`file:`, relative to the
  manifest). Unknown keys are rejected, so evaluation metadata cannot ride
  inside the manifest; keep scoring answers and holdout labels in separate
  files the runner never reads.
- `feedback_examples` — the pool of prior user comments: `comment_event`
  (opaque provenance id), the original `comment_text`, optional retrieval
  `tags`, and an optional `approved_change` carrying `approved_change_ref`,
  `before`, and `after` texts. An approved **deletion** has a nonempty `before`
  and an empty `after`; an approved **creation** the reverse; an ordinary
  revision has both nonempty. A pair with both sides empty, or with identical
  `before` and `after`, is rejected — an approved change that changes nothing is
  not feedback. The actual texts (the empty side included) are preserved
  verbatim through loading, rendering, retrieval, and input identity; the empty
  side of an approved change renders in the model requests as an explicit
  "(empty: the approved revision deleted/created this text)" marker, never as
  invented placeholder prose. Tags are a rebuildable index over the user's own
  words, never user rules; original comments and provenance always travel
  intact.
- `themes` (optional) — explicit assignments for reproducible replay: each
  theme lists its `candidate_refs` (every candidate must be assigned exactly
  once), plus the `entry_refs` and `document_refs` that form its context.
  Without the block, one implicit theme `default` groups everything.

Relevance selection over the feedback pool is deliberately simple and
inspectable (`src/core/memory_replay/retrieval.py`): a comment is selected when
its tags match the theme's declared principles or its words overlap the theme's
texts — the principle axis is what makes an otherwise matching correction
survive a project rename. The selection with its matched tags and terms is
recorded in `run.json`.

## What the models see and may do

Each stage gets one content-only chat-completions request: every byte of
evidence is inline, no tools are offered, and the reply is one JSON object. A
model therefore has no read path beyond the supplied evidence and no write path
at all — the isolation is a property of the transport, not of an instruction
saying "do not write".

The request states the frozen topics vocabulary (`## Allowed topics`) and shows
each current entry's store path together with its `ref` — a model cannot honor
vocabulary it never sees.

- **Editor** returns complete proposed entries (rewrite with full replacement
  text, delete, keep, or new) plus one disposition row per candidate
  (`propose`, `no_change`, `needs_decision`). An explicit remember request
  keeps a visible row naming it.
- **Reviewer** (editor-review mode only) sees the same evidence, the same
  selected feedback, and the editor's proposed entries. It may keep, delete, or
  rewrite, and its rows become the final dispositions; a change it initiates
  gets a row whose `source_ref` is the touched entry's `ref`, with the store
  path kept separately in the row's `paths`. The editor's reasons stay in the
  audit record and are withheld from the reviewer request.

## Mechanical validation and the proposal

After the stages, deterministic code (`src/core/memory_replay/validate.py`)
checks entry formats through the store's own parser, topic vocabulary
membership, source-ref resolution, path shapes (no traversal), disposition
coverage, and that the generated diff round-trips under a strict unified-diff
applicator: hunks land exactly where their headers say, in order, without
overlap, and the unchanged text around them — prefix, between hunks, and the
trailing suffix — survives, so the applied state is the complete file. Then it
writes the bundle:

```
<output-dir>/runs/<input-identity prefix>/
  proposal.json        # exactly the plan 4.1 schema
  report.html          # final diff and dispositions first, evidence folded
  sources/<ref>.md     # frozen evidence snapshots (sha256 in proposal.json)
  frozen/manifest.yaml # the fully inlined frozen inputs (self-contained)
  run.json             # identity, model identity, selection, usage, timing, status,
                       # system-prompt fingerprints, bundle write-time hashes
  raw/                 # the exact request and response text of every model call
```

The frozen inputs are written before the first model call, so failed runs are
self-contained too. `run.json` records the SHA-256 of every bundle file at the
moment the run wrote it (`bundle_integrity`) and fingerprints of the exact
system prompts sent to each stage (`system_prompts`); a later comparison fails
visibly when a recorded file or the prompt contract changed.

`proposal.json` carries exactly the schema fields: `base_commit`, `sources`
(`ref`/`sha256`/`snapshot`), `feedback_refs`
(`comment_event`/`approved_change_ref`, the latter nullable),
`candidate_results` (`source_ref`/`outcome`/`paths`/`reason` — every
`source_ref` is a ref from `sources`, a candidate's or an existing entry's,
never a store path),
`reviewed_patch` (unified diff against the base), and `approval_digest` —
SHA-256 over the canonical JSON encoding of `base_commit` and
`reviewed_patch` jointly. Every changed path maps back through a `propose`
disposition to evidence, reviewer-initiated changes included.

Run records (`run.json`, `raw/`) hold the raw model outputs, the selected
feedback references, the input identity, the model identity, and whatever
usage the endpoint reported (output tokens and latency; cost only when
actually known). They are local artifacts for the later evaluation — keep the
output directory out of git.

## Reuse and exit codes

A completed run is identified by its inputs: every source, the whole feedback
pool, theme assignments, the mode, the model identity, and the prompt versions.
Re-running identical inputs reuses the completed bundle without a model call;
new relevant feedback, rule, or document evidence changes the identity and
invalidates reuse. There is no workflow state machine — the bundles themselves
are the state.

Exit status reflects execution only: `0` for a completed or reused proposal
(including one whose dispositions say `needs_decision`), `1` for a load,
transport, parse, or mechanical-validation failure. A failed run leaves its
directory with `run.json` `status: "failed"` and no proposal; the next identical
run redoes it. Model judgments never fail the command.

## Paired comparison of one recorded run

`charliebot memory compare --run-dir <editor-review-run> --output-dir <dir>`
is the offline, deterministic second half of the evaluation. It re-derives
BOTH arms of an `editor-review` run from the run's own recorded outputs — the
editor-only arm from the exact editor response the reviewer consumed, the
post-review arm from the recorded reviewer responses — with **no model calls
and no live memory writes**. Comparing a fresh editor-only invocation against
a recorded review would confound review gain with a new stochastic editor
draw; this comparison cannot, because both arms parse the same recorded
response.

Before reporting anything, the comparison proves its inputs are the recorded
run's inputs and fails visibly otherwise:

- the frozen manifest copy inside the run bundle (or, for runs recorded before
  self-contained bundles existed, the manifest at the path the run record
  names) must reproduce the run's recorded input identity;
- every bundle file hashed at run time must still match that hash;
- each recorded editor request must be byte-identical to the request
  reconstructed from the frozen inputs and the recorded feedback selection,
  and each recorded reviewer request byte-identical to the request
  reconstructed from the frozen inputs and the recorded editor response — the
  proof that the reviewer actually consumed that response;
- the recorded system-prompt fingerprints must match the current replay
  prompts;
- a recorded proposal must still match the finalization of the recorded
  reviewer responses (base, sources, patch, approval digest, final
  dispositions).

`comparison.json` and `report.html` are self-contained. They carry the source
run identity and base, explicit provenance that both arms share one recorded
editor response (with per-theme request/response hashes), the verification
results, per-theme full final texts, diffs, and candidate dispositions for
both arms (maintenance changes included), fixed theme and input-candidate
denominators with `no_change` and `needs_decision` rows visible, editor and
reviewer output token counts and elapsed request times exactly as recorded
(missing values `null`; the incremental review cost is the review calls
only), and any execution/parse/validation failure. A failed arm is reported
as failed — never substituted with empty or no-change output — and stays in
the denominators.

Quality is reported as **unjudged**: fewer lines, more deletions, or fewer
proposed paths do not establish better quality. The exit status communicates
mechanical execution and format success only, never whether review improved
quality. Runs recorded before this provenance metadata existed are supported
when every check their record can support passes, and the comparison
explicitly declares what such a record never saved instead of silently
certifying it. The comparison writes only into its own output root, which
must not overlap the run directory or the live store — its inputs are never
modified.

## Isolation guarantees

- The live memory store is never opened for reading or writing: the manifest
  supplies the frozen store state, and nothing in the pipeline touches
  `cfg.memory_dir`.
- The output root must not overlap the live store or any frozen input file
  (either direction of containment); violations are rejected before any write.
- Proposed entry paths must match `entries/<topic>/<slug>.md` in the store's
  charsets, which rules out traversal; new entries must use a declared topic.

## Out of scope in this delivery

Live curator prompts, daily scheduling, the production approval/commit
endpoints, and any claim of measured quality improvement are all later work.
The private historical corpus and the baseline-versus-new-design comparison
belong to the master's evaluation step; synthetic tests here assert only the
observable boundaries of the core.
