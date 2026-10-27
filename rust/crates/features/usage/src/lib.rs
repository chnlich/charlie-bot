//! `src/features/usage`: the quota panel, the usage ledger capture, the token usage page, the cold-storage sweep and the
//! `backend_ids` startup check are ported. The home card stays registered.

mod api;
mod backend_ids;
pub mod capture;
pub mod cool_storage;
mod count;
mod ext_usage;
pub mod ledger;
mod ndjson;
mod page;
mod page_rows;
mod poller;
mod sql;
#[cfg(test)]
mod test_support;
mod utc_datetime;

use std::sync::Arc;

use charliebot_registry::context::FeatureContext;
use charliebot_registry::pages::HomeCard;
use charliebot_registry::{Registry, RegistryError};
use futures_util::FutureExt;

use crate::capture::capture_into_default_ledger;

pub use capture::{capture_charliebot, capture_local, capture_runs, capture_usage};
pub use count::Count;
pub use ledger::{LedgerAccount, LedgerRow, UsageLedger, default_ledger_path};

/// `register` (`src/features/usage/__init__.py`): the `/api` router of `ext_usage`, the page router, the `ext_usage`
/// service in phase Ready, the home card, the `usage_ledger` and `cool_storage` scheduled handlers, then the
/// `backend_ids` startup check.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(api::routes, api::matches)?;
  registry.router(page::routes, page::matches)?;
  registry.service(Arc::new(poller::ExtUsagePoller::default()))?;
  registry.home_card(HomeCard {
    name: "Token usage by model",
    url: "/token-usage",
    description: "Tokens per model across every agent log on this host.",
  })?;
  registry.scheduled_handler("usage_ledger", run_usage_ledger)?;
  registry.scheduled_handler("cool_storage", cool_storage::run_handler)?;
  registry.startup_check("backend_ids", backend_ids::check_backend_ids)
}

fn run_usage_ledger(context: Arc<FeatureContext>) -> futures_util::future::BoxFuture<'static, anyhow::Result<String>> {
  async move {
    let context_for_capture = Arc::clone(&context);
    let written = tokio::task::spawn_blocking(move || {
      let config = context_for_capture.config.get()?;
      capture_into_default_ledger(config.as_ref(), context_for_capture.registry.as_ref())
    })
    .await??;
    let summary = written.iter().map(|(source, count)| format!("{source} {count}")).collect::<Vec<_>>().join("; ");
    let sources = written
      .iter()
      .map(|(source, count)| (source.clone(), serde_json::Value::from(*count)))
      .collect::<serde_json::Map<_, _>>();
    tracing::info!(sources = ?sources, "usage_ledger_handler_done");
    Ok(summary)
  }
  .boxed()
}

#[cfg(test)]
mod tests {
  use charliebot_registry::wiring::Phase;

  use super::*;

  #[test]
  fn register_mounts_the_routers_starts_the_service_lists_the_card_and_checks_the_ids() {
    let mut registry = Registry::new();
    registry.register_package("src.features.usage", register).expect("the package registers");
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
