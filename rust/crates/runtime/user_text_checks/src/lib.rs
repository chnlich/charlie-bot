//! The shared checks for user-facing page and reply text, with their compiled pattern config.

mod api;
mod checks;
mod config;
mod entry;
mod links;
mod markup;
mod page;
mod sessions;
#[cfg(test)]
mod tests;
mod text;

pub use api::{
  CHECK_NAMES, COMMIT_NAMED, LINK_RENDERABLE, RULE_DEFINED_TERMS, SESSION_LINKED, TRACKER_ID_NEARBY, TextViolation,
  UserTextChecks, VOCABULARY_RULE_TERM, page_failure_detail, reply_refusal_detail,
};
pub use config::UserTextChecksConfig;
pub use entry::{CONFIG_SECTION, config_section, register};
pub use links::{PageLinkTarget, check_page_link};
