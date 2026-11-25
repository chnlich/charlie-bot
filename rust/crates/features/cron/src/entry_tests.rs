//! The registration entry, exercised through a registry: a scheduled handler registers once, and its name resolves lazily.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use charliebot_registry::Registry;
use charliebot_registry::context::FeatureContext;
use futures_util::future::BoxFuture;

use crate::register;

static HANDLER_POLLS: AtomicUsize = AtomicUsize::new(0);

fn handler(_features: Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<String>> {
  Box::pin(async {
    HANDLER_POLLS.fetch_add(1, Ordering::SeqCst);
    Ok("synthetic result".to_string())
  })
}

fn register_handler(registry: &mut Registry) -> Result<(), charliebot_registry::RegistryError> {
  registry.scheduled_handler("synthetic", handler)
}

fn register_duplicate_handler(registry: &mut Registry) -> Result<(), charliebot_registry::RegistryError> {
  registry.scheduled_handler("synthetic", handler)
}

#[test]
fn scheduled_handler_registration_keeps_name_and_lazy_lookup_rules() {
  HANDLER_POLLS.store(0, Ordering::SeqCst);
  let mut registry = Registry::new();
  registry.register_package("test.cron", register).unwrap();
  assert!(registry.scheduled_handler_named("synthetic").is_none());
  registry.register_package("test.handler", register_handler).unwrap();
  assert!(registry.scheduled_handler_named("synthetic").is_some());
  assert_eq!(HANDLER_POLLS.load(Ordering::SeqCst), 0);
  assert!(registry.register_package("test.duplicate", register_duplicate_handler).is_err());
  assert_eq!(HANDLER_POLLS.load(Ordering::SeqCst), 0);
}
