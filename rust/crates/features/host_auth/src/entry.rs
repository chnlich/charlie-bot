//! The host-auth package's registrations: the readback supervisor entry, the config section, the router, the poller
//! service, the host readers and the home card.

use std::sync::Arc;

use charliebot_registry::pages::HomeCard;
use charliebot_registry::{Registry, RegistryError};

use crate::{api, config, poller, process_tree, readers};

pub const READBACK_SUPERVISOR_ARG: &str = process_tree::SUPERVISOR_ARG;

/// The helper entry that runs one readback as a supervised process.
pub fn readback_supervisor_entry(args: &[std::ffi::OsString]) -> Option<u8> {
  process_tree::helper_entry(args)
}

/// `register`: the config section, the router, the `host_auth` service in phase Ready, the host readers the cron gate
/// and the Waiting on you read use, then the home card.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.config_section(config::section())?;
  registry.router(api::routes, api::matches)?;
  registry.service(Arc::new(poller::HostAuthPoller::default()))?;
  registry.host_logins(readers::host_logins)?;
  registry.host_probe(readers::probe_alias_now)?;
  registry.home_card(HomeCard {
    name: "Host login authorization",
    url: "/host-auth",
    description: "Per-host ssh login state and the browser logins that renew it.",
  })
}
