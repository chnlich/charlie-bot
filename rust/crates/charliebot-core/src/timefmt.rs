//! Timestamp renderings the Python server emits.

use std::time::{SystemTime, UNIX_EPOCH};

use time::OffsetDateTime;

const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// `datetime.now(UTC).isoformat()`: `YYYY-MM-DDTHH:MM:SS.ffffff+00:00`, without the fraction when the
/// microsecond is zero (CPython's `isoformat` drops a zero fraction).
pub fn utc_now_iso() -> String {
  iso_from_system_time(SystemTime::now())
}

/// [`utc_now_iso`] of a given instant.
pub fn iso_from_system_time(instant: SystemTime) -> String {
  let moment = OffsetDateTime::from(instant);
  let micros = moment.microsecond();
  let base = format!(
    "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
    moment.year(),
    u8::from(moment.month()),
    moment.day(),
    moment.hour(),
    moment.minute(),
    moment.second()
  );
  if micros == 0 { format!("{base}+00:00") } else { format!("{base}.{micros:06}+00:00") }
}

/// `email.utils.formatdate(seconds, usegmt=True)`: `Sun, 18 Oct 2026 07:15:40 GMT`.
///
/// `time.gmtime` floors a float timestamp to whole seconds, so callers pass the floored second.
pub fn http_date(unix_seconds: i64) -> String {
  let moment = OffsetDateTime::from_unix_timestamp(unix_seconds).expect("a file mtime is a representable instant");
  format!(
    "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
    WEEKDAYS[moment.weekday().number_days_from_monday() as usize],
    moment.day(),
    MONTHS[u8::from(moment.month()) as usize - 1],
    moment.year(),
    moment.hour(),
    moment.minute(),
    moment.second()
  )
}

/// The log line timestamp: ISO 8601 UTC with microseconds and a `Z` suffix.
pub fn log_timestamp() -> String {
  let moment = OffsetDateTime::from(SystemTime::now());
  format!(
    "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}Z",
    moment.year(),
    u8::from(moment.month()),
    moment.day(),
    moment.hour(),
    moment.minute(),
    moment.second(),
    moment.microsecond()
  )
}

/// Seconds since the epoch of `instant`, as CPython's `st_mtime` float computes it from the stat pair.
pub fn python_float_seconds(seconds: i64, nanoseconds: i64) -> f64 {
  // posixmodule.c fill_time: `sec + nsec * 1e-9`, evaluated in double.
  seconds as f64 + nanoseconds as f64 * 1e-9
}

/// Whole seconds of `instant` since the epoch, floored.
pub fn unix_seconds(instant: SystemTime) -> i64 {
  match instant.duration_since(UNIX_EPOCH) {
    Ok(after) => after.as_secs() as i64,
    Err(before) => -(before.duration().as_secs_f64().ceil() as i64),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::time::Duration;

  #[test]
  fn iso_drops_a_zero_fraction_like_isoformat() {
    let whole = UNIX_EPOCH + Duration::from_secs(1_760_771_740);
    assert_eq!(iso_from_system_time(whole), "2025-10-18T07:15:40+00:00");
    let fractional = whole + Duration::from_micros(685_829);
    assert_eq!(iso_from_system_time(fractional), "2025-10-18T07:15:40.685829+00:00");
  }

  #[test]
  fn http_date_matches_formatdate_usegmt() {
    assert_eq!(http_date(1_760_771_740), "Sat, 18 Oct 2025 07:15:40 GMT");
  }
}
