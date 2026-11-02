# Taste extraction

This run turns the user's own choices into precedent rows in the taste table. The daily run and the backfill
share this contract.

Read these two files before you write a row or a memory change:
- the "Text a model reads" subsection of the Writing Style section in `~/workspace/charlie-bot/prompts/master.md`;
- `~/workspace/charlie-bot/skills/llm-context-guideline/SKILL.md`.

## Candidates

The run reads the candidates that `charliebot taste candidates` prints for its window. The window of the
daily run is the 24 hours before its scheduled time. The window of a backfill is the one its task names.
Each candidate carries:
- `source`: `comments`, `notes`, `downvote-replies` or `chat`;
- `text`: the user's own words;
- `previous_web_reply`: the reply the user read before writing `text`, which states the question and the
  recommendation;
- `session_id`, `url` and `timestamp`;
- `event_id` for a chat message or a comment batch, or `plan_version` for a plan note;
- `trade_off_options` on a plan note: the option lines of the predecessor page, or `null` when that page
  is gone.

The `text` holds the user's choice when the user made one. The `previous_web_reply` gives the context of
that choice. The agent wrote it, so it never supplies the chosen option.

## Which candidates become rows

A candidate becomes a row when the user:
- picks one option among the options that the candidate or its `trade_off_options` names;
- overrides a recommendation;
- rejects a working result and states a preference.

A candidate that reports a defect or asks for new work becomes no row.

Text that the system or another agent delivered is not the user's words. A scheduled trigger notice, a
context reset, a task report, a message from another session and a takeover briefing become no row.

A general preference that the user states becomes a memory candidate, staged with `charliebot memory add`.
It becomes no row, because a preference holds across situations and a row holds one situation.

## Rows

Write one row for each candidate that qualifies. A row carries:
- `decided_at`: the candidate's `timestamp`;
- `domain`: `<area>/<topic>`. Use a domain that `charliebot taste index` already lists when one fits the
  situation;
- `kind`: `choice` when the user picked among options, and `rejection` when the user rejected a working
  result;
- `situation`: one or two sentences;
- `options`: every option the candidate names, and `[]` when it names none. A rejection lists the delivered
  result and the result the user asked for;
- `recommended`: the recommendation that the candidate names, or `null`;
- `chosen`: the option the user picked, or the result the user asked for;
- `reason`: the user's words for the choice, or `未说明` when the candidate states no reason;
- `source`: `session_id` and `url` from the candidate, and exactly one of `event_id` or `plan_version`
  from the candidate;
- `supersedes`: the id of the row that this row reverses, or `null`.

Write each row with `charliebot taste add --file <row.json>`. The command validates every field, assigns
the id and refuses a duplicate. A refusal that names an existing row means that candidate already has a
row, so continue with the next candidate. Any other refusal stops the run, and the run reports the error.

Write rows only through `charliebot taste add`, and change a row's state only through `charliebot taste mark`.

## Reversals

Before you write a row, read the domain's active rows with `charliebot taste query --domain <domain>`. A
query of a domain with no active row fails and lists the known domains. Treat that failure as an empty set
of active rows.

Fill `supersedes` with an active row's id when all of these hold:
- the row is in the same domain;
- the row's situation matches the new situation;
- the new choice is the opposite of the row's `chosen`.

When the reversed row is covered by a memory entry, or a memory entry states the rule that the new choice
reverses, stage a memory revision candidate with `charliebot memory add`, following the
`llm-context-guideline` skill. Its first line is `# <title>`, and its body names the target entry and the
change.

## Patterns on Mondays

When the daily run falls on a Monday, the run proposes patterns. A pattern is a sentence that states one
direction the user chose in one kind of situation. A pattern covers at least three active rows that choose
that direction.

Propose all patterns in one Waiting on you item in the run's own session. The item names the rows behind
each pattern and asks for a yes or a no on each pattern.

Commit nothing before the user's yes.

When the user says yes to a pattern:
- commit one memory entry for the pattern as a user-approved commit, following the `llm-context-guideline`
  skill;
- mark each row behind the pattern as covered: `charliebot taste mark <id> --covered-by <entry>`.

When the user says no to a pattern, its rows stay active and nothing changes.
