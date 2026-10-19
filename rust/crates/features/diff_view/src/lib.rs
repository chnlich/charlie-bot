//! `src/features/diff_view`: the `/diff` page and the `/api/git` routes it calls. The routes read a repository's refs
//! and diffs through `git`. Two memos keep the diff manifest of each resolved range and the branch list of each repo,
//! so a repeat view runs no diff.

// The helpers answer a failure as the HTTP response it sends, as `charliebot_registry::caller` does, so their
// `Err` is a `Response`.
#![allow(clippy::result_large_err)]

mod api;
mod git;
mod manifest;
mod memo;
mod page;
mod params;
mod refs;
mod repos;
#[cfg(test)]
mod tests;

use charliebot_registry::pages::HomeCard;
use charliebot_registry::{Registry, RegistryError};

/// `register` (`src/features/diff_view/__init__.py`): the `/api/git` router, the page router, then the home card.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(api::routes, api::matches)?;
  registry.router(page::routes, page::matches)?;
  registry.home_card(HomeCard {
    name: "Diff viewer",
    url: "/diff",
    description: "Browse a repository diff between two refs.",
  })
}
