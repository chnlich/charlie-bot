//! Timestamp renderings the Python server emits.

use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

use time::OffsetDateTime;
use time::format_description::{self, BorrowedFormatItem};
use time::macros::format_description;

/// `datetime.now(UTC).isoformat()`: `YYYY-MM-DDTHH:MM:SS.ffffff+00:00`, without the fraction when the
/// microsecond is zero (CPython's `isoformat` drops a zero fraction).
pub fn utc_now_iso() -> String {
  iso_from_system_time(SystemTime::now())
}

/// [`utc_now_iso`] of a given instant.
pub fn iso_from_system_time(instant: SystemTime) -> String {
  isoformat(OffsetDateTime::from(instant))
}

const ISO_WHOLE: &[BorrowedFormatItem<'static>] =
  format_description!("[year]-[month]-[day]T[hour]:[minute]:[second][offset_hour sign:mandatory]:[offset_minute]");
const ISO_FRACTION: &[BorrowedFormatItem<'static>] = format_description!(
  "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:6]\
   [offset_hour sign:mandatory]:[offset_minute]"
);
const ISO_OFFSET_SECONDS: &[BorrowedFormatItem<'static>] = format_description!(
  "[year]-[month]-[day]T[hour]:[minute]:[second][offset_hour sign:mandatory]:[offset_minute]:[offset_second]"
);
const ISO_FRACTION_OFFSET_SECONDS: &[BorrowedFormatItem<'static>] = format_description!(
  "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:6]\
   [offset_hour sign:mandatory]:[offset_minute]:[offset_second]"
);

/// Render a datetime with Python `datetime.isoformat()`'s local clock, microsecond and UTC-offset form. The fraction
/// appears only when the microsecond is non-zero, and the offset's seconds only when they are non-zero.
pub fn isoformat(moment: OffsetDateTime) -> String {
  let format = match (moment.microsecond() != 0, moment.offset().seconds_past_minute() != 0) {
    (false, false) => ISO_WHOLE,
    (true, false) => ISO_FRACTION,
    (false, true) => ISO_OFFSET_SECONDS,
    (true, true) => ISO_FRACTION_OFFSET_SECONDS,
  };
  moment.format(format).expect("a datetime formats as isoformat")
}

/// `email.utils.formatdate(seconds, usegmt=True)`: `Sun, 18 Oct 2026 07:15:40 GMT`.
///
/// `time.gmtime` floors a float timestamp to whole seconds, so callers pass the floored second.
pub fn http_date(unix_seconds: i64) -> String {
  static HTTP_DATE: LazyLock<Vec<BorrowedFormatItem<'static>>> = LazyLock::new(|| {
    format_description::parse_borrowed::<2>(
      "[weekday repr:short], [day padding:zero] [month repr:short] [year repr:full padding:zero] \
       [hour padding:zero]:[minute padding:zero]:[second padding:zero] GMT",
    )
    .expect("the HTTP date format description is valid")
  });
  let moment = OffsetDateTime::from_unix_timestamp(unix_seconds).expect("a file mtime is a representable instant");
  moment.format(HTTP_DATE.as_slice()).expect("an HTTP date formats")
}

/// The log line's time: UTC to the millisecond, `YYYY-MM-DD HH:MM:SS.mmmZ`, the stamp `logline` reads.
pub fn log_timestamp() -> String {
  let moment = OffsetDateTime::from(SystemTime::now());
  format!(
    "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}Z",
    moment.year(),
    u8::from(moment.month()),
    moment.day(),
    moment.hour(),
    moment.minute(),
    moment.second(),
    moment.millisecond()
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
  fn isoformat_keeps_the_local_offset_and_six_digit_fraction() {
    let utc = OffsetDateTime::from_unix_timestamp(1_760_771_740).unwrap();
    let local = utc.to_offset(time::UtcOffset::from_hms(-7, 0, 0).unwrap());
    assert_eq!(isoformat(local), "2025-10-18T00:15:40-07:00");
    let fractional = local.replace_microsecond(685_829).unwrap();
    assert_eq!(isoformat(fractional), "2025-10-18T00:15:40.685829-07:00");
  }

  #[test]
  fn isoformat_writes_offset_seconds_only_when_they_are_present() {
    let utc = OffsetDateTime::from_unix_timestamp(1_760_771_740).unwrap();
    let local = utc.to_offset(time::UtcOffset::from_hms(5, 30, 15).unwrap());
    assert_eq!(isoformat(local), "2025-10-18T12:45:55+05:30:15");
    let fractional = local.replace_microsecond(685_829).unwrap();
    assert_eq!(isoformat(fractional), "2025-10-18T12:45:55.685829+05:30:15");
  }

  #[test]
  fn http_date_matches_formatdate_usegmt() {
    assert_eq!(http_date(1_760_771_740), "Sat, 18 Oct 2025 07:15:40 GMT");
    assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
    assert_eq!(http_date(951_782_400), "Tue, 29 Feb 2000 00:00:00 GMT");
    assert_eq!(http_date(1_699_142_400), "Sun, 05 Nov 2023 00:00:00 GMT");
    assert_eq!(http_date(1_699_228_799), "Sun, 05 Nov 2023 23:59:59 GMT");
    assert_eq!(http_date(1_704_067_200), "Mon, 01 Jan 2024 00:00:00 GMT");
    assert_eq!(http_date(4_102_444_799), "Thu, 31 Dec 2099 23:59:59 GMT");
  }
}
