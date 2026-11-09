# charliebot-devin

The `devin` backend type: the Devin CLI in print mode, run headless with every tool approved
(`--permission-mode=dangerous`), as the `cc-claude` type runs `claude -p` with skipped permissions.

## Public API

- `charliebot_devin::{register, register_with_host, PACKAGE, USAGE_SOURCE}`. The type's `factory`, `options`,
  `devin_cli` and `export` modules are private.

## Option

An option names `type: devin`, an `id` that starts with `devin-`, a `label`, and a `model`: the SWE-2 id that
`devin models list` prints after login. The type takes no other key.

## One turn

The runner calls `prepare_transport` with the run's log directory. The turn then:

1. writes the framed instructions and the message into `devin_prompt.md` there;
2. runs `devin --prompt-file=… --permission-mode=dangerous --respect-workspace-trust=false --model=… [--resume=<id>]
   --export=<log dir>/devin_export.json --print`, with no stdin;
3. on exit 0, reads stdout as the final answer and the export as the events: `session_attached` from `session_id`,
   the steps after the last `user` step or compaction summary, whichever comes later, which are the ones this turn
   added (text, tool calls and tool results), and the usage summed over those steps' `metrics`, with a null cost.
   `input_tokens` is the prompt sum, the cached part included, and `cache_read_input_tokens` is the cached count, as
   the codex translator reports OpenAI-style counts; the ledger's run-logs-only record reads `input_tokens`, so each
   input token is counted once. The export also holds the earlier turns, whose steps become no events and add no
   usage.

When a conversation grows long, Devin compacts it: a system step tagged `extra.telemetry` `source: "compaction"`,
`operation: "summary"` replaces the earlier steps, the current turn's `user` step included. The turn then starts
after that summary. The export drops the compacted steps and their `metrics`, so that turn's usage leaves out the
tokens spent before the compaction.

A nonzero exit fails the turn with Devin's stderr. When that stderr carries `"cognition.ai/retryable": true` and
states `reset in N seconds`, `retry_after` returns the duration and its calculated absolute time. The daily-limit form
`reset in H hours M minutes (at <date> UTC)` preserves the stated absolute time for the manager or worker's cooling
record even when the reset is beyond `backend_retry`'s automatic-wait limit. The wait limit decides whether the run
retries; it does not change the cooling time. A missing export,
one without `session_id`, one with neither a `user` step nor a compaction summary, or one whose current-turn agent
step has no `metrics` fails the turn and names the export path.

## Subscription quota

The usage panel reads Devin's daily and weekly plan quota from `GetUserStatus`. The quota poll also reads the CLI's
model catalog; its `Free` marker excludes those models from a plan member reading without keeping a model-name list in
this crate. Member selection reads only the state those panel reads last stored. The panel carries the plan's windows
only; a member's cooling lives in the shared backend-availability record that the pool reads for selection.

## Where the field names live

`export.rs` is the one file that names the export's fields. Their names come from the CLI binary's strings
(`ATIF-v1.x`), not from a Devin document, so a live export that differs is corrected there.

## Boundary

A devin launch runs in its session cgroup, as every spawned agent does. It does not run under the write-only Landlock
boundary that improve rounds apply to `claude`: that boundary wraps the `claude` launcher named by a claude option's
`cli_binary` (`dev/improve/remote-runner.sh`), and a devin option has no such key.
