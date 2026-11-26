//! The usage crate: the quota panel, the usage ledger capture, the token usage page, the cold-storage sweep and the
//! `backend_ids` startup check. The home card stays registered.

mod api;
mod backend_ids;
pub mod capture;
pub mod cool_storage;
mod count;
mod entry;
mod ext_usage;
pub mod ledger;
mod ndjson;
mod pacific;
mod page;
mod page_memo;
mod page_rows;
mod poller;
mod sql;
#[cfg(test)]
mod test_support;
mod usd;

pub use capture::{capture_charliebot, capture_local, capture_runs, capture_usage, host_name, rebuild_run_records};
pub use count::Count;
pub use entry::register;
pub use ledger::{LedgerAccount, LedgerRow, UsageLedger, default_ledger_path};

#[cfg(test)]
mod tests {
  use charliebot_registry::Registry;
  use charliebot_registry::wiring::Phase;

  use super::*;

  #[test]
  fn register_mounts_the_routers_starts_the_service_lists_the_card_and_checks_the_ids() {
    let mut registry = Registry::new();
    registry.register_package("features.usage", register).expect("the package registers");
    assert_eq!(registry.routers().len(), 2);
    let services = registry.services_in(Phase::Ready);
    assert_eq!(services.iter().map(|service| service.name()).collect::<Vec<_>>(), ["ext_usage"]);
    assert!(registry.services_in(Phase::Early).is_empty());
    let cards = registry.home_cards();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0].url, "/token-usage");
    assert_eq!(registry.startup_checks().map(|(name, _)| name).collect::<Vec<_>>(), ["backend_ids"]);
  }
}
