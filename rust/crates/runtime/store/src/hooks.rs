use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use charliebot_core::config::CharlieBotConfig;
use serde_json::Value;

use crate::StoreResult;
use crate::metadata::SessionMetadata;

/// A feature renderer for one persisted event type.
pub trait EventRenderer: Send + Sync {
  /// The rendered message, or none when the event carries no message.
  fn render(&self, event: &Value) -> StoreResult<Option<Value>>;
}

impl<F> EventRenderer for F
where
  F: Fn(&Value) -> StoreResult<Option<Value>> + Send + Sync,
{
  fn render(&self, event: &Value) -> StoreResult<Option<Value>> {
    self(event)
  }
}

/// One contribution's after-turn work. The store starts it as its own task after a master_done frame is broadcast.
pub struct AfterTurnWork {
  /// The contribution's class name. The task is named `after-turn-<name>-<session id>`.
  pub name: String,
  /// Future for the contribution's after-turn action.
  pub future: Pin<Box<dyn Future<Output = StoreResult<()>> + Send + 'static>>,
}

/// Feature-owned event renderers and runtime-state notifications.
pub trait RuntimeEventHooks: Send + Sync {
  /// The feature event renderers in registration order.
  fn event_renderers(&self) -> StoreResult<Vec<(String, Arc<dyn EventRenderer>)>>;

  /// Each contribution's work for one completed turn.
  fn after_turn(&self, meta: SessionMetadata, event: Value, cfg: CharlieBotConfig) -> Vec<AfterTurnWork>;

  /// The session's derived sidebar state, marked dirty.
  fn mark_sidebar_dirty(&self, session_id: &str);

  /// Feature-owned state for the session, dropped.
  fn drop_session_runtime_state(&self, session_id: &str);

  /// Each feature's files, copied from the parent session directory into the fork's directory.
  fn copy_on_fork(&self, parent_dir: &Path, child_dir: &Path) -> StoreResult<()>;
}

/// Explicit hook set for a server with no registered event contributions.
pub struct EmptyRuntimeEventHooks;

impl RuntimeEventHooks for EmptyRuntimeEventHooks {
  fn event_renderers(&self) -> StoreResult<Vec<(String, Arc<dyn EventRenderer>)>> {
    Ok(Vec::new())
  }

  fn after_turn(&self, _meta: SessionMetadata, _event: Value, _cfg: CharlieBotConfig) -> Vec<AfterTurnWork> {
    Vec::new()
  }

  fn mark_sidebar_dirty(&self, _session_id: &str) {}

  fn drop_session_runtime_state(&self, _session_id: &str) {}

  fn copy_on_fork(&self, _parent_dir: &Path, _child_dir: &Path) -> StoreResult<()> {
    Ok(())
  }
}
