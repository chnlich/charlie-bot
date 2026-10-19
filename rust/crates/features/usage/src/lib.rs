//! `src/features/usage`: the quota panel and usage ledger capture are ported. The usage page and cold-storage sweep
//! are pending, and the home card stays registered.

mod api;
pub mod capture;
mod count;
mod ext_usage;
pub mod ledger;
mod ndjson;
mod poller;
mod sql;

use std::sync::Arc;

use charliebot_registry::context::FeatureContext;
use charliebot_registry::pages::HomeCard;
use charliebot_registry::{Registry, RegistryError};
use futures_util::FutureExt;

pub use capture::{capture_charliebot, capture_local, capture_runs, capture_usage};
pub use count::Count;
pub use ledger::{LedgerAccount, LedgerRow, UsageLedger, default_ledger_path};

/// `register` (`src/features/usage/__init__.py`): the `/api` router of `ext_usage`, the `ext_usage` service in phase
/// Ready, the home card, then the `usage_ledger` scheduled handler.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(api::routes, api::matches)?;
  registry.service(Arc::new(poller::ExtUsagePoller::default()))?;
  registry.home_card(HomeCard {
    name: "Token usage by model",
    url: "/token-usage",
    description: "Tokens per model across every agent log on this host.",
  })?;
  registry.scheduled_handler("usage_ledger", run_usage_ledger)
}

fn run_usage_ledger(context: Arc<FeatureContext>) -> futures_util::future::BoxFuture<'static, anyhow::Result<String>> {
  async move {
    let context_for_capture = Arc::clone(&context);
    let written = tokio::task::spawn_blocking(move || {
      let config = context_for_capture.config.get()?;
      let path = default_ledger_path(&config.charliebot_home);
      let mut ledger = UsageLedger::open(path)?;
      capture_local(&mut ledger, config.as_ref(), context_for_capture.registry.as_ref())
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
  fn register_mounts_the_router_starts_the_service_and_lists_the_card() {
    let mut registry = Registry::new();
    registry.register_package("src.features.usage", register).expect("the package registers");
    assert_eq!(registry.routers().len(), 1);
    let services = registry.services_in(Phase::Ready);
    assert_eq!(services.iter().map(|service| service.name()).collect::<Vec<_>>(), ["ext_usage"]);
    assert!(registry.services_in(Phase::Early).is_empty());
    let cards = registry.home_cards();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0].url, "/token-usage");
  }
}
