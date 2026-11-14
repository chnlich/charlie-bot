//! The connection timing every chat channel listener shares: the wait a close allows the peer's reply, and the delay
//! before a refused or dropped connection retries. A channel's crate keeps its own connect and reconnect loop.

use std::time::Duration;

/// The wait the sender allows the peer's close reply before the connection is abandoned.
pub const CLOSE_TIMEOUT: Duration = Duration::from_secs(1);
/// The delay before the first retry of a refused or dropped connection.
pub const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
/// The cap on the retry delay.
pub const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// The delay that follows `backoff`: doubled, and no longer than [`MAX_BACKOFF`].
pub fn next_backoff(backoff: Duration) -> Duration {
  backoff.saturating_mul(2).min(MAX_BACKOFF)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_retry_delay_doubles_from_the_first_delay_to_its_cap() {
    let mut backoff = INITIAL_BACKOFF;
    let mut seconds = vec![backoff.as_secs()];
    for _ in 0..7 {
      backoff = next_backoff(backoff);
      seconds.push(backoff.as_secs());
    }
    assert_eq!(seconds, [1, 2, 4, 8, 16, 30, 30, 30]);
  }
}
