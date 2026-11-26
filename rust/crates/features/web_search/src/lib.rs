//! The web search an agent runs through the server: `POST /api/web-search/mcp`, an MCP streamable-HTTP endpoint with
//! one tool, `web_search`.
//!
//! The endpoint backends disallow Claude Code's built-in web search, because their endpoint either refuses it or
//! answers without a search. This endpoint serves the search instead: Google's `google_search` grounding on the model
//! and endpoint of the `backends.calls.web_search` option, with each grounding redirect resolved to its real URL. The
//! endpoint sits under the server's access gate, and the `--mcp-config` that mounts it carries a run token, so no
//! credential travels in an argument.

mod endpoint;
mod entry;
mod mcp;
mod search;

pub use entry::register;

#[cfg(test)]
mod tests;
