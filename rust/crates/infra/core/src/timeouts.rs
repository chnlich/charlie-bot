//! Bounds for work performed by the Rust server.

/// The whole host-configured backend router call, below the delegate client's 30-second timeout.
pub const ROUTER_TIMEOUT_SECONDS: u64 = 25;

/// One HTTP attempt to Jev in the backend router's decision (`charliebot router`).
pub const ROUTER_JEV_REQUEST_TIMEOUT: u64 = 5;
/// The pause before the backend router's single retry after an HTTP 429.
pub const ROUTER_JEV_RATE_LIMIT_WAIT: u64 = 10;

// The router's worst case is a first attempt whose HTTP 429 arrives at its full 5 s bound. It then waits
// 10 s before the single retry, and the retry may use its full 5 s. The caller bounds the whole command at
// ROUTER_TIMEOUT_SECONDS, so the sum must stay below it.
const _: () = assert!(
  2 * ROUTER_JEV_REQUEST_TIMEOUT + ROUTER_JEV_RATE_LIMIT_WAIT < ROUTER_TIMEOUT_SECONDS,
  "the backend router's 429 + wait + retry must stay inside ROUTER_TIMEOUT_SECONDS"
);
