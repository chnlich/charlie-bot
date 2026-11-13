//! The Codex token-usage JSON field names.
//!
//! A `turn.completed` event carries a `usage` object, and a rollout's `token_count` event carries a `last_token_usage`
//! object and a cumulative `total_token_usage` object. All three objects name the same token counts, and the
//! translator, the rollout reader, the usage-log reader, the quota account and the pricing table read them. One module
//! owns the names, so a change reaches every reader.
//!
//! The prompt, completion and cached counts are `charliebot_core::event_types` names, which the Claude and usage-page
//! readers spell from there too; only the names Codex alone uses are defined here.

/// The token counts of a usage object, under the names their owner defines.
pub(crate) use charliebot_core::event_types::{
  USAGE_CACHED_INPUT_TOKENS as CACHED_INPUT_TOKENS, USAGE_INPUT_TOKENS as INPUT_TOKENS,
  USAGE_OUTPUT_TOKENS as OUTPUT_TOKENS,
};

/// The usage object of the latest request in a rollout's `token_count` event.
pub(crate) const LAST_TOKEN_USAGE: &str = "last_token_usage";

/// The cumulative usage of a rollout's session.
pub(crate) const TOTAL_TOKEN_USAGE: &str = "total_token_usage";
