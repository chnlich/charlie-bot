//! The Codex token-usage JSON field names.
//!
//! A `turn.completed` event carries a `usage` object, and a rollout's `token_count` event carries a `last_token_usage`
//! object and a cumulative `total_token_usage` object. All three objects name the same token counts, and the
//! translator, the rollout reader, the usage-log reader, the quota account and the pricing table read them. One module
//! owns the names, so a change reaches every reader.

/// The prompt tokens a usage object counts.
pub(crate) const INPUT_TOKENS: &str = "input_tokens";

/// The share of the input tokens that the provider served from its cache.
pub(crate) const CACHED_INPUT_TOKENS: &str = "cached_input_tokens";

/// The completion tokens a usage object counts.
pub(crate) const OUTPUT_TOKENS: &str = "output_tokens";

/// The usage object of the latest request in a rollout's `token_count` event.
pub(crate) const LAST_TOKEN_USAGE: &str = "last_token_usage";

/// The cumulative usage of a rollout's session.
pub(crate) const TOTAL_TOKEN_USAGE: &str = "total_token_usage";
