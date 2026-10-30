//! The timestamp forms of `host_auth.py`: `datetime.isoformat()` and `fromisoformat()` for the aware values the state
//! file holds, and `astimezone().strftime(...)` for the page's renderings in the server's own zone.

use std::ffi::CStr;

use anyhow::Context;
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};

/// `models.utc_now()`: `datetime.now(UTC)`, which carries microseconds and no finer.
pub fn utc_now() -> OffsetDateTime {
  let now = OffsetDateTime::now_utc();
  now.replace_nanosecond(now.microsecond() * 1_000).expect("a microsecond count is a valid nanosecond count")
}

/// `datetime.isoformat()`: the fraction appears only when the microsecond is not zero.
pub use charliebot_core::timefmt::isoformat;

/// `datetime.fromisoformat()` for a value with an offset. Naive text is refused, since Python cannot subtract a
/// naive value from the aware clock either.
pub fn parse(text: &str) -> anyhow::Result<OffsetDateTime> {
  OffsetDateTime::parse(text, &Rfc3339).with_context(|| format!("{text:?} is not an ISO timestamp with an offset"))
}

/// The page's two renderings of a stored timestamp.
#[derive(Clone, Copy)]
pub enum LocalFormat {
  /// `%Y-%m-%d %H:%M %Z`
  DateZone,
  /// `%H:%M`
  Clock,
}

/// `_local_timestamp`: the stored timestamp in the server's zone, or `None` when it is absent.
pub fn local_timestamp(moment: Option<&str>, format: LocalFormat) -> anyhow::Result<Option<String>> {
  moment.map(|text| parse(text).map(|moment| local_text(moment, format))).transpose()
}

fn local_text(moment: OffsetDateTime, format: LocalFormat) -> String {
  let (offset, zone) = local_zone(moment.unix_timestamp());
  let local = moment.to_offset(UtcOffset::from_whole_seconds(offset).expect("a zone offset is within a day"));
  match format {
    LocalFormat::DateZone => format!(
      "{:04}-{:02}-{:02} {:02}:{:02} {zone}",
      local.year(),
      u8::from(local.month()),
      local.day(),
      local.hour(),
      local.minute()
    ),
    LocalFormat::Clock => format!("{:02}:{:02}", local.hour(), local.minute()),
  }
}

/// The server zone's offset in seconds and abbreviation at `unix_seconds`. `localtime_r` gives the same two fields
/// CPython's `astimezone()` reads, and `%Z` prints the abbreviation.
fn local_zone(unix_seconds: i64) -> (i32, String) {
  let time = unix_seconds as libc::time_t;
  // SAFETY: `tm` holds integers and pointers only, so the zeroed value is valid, and `localtime_r` fills the whole of
  // it. The call reads `time` for its duration only.
  let mut tm: libc::tm = unsafe { std::mem::zeroed() };
  let filled = unsafe { libc::localtime_r(&time, &mut tm) };
  assert!(!filled.is_null(), "localtime_r converts every instant the page renders");
  // SAFETY: a successful `localtime_r` points `tm_zone` at a NUL-terminated name in the C library's zone table, which
  // lives for the process.
  let zone = unsafe { CStr::from_ptr(tm.tm_zone) }.to_string_lossy().into_owned();
  (tm.tm_gmtoff as i32, zone)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn isoformat_drops_a_zero_fraction_and_keeps_the_offset() {
    let whole = parse("2026-10-18T09:00:00+00:00").expect("the stamp parses");
    assert_eq!(isoformat(whole), "2026-10-18T09:00:00+00:00");
    let fractional = parse("2026-10-18T09:00:00.000125+00:00").expect("the stamp parses");
    assert_eq!(isoformat(fractional), "2026-10-18T09:00:00.000125+00:00");
  }

  #[test]
  fn text_without_an_offset_is_refused() {
    assert!(parse("2026-10-18T09:00:00").is_err());
  }

  #[test]
  fn the_clock_rendering_is_hours_and_minutes() {
    let text = local_timestamp(Some("2026-10-18T09:05:00+00:00"), LocalFormat::Clock).expect("the stamp renders");
    let text = text.expect("a stamp renders to text");
    assert_eq!(text.len(), 5);
    assert_eq!(text.as_bytes()[2], b':');
  }
}
