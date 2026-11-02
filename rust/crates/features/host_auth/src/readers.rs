//! The host readers that other features reach through the registry: the login records the cron gate and the Waiting on
//! you read consume, and the probe the gate runs before a fire.

use std::path::PathBuf;

use charliebot_registry::FeatureContext;
use charliebot_registry::registry::HostLogin;
use futures_util::future::BoxFuture;

use crate::host_auth;
use crate::probe::probe_host_fresh;

/// The state file of the profile that `features` serves.
fn state_path(features: &FeatureContext) -> anyhow::Result<PathBuf> {
  Ok(features.config.get()?.charliebot_home.join(host_auth::STATE_FILENAME))
}

/// The login records of the probed hosts. An unreadable state file is an error, not an empty list.
pub fn host_logins(features: &FeatureContext) -> anyhow::Result<Vec<HostLogin>> {
  host_auth::host_logins(&host_auth::read_state(&state_path(features)?)?)
}

/// Probe `alias` now over a new connection, write the verdict into the state, and return the host's record.
pub fn probe_alias_now<'a>(features: &'a FeatureContext, alias: &'a str) -> BoxFuture<'a, anyhow::Result<HostLogin>> {
  Box::pin(async move {
    let state_path = state_path(features)?;
    host_auth::probe_alias_with(&state_path, &host_auth::ssh_config_path(), alias, probe_host_fresh).await
  })
}
