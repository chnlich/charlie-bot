# Week-one review of parent-to-child messages

The review labels each message that a parent session sent to one of its direct child sessions in a time window. A
model reads the message, the files it points to, and the child's task record, then marks the content the child does
not need. The labels count how many messages carry unneeded content and how many carry only unneeded content.

The entry point is `review.mjs`. It runs on Node 20 or later and calls the `claude` CLI and the internal LLM gateway.

```sh
node dev/week-review/review.mjs label --from 2026-10-14T20:24:49Z --to 2026-10-21T20:29:58Z \
  --reports-until 2026-10-21T20:33:00Z --out <dir> [--home <CHARLIEBOT_HOME>] [--model claude-opus-5-5] [--parallel 6]
node dev/week-review/review.mjs tally --out <dir>
node dev/week-review/review.mjs rejudge --out <dir> --model <model> --gateway <base URL> [--sample 20] [--seed 1]
```

- `label` selects the window's messages from the session store (`<home>/sessions`, default `CHARLIEBOT_HOME` or
  `~/.charliebot`), builds one packet per child and chunk, judges each packet, and writes the rows. It resumes from the
  judgments already in `<dir>`, and it refuses a judgment that another judge wrote.
- `tally` prints the counts of `<dir>` next to the week-one numbers (1104 messages, 286 with unneeded content, 20 wholly
  unrelated), with the window, judge and prompt hashes from `run.json`.
- `rejudge` draws a seeded sample of the labeled messages and judges each one again through the gateway. It prints
  agreement with the Opus labels and exits 0 when every answer has the expected shape. Agreement is not a gate.

`<dir>` holds the outputs, which contain message text and file text. Keep it outside the repository.

| File | Contents |
|---|---|
| `labels.jsonl` | one row per message: `event_id`, `sender`, `target`, `sent_at`, `verdict`, `kinds`, `judge_backend` |
| `messages.jsonl` | the same rows with the file states, character counts, and the judge's quote and reason |
| `packets/`, `judgments/` | each packet the judge read, and the parsed answers and raw result per packet |
| `run.json`, `tally.txt` | the run's window, judge and prompt hashes; the tally |
| `rejudge.jsonl`, `rejudge_summary.txt` | the gateway model's answers for the sample, and the agreement |

## Judging instructions

`judge_prompt.md` is the manager instructions. `judge_prompt_worker.md` is the instructions for a worker child, which
adds the note that a worker node's reviewer reads the later parent messages. Both are the recovered text, byte for
byte, of the review's last edit to each file. They were recovered from the session's chat log:

- the base text: the tool result at event 8760 (the directory listing of the earlier relay audit, which printed
  `classify_prompt.md` in full), 2026-10-21 20:31Z;
- the edits, in order: event 8849 (the FILE-block paragraph and the off-goal share over the FILE blocks), event 8855 (the
  repeated-file sentence), event 9081 (the worker note, worker prompt only), event 9257 (the own-file sentence, both
  prompts).

The judge is `claude -p --model claude-opus-5-5 --output-format json --dangerously-skip-permissions` with the tools
disabled (event 8849, `classify.sh`). The prompt and packet go in on stdin, separated by a rule line. The answer is the
last fenced `json` block of the result, or failing that the array from the first `[` to the last `]` (event 9014 and
event 9027 for the repair steps). A message the answer skips gets one more call that judges only that message (event
9027, `fill_missing.py`). An answer for an id outside the packet is dropped and its id recorded in the judgment
(`dropped_ids`), as the same repair step did: a model that mistypes an id leaves its message unanswered, so the retry
judges it. Each packet still needs exactly one answer per message, or the run fails.

## Message selection and file merging

- A message counts when it is an `agent_message` event in a child's chat log, `from_session` is the child's
  `task_parent_id`, and its timestamp is in the window (event 8789, `scan_window.py`).
- A child's reports to its parent are the parent's `agent_message` events from that child, up to `--reports-until`.
  They go in the packet as context (event 8839, `build_packets.py`).
- A file a message names is a path under `~`, the home directory, or `/tmp`, or an `artifacts/` path resolved in the
  parent's session (event 8810, `paths.py`; event 8834 fixes the trailing punctuation). A
  path is read as text, as the file's text with HTML tags reduced to text, or it is marked missing, directory or binary.
  Text longer than 30000 characters is cut. A file in the child's own session shows only its name (event 9257).
- Packets hold up to 120000 characters, and a repeated file in one packet shows a pointer to the first message that
  carried it (event 8839).

## Recovery notes

Recovered as written: the selection rule, the file-merge states and cap, the packet layout, the judge invocation, the
verdict scale (`needed`, `mixed`, `unrelated`), the five kinds, the answer parsing and the fill step, and the tally.

Changed, with the reason:

- The recovered path pattern named the author's home directory as a literal. The tool takes the running user's home
  directory, so the same paths match on any host and no host path lives in the repository.
- The record timestamps are compared as instants, not as strings. The window bounds are written with `Z`.
- The recovered worker prompt file was the one used for the worker packets before the own-file sentence was added to
  it (event 9257). The tool uses the final file for both profiles. The sentence only matters for a packet that points to
  the child's own file, and no worker packet did in the recovered run (the rerun listing at event 9263 lists manager children
  only).
- The recovered packet build read the child's reports with no upper bound (event 8839). The tool bounds them at
  `--reports-until`, so a rerun after the store has grown holds the reports the review saw.

Not recovered, and the replacement:

- The packets and per-packet judgments of the recovered run were in the removed review directory. The tool rebuilds
  them from the rules above, so the packets come from the store as it now stands.
- The store's parent links changed after the review. Child sessions re-parented on 2026-10-29 keep their messages, but
  the `task_parent_id` they now carry names a different session, so the recovered selection rule no longer picks up
  their messages from the old parent. The run reports the count as it stands; the report explains the difference.
- The model's answers are not deterministic. The recovered review measured run-to-run agreement of 89.4% on the
  needed-versus-unneeded call for messages judged twice (event 9299).

## Replay seam

The offline replay of the pre-send check will join this tool as another subcommand. It reuses the window selection
(`lib/window.mjs`), the file merge (`lib/files.mjs`), the packet layout (`lib/packets.mjs`) and the judge
(`lib/judge.mjs`), and it joins the labels on `event_id`. The `COMMANDS` table in `review.mjs` takes the new entry.

## Tests

`test/review.test.mjs` covers the contracts above on synthetic sessions and files. The Rust test
`rust/crates/app/script-tests/tests/week_review/main.rs` runs it under `node --test`, so `cargo test --workspace --locked`
runs it too.
