//! Bounds for work performed by the Rust server.

/// The whole host-configured backend router call, below the delegate client's 30-second timeout.
pub const ROUTER_TIMEOUT_SECONDS: u64 = 25;
