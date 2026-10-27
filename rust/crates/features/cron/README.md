# `charliebot-cron`

Scheduled cron tasks in `config.d/cron.d`. The crate owns the five `/api/cron/tasks` routes, cron task loading and
validation, the 60-second scheduler, scheduled handlers, task bindings and sequence recovery. The step-boundary
waiters in `boundary.rs` are the leaf module the scheduler and the sequence controller share.

## Public API and Python sources

| Rust item | Python source |
| --- | --- |
| `register` | `src/features/cron/__init__.py:5-14` |
| `config::DEFAULT_TIMEZONE` | `src/features/cron/config.py:14` |
| `config::StepConfig` | `src/features/cron/config.py:17-37` |
| `config::ScheduledTaskFields` | `src/features/cron/config.py:40-70` |
| `config::ScheduledTaskConfig` and its source/backend validation | `src/features/cron/config.py:73-145` |
| `config::ScheduledTaskError` | `src/features/cron/config.py:148-160` |
| `GET /api/cron/tasks` | `src/features/cron/api.py:163-196` |
| `PUT /api/cron/tasks/{name}` | `src/features/cron/api.py:257-267` |
| `POST /api/cron/tasks` | `src/features/cron/api.py:270-300` |
| `POST /api/cron/tasks/{name}/run` | `src/features/cron/api.py:302-327` |
| `DELETE /api/cron/tasks/{name}` | `src/features/cron/api.py:330-344` |
| `scheduler` service and 60-second tick | `src/features/cron/scheduler.py:26`, `:49-81`, `:108-152` |
| Cron sequence binding, stale-anchor wake duties, weekly chat-history recycle and recovery hooks | `src/features/cron/sequence_controller.py:61-208` |
| Cron startup backend reference check | `src/features/cron/backend_refs.py:7-25` |
| Scheduled handler registration and lookup | `src/runtime/hooks/scheduled_handlers.py:14-29` |
