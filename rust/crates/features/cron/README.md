# `charliebot-cron`

Scheduled cron tasks in `config.d/cron.d`. The crate owns the five `/api/cron/tasks` routes, cron task loading and
validation, the 60-second scheduler, scheduled handlers, task bindings and sequence recovery. The step-boundary
waiters in `boundary.rs` are the leaf module the scheduler and the sequence controller share.

## Public API

- `register`, which adds the routes, the `cron` turn contribution, the session metadata slot, the startup backend
  reference check, the scheduler service and the sequence owner
- `GET /api/cron/tasks`, `PUT /api/cron/tasks/{name}`, `POST /api/cron/tasks`, `POST /api/cron/tasks/{name}/run`
  and `DELETE /api/cron/tasks/{name}`
- `config::DEFAULT_TIMEZONE`, `config::StepConfig`, `config::ScheduledTaskConfig`, `config::ScheduledTaskError`,
  `config::validate_task`, `config::task_from_value`, `config::CREATE_MODEL` and `config::UPDATE_MODEL`
- `seed::SeedReport`, `seed::seed_default_cron_tasks` and `seed::setup_step`
- the `scheduler` service and its 60-second tick
- the cron sequence binding, the stale-anchor wake duties, the weekly chat-history recycle and the recovery hooks
- the scheduled handler registration and lookup
