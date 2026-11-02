//! The startup segments of one server start, from process start to `server_ready`.
//!
//! A segment is one stretch of the startup path, such as the store open, one pre-recovery step or one service start.
//! [`StartupClock::segment`] ends the open segment and logs `startup_segment segment=<name> elapsed_ms=<ms> at_ms=<ms>`.
//! The next segment starts at that instant, so the segments leave no gap between them.
//!
//! `at_ms` is the segment's end, in milliseconds since the clock started, rounded half to even. `elapsed_ms` is the
//! difference between two consecutive `at_ms` values, so the `elapsed_ms` values of one start sum to its last `at_ms`.
//! The server logs that last `at_ms` as `ready_in_ms` on `server_ready`.
//!
//! The recovery steps run as tasks while the early services start. A `recovery:<step>` segment is therefore the time
//! the startup path waits for that step after the services before it have started, and the step's own log line (such
//! as `task_tree_recovery_done elapsed_ms`) gives its full run time.

use std::time::{Duration, Instant};

/// The clock of one server start. The server starts it where `ready_in_ms` begins and passes it down the startup path.
pub struct StartupClock {
  started: Instant,
  last_at_ms: u64,
}

impl StartupClock {
  /// A clock whose segments count from `started`.
  pub fn new(started: Instant) -> Self {
    Self { started, last_at_ms: 0 }
  }

  /// Ends the open segment as `name`, logs it, and returns its `at_ms`.
  pub fn segment(&mut self, name: &str) -> u64 {
    let at_ms = round_ms(self.started.elapsed());
    let elapsed_ms = at_ms - self.last_at_ms;
    self.last_at_ms = at_ms;
    tracing::info!(segment = name, elapsed_ms, at_ms, "startup_segment");
    at_ms
  }
}

/// Python's `round(seconds * 1000)` of a duration: milliseconds, half to even. Rounding the boundaries keeps each
/// `elapsed_ms` a difference of rounded values, so the sum of a start's segments equals its `ready_in_ms` exactly.
fn round_ms(duration: Duration) -> u64 {
  (duration.as_secs_f64() * 1000.0).round_ties_even() as u64
}
