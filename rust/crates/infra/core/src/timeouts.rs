//! Bounds for work performed by the Rust server.

/// The whole host-configured backend router call, below the delegate client's 30-second timeout.
pub const ROUTER_TIMEOUT_SECONDS: u64 = 25;

/// One HTTP attempt to Jev in the backend router's decision (`charliebot router`).
pub const ROUTER_JEV_REQUEST_TIMEOUT: u64 = 5;
/// The pause before the backend router's single retry after an HTTP 429.
pub const ROUTER_JEV_RATE_LIMIT_WAIT: u64 = 10;

// The router's worst case is one attempt, the rate-limit wait and the retry (5 + 10 + 5); the caller
// bounds the whole command at ROUTER_TIMEOUT_SECONDS, so the sum must stay below it.
const _: () = assert!(
  2 * ROUTER_JEV_REQUEST_TIMEOUT + ROUTER_JEV_RATE_LIMIT_WAIT < ROUTER_TIMEOUT_SECONDS,
  "the backend router's attempt + wait + attempt must stay inside ROUTER_TIMEOUT_SECONDS"
);
