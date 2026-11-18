//! `web/features/host_auth`: the host login panel. A standing poller probes every ssh alias in the operator's config
//! and keeps the result in `<home>/host_auth.json`. The feature serves that result at `/host-auth` and
//! `/api/host-auth/*`, with the browser logins on that same page.

mod api;
mod attention;
mod browser_login;
mod config;
mod host_auth;
mod login_state;
mod page;
mod poller;
mod probe;
mod process_tree;
mod readers;
mod rows;
mod ssh_config;
mod state_file;
mod timestamps;

use std::sync::Arc;

use charliebot_registry::pages::HomeCard;
use charliebot_registry::{Registry, RegistryError};

/// The lock every test takes before it claims or holds the process-wide round slot.
///
/// `poller_round_with` skips a round whose slot another test holds, so an unlocked holder makes an unrelated
/// test's round vanish. The tests that would otherwise collide share this lock.
#[cfg(test)]
pub(crate) mod test_support {
  use std::io;
  use std::sync::{LazyLock, Mutex};

  pub(crate) fn round_slot_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(tokio::sync::Mutex::default);
    &LOCK
  }

  /// The answer of the single-host probe that a host-bound login's check runs under test: a return code and output.
  /// The tests that set it hold `round_slot_lock`, so no two of them answer differently at once.
  static BOUND_ANSWER: Mutex<(i32, String)> = Mutex::new((0, String::new()));

  pub(crate) fn set_bound_answer(returncode: i32, output: &str) {
    *BOUND_ANSWER.lock().expect("the answer lock is never poisoned") = (returncode, output.to_string());
  }

  /// The probe a host-bound login's check and a host row's check run under test. It answers from `BOUND_ANSWER`, so no
  /// ssh runs.
  pub(crate) async fn bound_probe(_alias: String) -> io::Result<(i32, String)> {
    Ok(BOUND_ANSWER.lock().expect("the answer lock is never poisoned").clone())
  }
}

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

#[cfg(test)]
mod tests {
  use charliebot_registry::wiring::Phase;

  use super::*;

  #[test]
  fn register_mounts_the_router_starts_the_service_and_lists_the_card() {
    let mut registry = Registry::new();
    registry.register_package("features.host_auth", register).expect("the package registers");
    assert_eq!(registry.routers().len(), 1);
    let services = registry.services_in(Phase::Ready);
    assert_eq!(services.iter().map(|service| service.name()).collect::<Vec<_>>(), ["host_auth"]);
    assert!(registry.services_in(Phase::Early).is_empty());
    let cards = registry.home_cards();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0].url, "/host-auth");
    assert_eq!(cards[0].name, "Host login authorization");
  }
}
