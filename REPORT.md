# Cron fire to bound session

## Result

The approved S1–S8 behavior is implemented on the current `main` base. Prompt fires now enqueue one trigger in the bound session; that session starts a prompt task through `charliebot delegate --bound-task`. Steps and handlers keep their server-owned paths. The release and server switch were not run. No real-home cron task or config file was changed; the required delayed-trigger mechanism wrote launch bookkeeping for this session.

S8(1) passed on one isolated host3 home. The old and new server each refused the other version's live round by its `cron:<task>:<firing>:leaf` request id. The synthetic workers ended successfully before final shutdown, and the final server process, backend child processes, and listener were absent afterward.

## Approved schema and choices

- API request: add only optional `bound_task`; use existing `description` for the optional session note. The CLI adds `--bound-task` and `--note`. The worker creation fact gains no task-name field; `cron:<task>:<firing>:leaf` carries the task identity.
- Dropped API `note` field: `description` already carries the note. A separate API field would add back that distinction.
- Dropped persisted round-task field: the request id carries the task name. A stored field would add it back if task identity could no longer be read from that id.
- Dropped accepting `cron:` request ids on ordinary delegates: T5 makes those calls 400. A separate task-name field would add scheduled attribution back if non-bound callers needed it.
- Dropped a steps template: `memory-curator` remains a steps task and the server starts its graph on each fire. A steps template would be needed only if that task were routed through a session.
- T1–T5 match the resolved answers: skips are task-local; the session defaults to its bound task and states a specific reason when it chooses otherwise; the server starts steps graphs; prompt tasks use two repo-owned templates by category; a non-bound `cron:` request id is rejected with 400.

## Acceptance evidence

| Requirement | Code and evidence |
|---|---|
| Prompt fire with repo, without repo, and `mode: master` | `rounds_tests::a_prompt_task_with_a_repo_fires_its_trigger_into_the_bound_session_with_the_round_template`; `rounds_tests::a_prompt_task_without_a_repo_gets_the_repo_less_template`; `rounds_tests::a_master_task_fire_sends_its_own_prompt_without_a_template` |
| Steps graph and handler fire | `scheduler::tests::a_steps_task_launches_its_first_step_prompt_in_either_launch_path` passed when run explicitly with `--ignored`; `scheduler::tests::scheduler_tick_runs_handlers_and_updates_the_shared_session_views` passed in the cron suite. `configs/cron.default.yaml` keeps `memory-curator` as a steps task. |
| Unreadable template | `rounds_tests::an_unreadable_template_posts_an_error_records_a_skip_and_starts_no_worker` checks the bound-session error, skip reason, `last_scheduled_run`, and absence of a worker. |
| T1 and restart-persistent skip | `rounds_tests::a_queued_trigger_input_of_the_task_skips_the_due_fire_and_another_tasks_input_does_not`; `rounds_tests::an_open_round_of_the_task_skips_its_due_fire_and_another_tasks_open_round_does_not`; `rounds_tests::a_round_queued_before_a_restart_skips_the_due_fire_on_a_fresh_scheduler`. |
| Bound-route refusals | `rounds_tests::a_bound_delegation_refuses_a_task_that_names_another_session_or_no_known_task`; `rounds_tests::a_bound_delegation_refuses_a_master_steps_or_handler_task_and_a_note_on_a_repo_less_task`; `rounds_tests::a_bound_delegation_is_refused_while_a_round_is_in_flight_and_while_the_host_awaits_approval`; `routes::tests::a_bound_delegation_refuses_each_field_the_unbound_path_defaults_and_a_cron_id_outside_one`. |
| Shared host-login check | The fire and bound route obtain the hold from `host_gate::hold_reason` through the registered binding reader; there is one implementation of the check. |
| S8(1) request-id compatibility | `firing::tests::a_round_request_id_names_the_task_that_created_the_round` asserts the `cron:<task>:<firing>:leaf` form, and `rounds_tests::a_round_queued_before_a_restart_skips_the_due_fire_on_a_fresh_scheduler` checks restart persistence. The isolated live readback on host3 used a home at `~/scripts/20261118_cron_fire_s8/home-255146`: old server `a23d06b20` created `cron:s8-cross-version:2026-11-18T09:53:30.151127+00:00:leaf`; the new server returned 409 naming worker `63dd304b-8758-5dfb-a8ef-9748f2b6440d`. The new server then created `cron:s8-cross-version:2026-11-18T09:53:33.264850+00:00:leaf`; the old server returned 409 naming worker `3956386a-ee62-52e5-b5b5-182c8985ccf2`. Both runs ended successfully. `~/charliebot_runs/20261118T095328-15fa09/log` records the readback and exit 0. |

## Deviations

| Approved term | What landed | Reason |
|---|---|---|
| Plan anchors at `b3d922d19` | The behavior is implemented in the current `main` modules; line and symbol anchors moved. | Current `main` was 92 commits past the plan baseline, so the implementation followed the approved behavior at the current code paths. |
| `delegate --help` bounded-size check | The cap is 4,800 characters; measured help is 4,702 and names both new flags. | The approved CLI options and their usage contract add help text beyond the previous 4,100-character cap. |

There is no deviation to the approved API schema, resolved Trade-offs, or section 3 behavior. The required live S8(1) readback passed.

## Verification

All Rust commands ran through `dev/remote-run/cargo-slot.sh` from `rust/`.

- Full suites passed for `charliebot-tasks`, `charliebot-store`, `charliebot-registry`, `charliebot-cron`, and `charliebot-cli`.
- The steps fire, bound delegation, and steps-chain integration tests named above also passed when run explicitly.
- `cargo fmt --all -- --check` passed.
- `cargo clippy --workspace --all-targets -- -D warnings` passed after boxing the ready payload in `ScheduledRoundStart`.
- S8(1) live readback passed using the synthetic server launcher in `~/scripts/20261118_cron_fire_s8/live.sh`. The old and new binaries shared one synthetic `CHARLIEBOT_HOME`; each returned 409 for the other version's active round. The trigger log names both request ids and workers. All worker and manager runs reached terminal states before the final server stop; tracked fake-CLI PIDs were gone and port 18963 was closed afterward.
