//! `web/features/diff_view`: the `/diff` page and the `/api/git` routes it calls. The routes read a repository's refs
//! and diffs through `git`. Two memos keep the diff manifest of each resolved range and the branch list of each repo,
//! so a repeat view runs no diff.

// The helpers answer a failure as the HTTP response it sends, as `charliebot_registry::caller` does, so their
// `Err` is a `Response`.
#![allow(clippy::result_large_err)]

mod api;
mod entry;
mod git;
mod manifest;
mod memo;
mod page;
mod params;
mod refs;
mod repos;

pub use entry::register;

#[cfg(test)]
mod tests;
