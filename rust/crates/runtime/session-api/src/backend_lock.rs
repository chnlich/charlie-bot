//! Shared continuation-domain judgment for the backend switch route and its header menu.

use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_registry::Registry;
use charliebot_store::{StoreError, StoreResult};

/// `backend_types.same_continuation_domain` over member sets: each id expands to its members (a pool's members, a
/// model the set of itself) and the judgment passes only when every member shares one continuation domain, so a
/// model and the pool holding it alone get the same verdict. A member without a configured option passes nothing,
/// as an unconfigured id did.
pub(crate) fn same_continuation_domain(
  current: &str,
  target: &str,
  cfg: &CharlieBotConfig,
  registry: &Registry,
) -> StoreResult<bool> {
  let members = continuation_members(current, cfg).into_iter().chain(continuation_members(target, cfg));
  let mut shared: Option<String> = None;
  for member in members {
    let Some(option) = cfg.get_backend_option(member) else { return Ok(false) };
    let domain = continuation_domain(option, cfg, registry)?;
    match &shared {
      Some(one) if one != &domain => return Ok(false),
      Some(_) => {}
      None => shared = Some(domain),
    }
  }
  Ok(true)
}

/// The members a backend id expands to for the switch lock: a pool's member ids, or the id itself.
fn continuation_members<'a>(id: &'a str, cfg: &'a CharlieBotConfig) -> Vec<&'a str> {
  match cfg.backends.pool(id) {
    Some(pool) => pool.members.iter().map(|member| member.backend.as_str()).collect(),
    None => vec![id],
  }
}

fn continuation_domain(option: &BackendOption, cfg: &CharlieBotConfig, registry: &Registry) -> StoreResult<String> {
  let lifecycle = registry.lifecycle_for(&option.backend_type).map_err(|error| StoreError::new(error.to_string()))?;
  Ok(lifecycle.continuation_domain(option, cfg))
}
