//! Sidebar contributions (`src/runtime/hooks/sidebar_contributions.py`): what a feature adds to sidebar rows and to
//! session housekeeping. The session store names no feature; it asks the registered contributions at four moments:
//! the sidebar probe reads `row_flags` and watches `watched_files`, a fork or an elone calls `copy_on_fork`, a
//! dropped session calls `drop_runtime_state`, and the sidebar listings call `view_member`.

use std::path::Path;
use std::sync::Arc;

use charliebot_store::metadata::SessionMetadata;

use crate::registry::{Registry, RegistryError};

/// One feature's sidebar contribution. Every method defaults to adding nothing.
pub trait SidebarContribution: Send + Sync {
  /// Paths relative to the session directory that `row_flags` reads. The probe re-runs `row_flags` only when one of
  /// them, or another probe input, changed.
  fn watched_files(&self) -> &'static [&'static str] {
    &[]
  }

  /// The sidebar flags this feature sets on one session's row. A corrupt file yields no flag and does not fail.
  fn row_flags(&self, _session_dir: &Path, _session_id: &str) -> Vec<(&'static str, bool)> {
    Vec::new()
  }

  /// Copy this feature's files from the parent's session directory into the child's.
  fn copy_on_fork(&self, _parent_dir: &Path, _child_dir: &Path) -> anyhow::Result<()> {
    Ok(())
  }

  /// Drop this feature's in-memory state for one session.
  fn drop_runtime_state(&self, _session_id: &str) {}

  /// The key of the sidebar view whose root `meta` is, or `None`. A view lists its roots and every row below them.
  fn view_member(&self, _meta: &SessionMetadata) -> Option<&'static str> {
    None
  }
}

impl Registry {
  /// Register a sidebar contribution (`sidebar_contributions.register_sidebar_contribution`).
  pub fn sidebar_contribution(
    &mut self,
    name: &'static str,
    contribution: Arc<dyn SidebarContribution>,
  ) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.sidebar_contributions.insert(package, name, contribution)
  }

  /// The sidebar contributions in registration order.
  pub fn sidebar_contributions(&self) -> impl Iterator<Item = &Arc<dyn SidebarContribution>> {
    self.sidebar_contributions.values()
  }
}
