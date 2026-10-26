//! Timestamp text of the usage sources, in the form Python's `datetime` writes it.

use anyhow::Context;
use charliebot_core::timefmt;
use time::{Duration, OffsetDateTime};

/// `datetime.fromtimestamp(seconds, tz=UTC).isoformat()`.
pub(crate) fn utc_isoformat(seconds: f64) -> anyhow::Result<String> {
  let whole = seconds.floor();
  let mut micros = ((seconds - whole) * 1e6).round_ties_even() as i64;
  let mut instant = OffsetDateTime::from_unix_timestamp(whole as i64)
    .with_context(|| format!("timestamp {seconds} is out of range"))?;
  if micros == 1_000_000 {
    instant += Duration::seconds(1);
    micros = 0;
  }
  let instant = instant
    .replace_nanosecond(u32::try_from(micros * 1_000).expect("a microsecond is below one second"))
    .context("the usage timestamp fraction is in range")?;
  Ok(timefmt::isoformat(instant))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn utc_isoformat_matches_python_for_whole_and_fractional_seconds() {
    assert_eq!(utc_isoformat(1_792_000_000.0).unwrap(), "2026-10-14T17:46:40+00:00");
    assert_eq!(utc_isoformat(1_792_000_000.5).unwrap(), "2026-10-14T17:46:40.500000+00:00");
  }
}
