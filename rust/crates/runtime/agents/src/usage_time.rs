//! Timestamp text of the usage sources, in the form the sources write it.

use anyhow::Context;
use charliebot_core::text_repr::{is_truthy, value_str};
use charliebot_core::timefmt;
use serde_json::{Map, Value};
use time::{Duration, OffsetDateTime};

/// The `timestamp` field of a usage source's line, as text, or the empty string when the field is missing or falsy.
pub fn timestamp_of(line: &Map<String, Value>) -> String {
  match line.get("timestamp") {
    Some(timestamp) if is_truthy(timestamp) => value_str(timestamp),
    _ => String::new(),
  }
}

/// The UTC time of *seconds*, in ISO 8601 text with microseconds.
pub fn utc_isoformat(seconds: f64) -> anyhow::Result<String> {
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
  fn timestamp_of_reads_a_present_timestamp_and_empties_a_missing_or_falsy_one() {
    let with = |timestamp: Value| serde_json::json!({ "timestamp": timestamp }).as_object().unwrap().clone();
    assert_eq!(timestamp_of(&with(serde_json::json!("2026-10-14T17:46:40Z"))), "2026-10-14T17:46:40Z");
    assert_eq!(timestamp_of(&Map::new()), "");
    assert_eq!(timestamp_of(&with(Value::Null)), "");
    assert_eq!(timestamp_of(&with(serde_json::json!(""))), "");
  }

  #[test]
  fn utc_isoformat_matches_for_whole_and_fractional_seconds() {
    assert_eq!(utc_isoformat(1_792_000_000.0).unwrap(), "2026-10-14T17:46:40+00:00");
    assert_eq!(utc_isoformat(1_792_000_000.5).unwrap(), "2026-10-14T17:46:40.500000+00:00");
  }
}
