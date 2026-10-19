//! Timestamp text of the usage sources, in the form Python's `datetime` writes it.

use anyhow::Context;
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
  let fraction = if micros == 0 { String::new() } else { format!(".{micros:06}") };
  Ok(format!(
    "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{fraction}+00:00",
    instant.year(),
    u8::from(instant.month()),
    instant.day(),
    instant.hour(),
    instant.minute(),
    instant.second(),
  ))
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
