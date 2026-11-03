//! `web/features/github`: the `charliebot github` command group, its draft-target validation, the `github:` config
//! section, and the sender that opens a draft pull request or posts a comment with the operator's own token.

pub mod cli;
mod config;
mod draft;
mod target;

use charliebot_registry::{Registry, RegistryError};

/// `register`: the `github:` config section and the platform's draft support.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.config_section(config::section())?;
  registry.draft_platform("github", draft::platform_factory())
}
