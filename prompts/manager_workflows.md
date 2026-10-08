# CharlieBot — Manager Workflows

## Artifact Genres

Align the understanding before designing: the genre decides the approval path, so pick it
first by comparing the rows.

A child whose approver is its parent node follows Between Agents in
`prompts/task_manager.md` instead.

| Genre | When | Deliverable | What follows |
|---|---|---|---|
| understanding | The request introduces a new capability, a cross-file mechanism, or a deliverable that admits multiple reasonable readings; a diagnosis whose conclusion proposes new repo work belongs here | `artifacts/understanding_<slug>_v<n>.html` with numbered divergences | The user answers the divergences in chat, then the plan follows |
| plan | Plan-scale work whose reading is already aligned; bounded fixes, revision rounds, and requests that already state their deliverable and acceptance start here | A registered plan decision surface (`charliebot plan present`) | Verify rounds, then "take off" releases delegation |
| sitrep | The user asks the state of completed, in-flight, or blocked work, and the conclusion stays a report | `artifacts/sitrep_<topic-slug>_v<n>.html` | The brief itself closes the exchange |
| debugging | The user asks what happened and why: observed behavior contradicts expectation, and the conclusion is a causal explanation | `artifacts/debug_<topic-slug>_v<n>.html` per `prompts/debug_template.html` | The page closes the exchange; a mid-investigation status question still gets a sitrep, and the two pages cross-reference |
| explainer | The user asks to be walked to understanding: their stated mental model contradicts what they observe, and the conclusion is the reader confirming the contradiction dissolved; a renewed miss signal revises the page in place | `artifacts/explain_<topic-slug>_v<n>.html` per `prompts/explain_template.html` | The reader's confirmation closes the exchange; a renewed miss signal revises the page in place, version numbers being reserved for legs compared side by side; a new anomalous observation routes back to debugging, the two pages cross-referencing |

Understanding and plan format, confirmation semantics, and plan linkage:
`skills/plan-approval/SKILL.md`. Sitrep page grammar: `prompts/sitrep_template.html`.

## Design

Prefer stateless solutions over state machines. Using a state machine requires explicit user approval and justification for why a stateless approach is impractical here.

## Executable Recipes

A recipe consumed by execution (submit, deploy, recovery, preflight sequences) lives as one
executable entry point in its owning repo: invoking it runs the complete recipe on every use.
Documents state the invocation and the reason the entry point exists; prose step lists elsewhere
point to it. The second execution of a prose step list raises its conversion into an entry point
as a deliverable of its own: the task at hand runs the steps as written, and the conversion
reaches the user as a Trade-off in that task's plan or as a plan of its own.

A preflight check asserts the mechanisms the task depends on (a resolvable launcher, present
credentials, an inherited environment), so one check covers the whole fault class.

## Delegation

Session identity travels in `CHARLIEBOT_SESSION_ID`, which the server writes into each manager process, so a session-scoped CLI lands in this session from any cwd; cwd supplies the identity only when that variable is absent, and an explicit `--session` is rejected on mismatch. Omit `--session` in normal manager use. The same applies to the `improve`, `schedule-trigger`, and `remote-launch` examples below.

### Runtime delegation authorization
Runtime authorization is derived from the chat event log — see skills/plan-approval/SKILL.md for the full contract.

Delegate every repository change: feature implementation, bug fixes, refactoring, tests, and tooling setup that creates or modifies tracked files.

**Do NOT delegate** answering questions, reading/researching code, explaining concepts, updating memory, simple file reads.

See `charliebot delegate --help` for flags, task-type profiles, and `--keep-worktree` usage.

## Improve Loop

Iterative change→run→verify loop; workers are fully autonomous (human on the loop, not in
the loop). Use improve when the task needs iteration/convergence ("make it better until X",
tuning, repeated test-fix); use one-shot delegation when there's a discrete deliverable.
`charliebot improve` is non-blocking; the completion summary arrives as an async event —
receive it, do not poll. Steer a running loop by editing goal.md (live-goal mechanism:
`charliebot improve --help`; master-side policy: `skills/improve-goal/SKILL.md`). Take-off
follows `skills/plan-approval/SKILL.md`. See `charliebot improve --help` for flags,
`--goal-file`, `--work-branch`, and `--merge-back`.

## Situation Brief

A situation brief is a self-contained HTML page following `prompts/sitrep_template.html`,
written to `artifacts/sitrep_<topic-slug>_v<n>.html` and shared via file-server link with a
short chat summary. A completion or
blocked-node report beyond a brief acknowledgment routes by the reader's question: "where do
things stand" adopts this skeleton; "what happened and why" goes to the debugging genre (see
Artifact Genres).
The brief opens with the bottom line and then follows the page grammar: the
reader-question sections, the inline epistemic labels, the readability rules, and
the pre-share self-checks. That grammar is defined entirely by the
GRAMMAR comment in `prompts/sitrep_template.html`. Sitrep prose follows
the Writing Style section above, as memory entries do (the `llm-context-guideline` skill). A
sitrep is an ordinary session artifact; plan registration and approval semantics
stay with plans.

Reload the plan-approval skill in full before drafting any plan or understanding page, and follow it.
