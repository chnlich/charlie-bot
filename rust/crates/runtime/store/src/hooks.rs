use std::path::Path;
use std::sync::Arc;

use serde_json::Value;

use crate::StoreResult;

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

/// Feature-owned event renderers and runtime-state notifications.
pub trait RuntimeEventHooks: Send + Sync {
  /// The feature event renderers in registration order.
  fn event_renderers(&self) -> StoreResult<Vec<(String, Arc<dyn EventRenderer>)>>;

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

  fn mark_sidebar_dirty(&self, _session_id: &str) {}

  fn drop_session_runtime_state(&self, _session_id: &str) {}

  fn copy_on_fork(&self, _parent_dir: &Path, _child_dir: &Path) -> StoreResult<()> {
    Ok(())
  }
}
