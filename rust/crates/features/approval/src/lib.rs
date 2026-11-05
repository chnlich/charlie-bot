//! The approval day view feature (`web/features/approval`): the page at `GET /approval` and the route the page
//! reads, `GET /api/approval/day`. Both cover one Pacific day's ranked T0 queue, login sheet and digest.

mod api;
mod day;
mod page;
#[cfg(test)]
mod test_support;

use charliebot_registry::{Registry, RegistryError};

/// The feature's registration: the day API's router, then the page's router.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(api::routes, api::matches)?;
  registry.router(page::routes, page::matches)
}
