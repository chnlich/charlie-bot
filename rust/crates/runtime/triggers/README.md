# `charliebot-triggers`

Delayed or watched triggers for the Rust server. The crate owns the internal schedule and cancel routes, durable
trigger records, waiters and startup recovery. The trigger step is registered after task recovery has completed.

## Public API and Python sources

| Rust item | Python source |
| --- | --- |
| `register` | `server.py:279-282`; routes are mounted through `internal.router` at `server.py:351` |
| `triggers::MAX_PENDING_TRIGGERS` | `src/runtime/triggers.py:77` |
| `triggers::TriggerStatus` | `src/infra/models.py:84-88` |
| `triggers::LocalPid` | `src/infra/models.py:273-285` |
| `triggers::RemotePid` | `src/infra/models.py:288-299` |
| `triggers::SlurmJob` | `src/infra/models.py:301-310` |
| `triggers::WatchTarget` | `src/infra/models.py:312-315` |
| `triggers::PendingTrigger` | `src/infra/models.py:318-330` |
| `triggers::ScheduleTriggerRequest` | `src/infra/models.py:698-706` |
| `triggers::TriggerCreateError` | `src/runtime/triggers.py:84-88` |
| `triggers::TriggerManager` and `TriggerManager::new` | `src/runtime/triggers.py:440-459` |
| `TriggerManager::create_trigger` | `src/runtime/triggers.py:461-593` |
| `TriggerManager::list_triggers` | `src/runtime/triggers.py:595-660` |
| `TriggerManager::cancel_trigger` | `src/runtime/triggers.py:662-672` |
| `TriggerManager::recover_pending` | `src/runtime/triggers.py:674-694` |
| `triggers::routes` | `src/runtime/api/internal.py:258-317` |
| `triggers::ROUTE_PATTERNS` | `src/runtime/api/internal.py:258-317` |
| `triggers::trigger_step` | `server.py:275-285` |

`TriggerManager::list_triggers` has the public signature
`pub async fn list_triggers(&self, session_id: &str) -> anyhow::Result<Vec<PendingTrigger>>`.

The models keep Python's discriminator and field order so existing Python-written trigger files can be recovered,
and Rust-written records can be read by Python. A scheduled trigger reaches the existing dispatch admission path with
its trigger id as the stable input id.
