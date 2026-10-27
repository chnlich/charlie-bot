//! The session store, opened with the registry's metadata slots and the hooks the registry answers.

use std::sync::Arc;

use charliebot_core::config::CharlieBotConfig;
use charliebot_registry::Registry;
use charliebot_registry::store_hooks::store_hooks;
use charliebot_store::Store;

/// Open the store with the slots the registered packages declare, in registration order.
pub fn create(cfg: CharlieBotConfig, registry: Arc<Registry>) -> Result<Store, String> {
  let slots = registry.metadata_slots();
  Store::new(cfg, slots, store_hooks(registry)).map_err(|error| error.to_string())
}
