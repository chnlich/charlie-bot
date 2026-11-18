//! The binding lookup a request that only renders it takes.
//!
//! A render shows the binding's facts and decides nothing from them, so it reads the snapshot each controller's
//! listing already serves. The write paths keep `Registry::binding_for`, whose controllers read their fresh state.

use std::sync::Arc;

use crate::registry::Registry;
use crate::sequence::SequenceBinding;

impl Registry {
  /// The binding of `session_id` for a request that only renders it.
  ///
  /// Every controller answers with the read its listing already serves, so a page shows the bindings the sidebar
  /// shows. [`Registry::binding_for`] reads each controller's fresh state for the write paths whose outcome the
  /// binding decides. Two controllers that both bind one session fail, as they do for [`Registry::binding_for`].
  pub fn binding_for_render(&self, session_id: &str) -> anyhow::Result<Option<Arc<dyn SequenceBinding>>> {
    let mut found: Option<Arc<dyn SequenceBinding>> = None;
    for controller in self.sequence_controllers() {
      let Some(binding) = controller.binding_for_render(session_id) else {
        continue;
      };
      if found.is_some() {
        anyhow::bail!("multiple sequence controllers bind session {session_id:?}");
      }
      found = Some(binding);
    }
    Ok(found)
  }
}
