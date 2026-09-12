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
alternative (below). The fixed-input variant experiment — one command that runs
the named curation variants over several frozen cases — is documented in
[The fixed-input variant experiment](#the-fixed-input-variant-experiment).

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

## The exchange contract (v3)

Each stage gets one content-only chat-completions request: no tools are
offered, and the reply is exactly one JSON object. A model therefore has no
read path beyond the supplied evidence and no write path at all — the isolation
is a property of the transport, not of an instruction saying "do not write".

**Evidence is one JSON payload.** After the theme name, the user content
carries a single JSON object under `## Evidence` holding every byte of frozen
input: `guidelines` (the admission policy), `entries` (the current base
entries, each with its store `path` and `ref`), `documents`, `candidates`,
`feedback` (the selected prior user comments with provenance ids), the
`allowed_topics` vocabulary, and the finite `disposition_refs` domain. Every
text is an exact JSON string, so arbitrary entry or capture content — headings,
JSON fragments, anything — stays data and cannot be confused with request
structure; there are no prose sections around evidence and no second rendering
of it.

**Every operation is relative to the original frozen base.** `keep` retains the
base entry and carries no text — it never expresses different content;
`rewrite` replaces it with the complete text; `delete` removes it; `new` adds
an absent path in a declared topic. A reviewer that accepts an editor's
new/rewrite proposal re-emits that operation with the complete text it
approves — never `keep` with changed text — and drops an editor-created entry
by omitting the operation and updating the candidate's final disposition.

**Disposition rows name only the finite ref domain.** Exactly one row per
candidate ref (`disposition_refs.candidates`); the only other allowed
`source_ref` is a base entry's ref (`disposition_refs.entries`) for a change
the stage initiates on its own. Feedback comment ids and approved-change refs
are evidence provenance — citable in `source_refs`, never disposition rows.
`new`/`rewrite` rows must cite supporting evidence; `keep`/`delete` rows may
carry optional citations, which are validated against the actually available
evidence, so their presence alone never rejects a no-write operation — but an
unknown ref still fails.

- **Editor** returns complete proposed entries plus one disposition row per
  candidate (`propose`, `no_change`, `needs_decision`). An explicit remember
  request keeps a visible row naming it.
- **Reviewer** (editor-review mode only) sees the same evidence, the same
  selected feedback, and the editor's proposed operations
  (`evidence.editor_proposals`) — never the editor's justifications, which stay
  in the audit record. Its rows become the final dispositions; a change it
  initiates gets a row whose `source_ref` is the touched entry's `ref`, with
  the store path kept separately in the row's `paths`.

The frozen guideline inside the evidence remains the authoritative admission
policy; nothing in the request adds rules beyond it. Scoring answers have no
channel into either request.

## Bounded mechanical repair

A stage response that fails mechanical validation — JSON shape, unknown ref,
path or entry-format violation, disposition-coverage gap, or a
patch/disposition inconsistency in the stage's own final state — is re-asked
**at most once** (two model responses maximum). The re-ask's input is the
original authorized evidence, that stage's own previous raw response, and the
concrete validation errors; the reviewer's repair input never contains the
editor's rationale. Model judgments (`no_change`, `needs_decision`, rejecting
every candidate) are never retry triggers, and backend, auth, and transport
failures are never retried — they fail the run immediately. A second invalid
response fails visibly: nothing is coerced into a valid output, no real
candidate is discarded, and no failed response becomes `no_change`. Cross-theme
conflicts (two themes driving one path) cannot appear inside a stage and stay
visible at final assembly.

## Mechanical validation and the proposal

After the stages, deterministic code (`src/core/memory_replay/validate.py`)
checks entry formats through the store's own parser, topic vocabulary
membership, source-ref resolution (optional keep/delete citations included),
path shapes (no traversal), disposition coverage, and that each stage's own
final state agrees with its propose rows — a changed path with no propose row
has no evidence mapping, and a propose row over an unchanged path points at
nothing; both are attributed to the stage that made them, where the bounded
repair can reach them. Final assembly additionally rejects cross-theme
conflicts, and the generated diff must round-trip under a strict unified-diff
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
  run.json             # identity, model identity, selection, status, prompt versions,
                       # system-prompt fingerprints, bundle write-time hashes, the recovery
                       # policy, and every recorded attempt with its validation outcome,
                       # chosen flag, and usage
  raw/                 # the exact request and response text of every model attempt,
                       # named <role>-<theme>.attempt-<n>.{request,response}.txt
```

The frozen inputs are written before the first model call, so failed runs are
self-contained too. Every attempt's request, response, mechanical validation
result, and available usage is persisted — failed repair attempts included,
with unknown usage recorded as null. `run.json` records the SHA-256 of every
bundle file at the moment the run wrote it (`bundle_integrity`; the record
itself and the derived report are excluded) and fingerprints of the exact
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

Run records (`run.json`, `raw/`) hold every attempt's raw request and
response, the validation outcome and chosen flag, the selected feedback
references, the input identity, the model identity, and whatever usage the
endpoint reported (output tokens and latency; cost only when actually known,
null when unknown). They are local artifacts for the later evaluation — keep
the output directory out of git.

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
editor-only arm from the exact chosen editor response the reviewer consumed,
the post-review arm from the recorded chosen reviewer responses — with **no
model calls and no live memory writes**. Comparing a fresh editor-only
invocation against a recorded review would confound review gain with a new
stochastic editor draw; this comparison cannot, because both arms parse the
same recorded response.

The run's recorded prompt versions select the exchange contract used for
every check: v3 runs are read under the v3 contract, v2 runs under the v2
contract with its original operation and validation meanings (a v2 run's known
failed arms stay failed — v3's wider citation allowance does not reach back
into old records), and anything else fails explicitly.

Before reporting anything, the comparison proves its inputs are the recorded
run's inputs and fails visibly otherwise:

- the frozen manifest copy inside the run bundle (or, for runs recorded before
  self-contained bundles existed, the manifest at the path the run record
  names) must reproduce the run's recorded input identity;
- every bundle file hashed at run time must still match that hash;
- the recorded attempt chain of every stage must reconstruct: attempt 1's
  request must be the request the recorded contract builds, a repair attempt's
  request must be the bounded repair request over the same evidence, the
  previous recorded response, and that attempt's recorded validation errors,
  the chosen attempt must be the last one, and a response recorded as failed
  must still fail mechanical validation;
- each recorded editor request must be byte-identical to the request
  reconstructed from the frozen inputs and the recorded feedback selection,
  and each recorded reviewer request byte-identical to the request
  reconstructed from the frozen inputs and the recorded editor response — the
  proof that the reviewer actually consumed that response;
- the recorded system-prompt fingerprints must match the prompts of the
  recorded contract;
- a recorded proposal must still match the finalization of the recorded
  reviewer responses (base, sources, patch, approval digest, final
  dispositions).

`comparison.json` and `report.html` are self-contained. They carry the source
run identity and base with the contract used to interpret it, explicit
provenance that both arms share one recorded editor response (with per-theme
request/response hashes and the full per-attempt chains), the verification
results, a stage-recovery section naming the themes that needed a re-ask,
per-theme full final texts, diffs, and candidate dispositions for
both arms (maintenance changes included), fixed theme and input-candidate
denominators with `no_change` and `needs_decision` rows visible, editor and
reviewer output token counts and elapsed request times exactly as recorded —
every recorded attempt included, failed repairs and transport-failed calls
too, missing values `null` (the incremental review cost is the review calls
only) — and any execution/parse/validation failure. A failed arm is reported
as failed — never substituted with empty or no-change output — and stays in
the denominators.

Quality is reported as **unjudged**: fewer lines, more deletions, or fewer
proposed paths do not establish better quality, and a recovered stage response
is stage execution/recovery after a mechanical failure — not independent-review
quality gain — because a retry can change a judgment. The exit status
communicates mechanical execution and format success only, never whether
review improved quality. Runs recorded before this provenance metadata existed are supported
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

## The fixed-input variant experiment

```bash
charliebot memory experiment \
  --input <manifest.yaml> [<manifest.yaml> ...] \
  --output-dir <directory> \
  --backend <configured-id> \
  [--variants NAME ...]
```

One invocation runs the selected variants against every supplied manifest case
(each file's stem names the case) and writes a self-contained summary plus a
readable HTML report under the output root:

```
<output-dir>/
  experiment.json                          # the self-contained variant-comparison summary
  report.html                              # the readable comparison page
  cases/<case>/runs/<variant>/runs/<id>/    # one replay bundle per case x variant (full run record)
  cases/<case>/comparisons/<variant>/       # the paired comparison of that run (editor-only control)
```

The variants are defined once, in `src/core/memory_replay/variants.py`; the CLI
and the pipeline only resolve names. They are, by content:

| variant | editor stage | reviewer stage | feedback view | rationale | reviewer capability |
|---|---|---|---|---|---|
| `baseline-original-flow` | baseline selector | baseline trim review | raw history | visible | trim-only |
| `rationale-hidden-review` | baseline selector | baseline trim review | raw history | **hidden** | trim-only |
| `whole-entry-review` | baseline selector | **whole-entry review** | raw history | visible | **whole-entry** |
| `approved-edit-feedback` | baseline selector | baseline trim review | **selected structured** | visible | trim-only |
| `combined-proposed-design` | **proposed design** | **proposed design** | **selected structured** | **hidden** | **whole-entry** |

Each single-intervention variant changes exactly one bolded dimension relative
to the baseline; the combined variant is the approved design. Everything else —
the frozen source snapshots, guideline, allowed topics, candidate set,
transport, JSON evidence payload, bounded recovery, validation, proposal
finalization, and paired comparison — is shared, so a variant difference is
attributable to its declared dimension alone.

**What is adapted, in every variant alike** (the summary carries this list):
mining, scheduling, the pending-proposal guard, and the production lint/report
subprocesses are frozen out — candidates arrive as manifest sources and nothing
touches the live store; the prompts' external reads (the guideline skill, the
master prompt's Writing Style section) are replaced by the frozen guideline
source, and every variant's prompts state the user's explicit English-memory
requirement so the guideline's stale language clause never confounds a
comparison; working-tree edits and `git checkout` restorations become
base-relative entry operations; the selector's handoff sheet becomes recorded
model output (dispositions plus the three Action/Home/Brevity proof lines),
whose visibility in the reviewer request is the declared rationale dimension;
and trim-only review is validated mechanically as **line removal** — a
trim-only reviewer's returned text for a path must be the selector's proposed
text for that path with whole lines removed, in order, which is the narrow
mechanical form of the pinned reviewer's "deleted or trimmed" under its
no-new-prose rule. Within-line rewriting is unavailable to it; a capability
violation is a visible execution failure eligible for the same one bounded
re-ask, never coerced into acceptance.

**Old-flow feedback is not no-feedback.** The production selector reads a
user-message digest, so the baseline's *raw-history* view exposes the
manifest's whole prior-comment pool — verbatim comment texts with provenance
ids, no approved revisions, exactly what the digest carried. The
`approved-edit-feedback` intervention replaces that with the existing relevance
selection and the structured original-comment / approved-before/after
examples, supplied to both editing and review as in the new design. Both views
render from the identical frozen pool; an absent approved after-text stays
absent, and an approved creation or deletion keeps its empty side verbatim.

**Citations follow the actual stage input.** For the experimental contracts the
allowed citation set is derived from what a stage actually saw — its theme's
assigned sources, the guideline, and exactly the feedback ids its view exposed
(approved-change refs only when their content was exposed) — and the same set
is enforced on the initial attempt, on repairs, and in the later comparison. A
response cannot cite a document its request never carried, or an approved
change its feedback view never showed. Recorded v2/v3 runs keep their original
wider citation meanings.

**Reuse, failures, and exit codes.** A completed case x variant bundle is
identified by its inputs, mode, model, prompt versions, and the variant's
behavioral definition; repeating the experiment reuses it without model calls.
A failed or killed attempt under the same output root stays exactly as
recorded — visible, with its attempt chain and usage — and is never silently
deleted and rerun; a fresh output root is an intentional new experimental draw.
The summary and report link the exact replay bundles and paired comparisons;
each arm's editor-only control comes from the paired comparison of the same
run, i.e. from the exact editor response that variant's reviewer consumed, not
from a fresh editor call. Variants draw their own editor responses; the summary
discloses which arms genuinely shared a byte-identical response, and a single
stochastic draw never grounds a causal claim. Semantic quality stays unjudged:
fewer lines, fewer proposals, or more deletions are recorded data, never a
quality pass. The exit status is 0 when every arm completed execution, format
validation, and its paired comparison; 1 when any arm failed (the summary is
still written and the failures stay recorded); and 1 without a summary when
the experiment itself could not run (bad arguments, manifest, backend, or
output root).

## Out of scope in this delivery

Live curator prompts, daily scheduling, the production approval/commit
endpoints, and any claim of measured quality improvement are all later work.
The private historical corpus and the master's scoring of these runs belong to
the master's evaluation step; synthetic tests here assert only the observable
boundaries of the core.
