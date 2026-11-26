//! The usage package's registrations: the routers, the ext-usage service, the home card, the scheduled handlers and the
//! backend ids check.

use std::sync::Arc;

use charliebot_registry::context::FeatureContext;
use charliebot_registry::pages::HomeCard;
use charliebot_registry::{Registry, RegistryError};
use futures_util::FutureExt;

use crate::capture::capture_into_default_ledger;
use crate::{api, backend_ids, cool_storage, page, poller};

/// `register`: the `/api` router of `ext_usage`, the page router, the `ext_usage`
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
