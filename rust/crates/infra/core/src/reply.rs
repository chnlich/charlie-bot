//! The reply budget: a reply over the budget still posts, and its readback sets `over_budget` so the agent can move
//! the full answer to a page.
//!
//! The rule lives here because the crates that measure a reply — `features/chat_threads` for the chat channels and
//! `features/web_reply` for the web channel — may depend only on infra and runtime, so neither can hold the rule for
//! the other. `prompts/web_reply.md` states the same budget to the agent.

/// The readback key that flags a reply whose text is over the budget.
pub const OVER_BUDGET_FIELD: &str = "over_budget";

/// The reply budget in characters: `prompts/web_reply.md` tells the agent to put a longer answer on a page.
pub const REPLY_BUDGET_CHARS: usize = 500;

/// Whether a reply of `chars` characters is over the budget.
pub fn is_over_budget(chars: usize) -> bool {
  chars > REPLY_BUDGET_CHARS
}
