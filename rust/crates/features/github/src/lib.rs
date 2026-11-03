//! `web/features/github`: the `charliebot github` command group, its draft-target validation, and the sender that
//! opens a draft pull request or posts a comment with the operator's own token. The token command comes from the
//! core config's `github:` section, which this crate does not own.

pub mod cli;
mod draft;
mod target;

use charliebot_registry::{Registry, RegistryError};

/// `register`: the platform's draft support.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.draft_platform("github", draft::platform_factory())
}
