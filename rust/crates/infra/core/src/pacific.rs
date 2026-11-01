//! The Pacific calendar day (`src/infra/pacific.py` semantics in the ported tree): the day the approval digest, the
//! approval day view and the standing policies' caps and expiries read by. Every approval date is a
//! America/Los_Angeles calendar day, whatever zone the process runs in.

use jiff::Timestamp;
use jiff::civil::Date;
use jiff::tz::TimeZone;

/// The house time zone (`config.HOUSE_TIMEZONE`): the zone every approval day reads in, and the default of a scheduled
/// task that names none.
pub const ZONE: &str = "America/Los_Angeles";

/// A Pacific-day read that refused (`pacific.PacificError`).
#[derive(Debug)]
pub struct PacificError(String);

impl std::fmt::Display for PacificError {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter.write_str(&self.0)
  }
}

impl std::error::Error for PacificError {}

/// The America/Los_Angeles zone (`pacific.zone`).
pub fn zone() -> Result<TimeZone, PacificError> {
  TimeZone::get(ZONE).map_err(|error| PacificError(format!("loading the time zone {ZONE}: {error}")))
}

/// The Pacific calendar day an instant falls in (`pacific.day_of`).
pub fn day_of(moment: Timestamp) -> Result<Date, PacificError> {
  Ok(moment.to_zoned(zone()?).date())
}

/// The Pacific calendar day a stored stamp holds (`pacific.day_of_stamp`). The stamps the server and the CLI write are
/// UTC ISO forms with an offset or a `Z` suffix and an optional fraction; a stamp outside those forms names no day.
pub fn day_of_stamp(text: &str) -> Result<Date, PacificError> {
  let moment: Timestamp =
    text.parse().map_err(|error| PacificError(format!("stored stamp {text:?} is not an instant: {error}")))?;
  day_of(moment)
}

/// The `YYYY-MM-DD` text of the Pacific day a stored stamp holds: `day_of_stamp` through `date_text`.
pub fn day_text_of_stamp(text: &str) -> Result<String, PacificError> {
  day_of_stamp(text).map(date_text)
}

/// The `YYYY-MM-DD` text of a Pacific day, the form the day API's `date` query and the standing policies' `expires`
/// dates use. ISO dates compare as text the way they compare as dates.
pub fn date_text(date: Date) -> String {
  date.to_string()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_utc_stamp_names_its_pacific_day() {
    // 06:59Z on 10-26 is still 10-25 in Los Angeles; 07:00Z is 10-26 (PDT, UTC-7).
    assert_eq!(day_of_stamp("2026-10-26T06:59:59.123456+00:00").unwrap().to_string(), "2026-10-25");
    assert_eq!(day_of_stamp("2026-10-26T07:00:00Z").unwrap().to_string(), "2026-10-26");
    // A PST winter stamp: 08:00Z is midnight in Los Angeles (UTC-8).
    assert_eq!(day_of_stamp("2026-11-05T08:00:00+00:00").unwrap().to_string(), "2026-11-05");
    assert_eq!(day_of_stamp("2026-11-05T07:59:59Z").unwrap().to_string(), "2026-11-04");
  }

  #[test]
  fn a_non_instant_stamp_names_no_day() {
    assert!(day_of_stamp("2026-10-26").is_err());
    assert!(day_of_stamp("not a stamp").is_err());
  }

  #[test]
  fn iso_dates_compare_as_text_the_way_they_compare_as_dates() {
    assert!(date_text("2026-11-15".parse().unwrap()).as_str() > "2026-11-14");
    assert!(date_text("2026-11-15".parse().unwrap()).as_str() <= "2026-11-15");
    assert!(date_text("2027-01-01".parse().unwrap()).as_str() > "2026-12-31");
  }
}
