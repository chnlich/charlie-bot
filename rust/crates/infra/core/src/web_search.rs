//! The server's web search MCP endpoint, which the agent commands mount and `features/web_search` serves.

/// The route of the MCP endpoint. `features/web_search` registers the route, and `backends/claude_code` mounts it
/// with `--mcp-config`; the path lives here because a backend may depend only on infra and runtime.
pub const MCP_PATH: &str = "/api/web-search/mcp";

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_path_is_the_mounted_route() {
    assert_eq!(MCP_PATH, "/api/web-search/mcp");
  }
}
