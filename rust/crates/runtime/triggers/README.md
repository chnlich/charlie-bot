# `charliebot-triggers`

Delayed or watched triggers for the Rust server. The crate owns the internal schedule and cancel routes, durable
trigger records, waiters and startup recovery. The trigger step is registered after task recovery has completed.

## Public API

- `register`
- `triggers::ScheduleTriggerRequest`, `triggers::TriggerCreateError` and `triggers::TriggerManager` with
  `TriggerManager::new`, `create_trigger`, `list_triggers`, `cancel_trigger` and `recover_pending`
- `triggers::LocalPid`, `triggers::RemotePid` and `triggers::SlurmJob`, the three watch targets
- `triggers::MAX_PENDING_TRIGGERS`
- `triggers::routes`, `triggers::ROUTE_PATTERNS` and `triggers::trigger_step`

`TriggerManager::list_triggers` has the public signature
`pub async fn list_triggers(&self, session_id: &str) -> anyhow::Result<Vec<charliebot_store::trigger_files::PendingTrigger>>`.

The models keep the discriminator and field order of the trigger records Python wrote, so a recorded trigger file
loads unchanged. A scheduled trigger reaches the existing dispatch admission path with its trigger id as the stable
input id.
