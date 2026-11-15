//! The UTC timestamp value type that session metadata, run records and backend usage panels carry.
//!
//! The store writes these timestamps into session files, and a backend builds and reads them in its usage panel, so
//! the type sits in the foundation both sides already depend on.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Datetime stored as UTC with a `Z` suffix and microsecond format.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct UtcDateTime(OffsetDateTime);

impl UtcDateTime {
  /// Return the current UTC time, truncated to microseconds.
  pub fn now() -> Self {
    let now = OffsetDateTime::now_utc();
    let nanos = (now.nanosecond() / 1_000) * 1_000;
    Self(now.replace_nanosecond(nanos).expect("a microsecond fits in the current second"))
  }

  /// Parse an ISO 8601 datetime; treat an offset-less value as UTC.
  pub fn parse(value: &str) -> Result<Self, time::error::Parse> {
    let normalized = value.replace(' ', "T");
    let time_part = normalized.split_once('T').map_or("", |(_, time)| time);
    let has_offset = time_part.contains('+') || time_part.contains('-');
    let with_utc = if normalized.ends_with('Z') || has_offset { normalized } else { format!("{normalized}Z") };
    OffsetDateTime::parse(&with_utc, &Rfc3339).map(|datetime| Self(datetime.to_offset(time::UtcOffset::UTC)))
  }

  /// Render the UTC datetime with microsecond precision.
  pub fn to_str(self) -> String {
    let value = self.0.to_offset(time::UtcOffset::UTC);
    let base = format!(
      "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
      value.year(),
      u8::from(value.month()),
      value.day(),
      value.hour(),
      value.minute(),
      value.second()
    );
    let microseconds = value.microsecond();
    if microseconds == 0 { format!("{base}Z") } else { format!("{base}.{microseconds:06}Z") }
  }

  /// Render the same UTC instant with a `+00:00` suffix.
  pub fn to_isoformat_text(self) -> String {
    format!("{}+00:00", self.to_str().trim_end_matches('Z'))
  }

  /// Return the underlying UTC instant.
  pub fn into_inner(self) -> OffsetDateTime {
    self.0
  }
}

impl From<OffsetDateTime> for UtcDateTime {
  /// Wrap one instant, normalized to UTC as the stored form is.
  fn from(value: OffsetDateTime) -> Self {
    Self(value.to_offset(time::UtcOffset::UTC))
  }
}

impl Serialize for UtcDateTime {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    serializer.serialize_str(&self.to_str())
  }
}

impl<'de> Deserialize<'de> for UtcDateTime {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    let value = String::deserialize(deserializer)?;
    Self::parse(&value).map_err(serde::de::Error::custom)
  }
}

impl fmt::Display for UtcDateTime {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(&self.to_str())
  }
}

#[cfg(test)]
mod tests {
  use super::UtcDateTime;

  #[test]
  fn the_stored_form_keeps_microseconds_and_omits_a_zero_fraction() {
    let whole = UtcDateTime::parse("2026-11-03T09:53:21Z").expect("a whole second parses");
    assert_eq!(whole.to_str(), "2026-11-03T09:53:21Z");
    let fractional = UtcDateTime::parse("2026-11-03T09:53:21.751733Z").expect("a microsecond value parses");
    assert_eq!(fractional.to_str(), "2026-11-03T09:53:21.751733Z");
  }

  #[test]
  fn an_offset_less_value_reads_as_utc_and_the_isoformat_text_reads_back() {
    let bare = UtcDateTime::parse("2026-11-03 09:53:21").expect("an offset-less value parses");
    assert_eq!(bare, UtcDateTime::parse("2026-11-03T09:53:21Z").expect("the same instant with a Z parses"));
    let fractional = UtcDateTime::parse("2026-11-03T09:53:21.5Z").expect("a fraction parses");
    assert_eq!(fractional.to_isoformat_text(), "2026-11-03T09:53:21.500000+00:00");
    assert_eq!(UtcDateTime::parse(&fractional.to_isoformat_text()).expect("the text parses back"), fractional);
  }
}
