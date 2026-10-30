//! `src/features/approval`: the approval day view (plan 4 v2, task W3) — `GET /approval` and the
//! `GET /api/approval/day` route it reads, one Pacific day's ranked T0 queue, login sheet and digest.

mod api;
mod day;
mod page;
#[cfg(test)]
mod test_support;

use charliebot_registry::{Registry, RegistryError};

/// `register`: the day API's router, then the page's router.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(api::routes, api::matches)?;
  registry.router(page::routes, page::matches)
}
