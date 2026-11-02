# Taste extraction

Read the Writing Style section of `~/workspace/charlie-bot/prompts/master.md` first, its "Text a model
reads" subsection included, and write every row by those rules. Repo text names no person: say "the
user".

This prompt serves the daily run and the backfill. The daily run reads the 24 hours before its
scheduled time. A backfill run reads the window its task gives it.

## Read the candidates

Run `charliebot taste candidates --since <iso>` for the run's window and read its output in full.
Each candidate carries the user's text, the latest web reply before it, the session link, and the
timestamp. A candidate from a plan note also carries its `<plan>/v<n>` and the predecessor page's
Trade-off lines, which name that fork's options.

Every candidate is the user's own text. An `agent_message` is another agent's message and never
becomes a candidate.

## Choose the candidates that are precedents

A candidate becomes a row when the user picks among options, overrides a recommendation, or rejects
a working result and states a preference. Leave out bug reports and new requests: they name a
defect or ask for work, not a choice.

A general preference the user states goes to `charliebot memory add`, not to a row, because it holds
outside one situation. The memory store owns what the user wants at all times; the table owns what
the user chose in one situation.

## Write the row

Read the domain's active rows before you write: `charliebot taste query --domain <domain>`. Prefer a
domain that `charliebot taste index` already lists, so the table stays coarse enough to compare
situations across sessions.

A row carries these fields:

- `decided_at`: the decision time, from the candidate's timestamp;
- `domain`: `<area>/<topic>`;
- `kind`: `choice` when the user picked among options, `rejection` when the user rejected a working
  result;
- `situation`: one or two sentences;
- `options`: every option the source names, and `[]` when it names none;
- `recommended`: the recommendation when the source states one, and `null` otherwise;
- `chosen`: the user's choice, or the result the user asked for;
- `reason`: the user's words, or `未说明` when the source states none;
- `source`: `session_id`, `url`, and exactly one of `event_id` or `plan_version`;
- `supersedes`: the row this precedent reverses, or `null`.

When you judge the new choice a reversal of an active row in the same situation, name that row in
`supersedes`. Before you do, compare the situations, not just the domains: an option chosen in a
different situation is no reversal. When the reversal contradicts a memory entry, also stage a
memory revision candidate with `charliebot memory add`, following the `llm-context-guideline` skill.

Write each row with `charliebot taste add --file <row.json>`. The command validates every field,
assigns the row id, and refuses a row whose source and choice repeat an existing row.

## Propose patterns on Mondays

The daily run on Monday proposes patterns. A pattern is at least three active rows that choose the
same direction in one kind of situation; the rule is one sentence that covers all of them.

List every pattern as one Waiting on you item in this session, naming the rows behind it, and ask
the user for a yes or a no on each. Commit nothing before that yes.

After the user says yes to a pattern, commit one memory entry for it as the user-approved commit
that the `llm-context-guideline` skill requires, then mark each of its rows covered with
`charliebot taste mark <id> --covered-by <entry>`. A pattern the user turns down changes nothing:
its rows stay active.
