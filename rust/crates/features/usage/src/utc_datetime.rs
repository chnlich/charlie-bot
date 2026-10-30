//! `models.parse_utc_datetime` for the `updated_at` text the cold rule reads: `datetime.fromisoformat`, with a value that
//! names no offset read as UTC (`ensure_utc`).
//!
//! The grammar follows the C parser of CPython 3.14, which `datetime` runs. A calendar date is `YYYY-MM-DD` or `YYYYMMDD`,
//! and one separator character may follow it before a time. The time is `HH`, `HH:MM` or `HH:MM:SS`, or the basic `HHMM`
//! and `HHMMSS`, with an optional `.` or `,` fraction of any length, read to microseconds. An offset is `Z`, `±HH`,
//! `±HHMM` or `±HH:MM`, with optional seconds. Hour 24 at zero minutes is the next day's midnight.
//!
//! Three parts of that grammar are not ported: ISO week dates, the parser's acceptance of a stray final character after
//! the seconds, and a fraction of a second in an offset, which `UtcOffset` cannot hold. Text that uses one reads as None,
//! so its session is not cold and keeps its files.

use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};

/// The instant `text` names, or None where `fromisoformat` raises `ValueError`.
pub(crate) fn parse(text: &str) -> Option<OffsetDateTime> {
  if !text.is_ascii() {
    return None;
  }
  let bytes = text.as_bytes();
  // A dash at index 4 makes the date 10 characters (`YYYY-MM-DD`), and anything else 8 (`YYYYMMDD`).
  let date_len = if bytes.get(4) == Some(&b'-') { 10 } else { 8 };
  let date = calendar_date(bytes.get(..date_len)?)?;
  let (date, time, offset) = match bytes.len() - date_len {
    0 => (date, Time::MIDNIGHT, UtcOffset::UTC),
    // One character after the date is a separator with no time after it.
    1 => return None,
    _ => {
      let ((hour, minute, second, microsecond), offset) = time_and_offset(&bytes[date_len + 1..])?;
      let (date, hour) = match hour {
        24 if (minute, second, microsecond) == (0, 0, 0) => (date.next_day()?, 0),
        _ => (date, hour),
      };
      (date, Time::from_hms_micro(hour, minute, second, microsecond).ok()?, offset.unwrap_or(UtcOffset::UTC))
    }
  };
  Some(PrimitiveDateTime::new(date, time).assume_offset(offset))
}

/// `YYYY-MM-DD` or `YYYYMMDD`: a year from 1 and a day that the calendar has.
fn calendar_date(text: &[u8]) -> Option<Date> {
  let (year, month, day) = if text.len() == 10 {
    if text[7] != b'-' {
      return None;
    }
    (digits(&text[..4])?, digits(&text[5..7])?, digits(&text[8..])?)
  } else {
    (digits(&text[..4])?, digits(&text[4..6])?, digits(&text[6..8])?)
  };
  if year == 0 {
    return None;
  }
  let month = Month::try_from(u8::try_from(month).ok()?).ok()?;
  Date::from_calendar_date(i32::try_from(year).ok()?, month, u8::try_from(day).ok()?).ok()
}

/// The clock fields of a time, and the offset it names: the offset starts at the first `-`, else the first `+`, else
/// at the `Z` that must end the text (`_parse_isoformat_time`).
fn time_and_offset(text: &[u8]) -> Option<(ClockFields, Option<UtcOffset>)> {
  let marker = b"-+Z".iter().find_map(|sign| text.iter().position(|byte| byte == sign));
  match marker {
    None => Some((clock(text)?, None)),
    Some(index) if text[index] == b'Z' => {
      if index + 1 != text.len() {
        return None;
      }
      Some((clock(&text[..index])?, Some(UtcOffset::UTC)))
    }
    Some(index) => Some((clock(&text[..index])?, Some(offset(text[index], &text[index + 1..])?))),
  }
}

/// `±HH[[:]MM[[:]SS]]`: a whole offset under a day. The fields add up, so minutes of 60 and seconds of 60 read as Python
/// reads them.
fn offset(sign: u8, text: &[u8]) -> Option<UtcOffset> {
  let (hour, minute, second, microsecond) = clock(text)?;
  if microsecond != 0 {
    return None;
  }
  let seconds = i32::from(hour) * 3600 + i32::from(minute) * 60 + i32::from(second);
  if seconds >= 86_400 {
    return None;
  }
  UtcOffset::from_whole_seconds(if sign == b'-' { -seconds } else { seconds }).ok()
}

/// `HH[:MM[:SS]]` or `HHMM[SS]` with one separator kind throughout, then an optional `.` or `,` fraction after the
/// seconds (`_parse_hh_mm_ss_ff`).
fn clock(text: &[u8]) -> Option<ClockFields> {
  let mut fields = [0u8; 3];
  let mut pos = 0;
  let mut colon = false;
  for (index, field) in fields.iter_mut().enumerate() {
    *field = two_digits(text.get(pos..)?)?;
    pos += 2;
    if pos == text.len() || index == 2 {
      break;
    }
    if index == 0 {
      colon = text[pos] == b':';
    }
    if colon {
      if text[pos] != b':' {
        return None;
      }
      pos += 1;
    }
  }
  let microsecond = match text.get(pos..)? {
    [] => 0,
    [separator, fraction @ ..] if matches!(*separator, b'.' | b',') => fraction_micro(fraction)?,
    _ => return None,
  };
  Some((fields[0], fields[1], fields[2], microsecond))
}

/// Hour, minute, second and microsecond, as `clock` reads them.
type ClockFields = (u8, u8, u8, u32);

/// The first six digits of a fraction, as microseconds. Every character of the fraction must be a digit, and at least one
/// must exist.
fn fraction_micro(fraction: &[u8]) -> Option<u32> {
  if !fraction.iter().all(u8::is_ascii_digit) {
    return None;
  }
  let kept = &fraction[..fraction.len().min(6)];
  let value = digits(kept)?;
  Some(value * 10u32.pow((6 - kept.len()) as u32))
}

/// Exactly two ASCII digits at the start of `text`.
fn two_digits(text: &[u8]) -> Option<u8> {
  match text {
    [tens @ b'0'..=b'9', ones @ b'0'..=b'9', ..] => Some((tens - b'0') * 10 + (ones - b'0')),
    _ => None,
  }
}

/// A non-empty run of ASCII digits, as its value.
fn digits(text: &[u8]) -> Option<u32> {
  if text.is_empty() || !text.iter().all(u8::is_ascii_digit) {
    return None;
  }
  Some(text.iter().fold(0, |value, &digit| value * 10 + u32::from(digit - b'0')))
}

#[cfg(test)]
mod tests {
  use time::format_description::well_known::Rfc3339;

  use super::*;

  /// The instant that an RFC 3339 text names, in UTC.
  fn at(text: &str) -> OffsetDateTime {
    OffsetDateTime::parse(text, &Rfc3339).expect("the expected instant is RFC 3339").to_offset(UtcOffset::UTC)
  }

  #[test]
  fn naive_and_offset_values_are_read_as_recorded() {
    assert_eq!(parse("2026-08-05T12:00:00+00:00"), Some(at("2026-08-05T12:00:00Z")));
    assert_eq!(parse("2026-08-05T12:00:00"), Some(at("2026-08-05T12:00:00Z")));
    assert_eq!(parse("2026-08-05T12:00:00+09:00"), Some(at("2026-08-05T03:00:00Z")));
    assert_eq!(parse("2026-08-05T12:00:00.5+05:30"), Some(at("2026-08-05T06:30:00.5Z")));
  }

  #[test]
  fn date_only_and_basic_forms_are_read_as_recorded() {
    assert_eq!(parse("2026-08-05"), Some(at("2026-08-05T00:00:00Z")));
    assert_eq!(parse("20260805"), Some(at("2026-08-05T00:00:00Z")));
    assert_eq!(parse("20260805T120000.5Z"), Some(at("2026-08-05T12:00:00.5Z")));
    assert_eq!(parse("2026-08-05T12:00"), Some(at("2026-08-05T12:00:00Z")));
    assert_eq!(parse("2026-08-05T12:00:00+0530"), Some(at("2026-08-05T06:30:00Z")));
  }

  #[test]
  fn hour_24_is_the_next_day_and_the_calendar_is_checked() {
    assert_eq!(parse("2026-08-05T24:00:00Z"), Some(at("2026-08-06T00:00:00Z")));
    assert_eq!(parse("2026-12-31T24:00"), Some(at("2027-01-01T00:00:00Z")));
    assert_eq!(parse("2026-08-05T24:00:01"), None);
    assert_eq!(parse("2026-02-29T12:00"), None);
    assert_eq!(parse("2024-02-29T12:00"), Some(at("2024-02-29T12:00:00Z")));
    assert_eq!(parse("0000-01-01"), None);
  }

  #[test]
  fn malformed_text_is_refused_as_recorded() {
    for text in [
      "",
      "abc",
      "2026",
      "2026-08",
      "2026-8-5",
      "2026-08-05T",
      "2026-08-05 ",
      "2026-08-05T12:0",
      "2026-08-05T1",
      "2026-08-05T12:00:00.",
      "2026-08-05T12:00:00.,5",
      "2026-08-05T12:00:00+",
      "2026-08-05T12:00:00+0",
      "2026-08-05T12:00:00+24:00",
      "2026-08-05T12:00:00Z+00:00",
      "2026-08-05T12:00:00+00:00Z",
      "+123-08-05",
      "2026-0_8-05",
      "2026-08-05T23:59:60",
      "2026-08-05T12:60",
      "2026-08-05T12:00:00.5.5",
      "20260805.0",
      "\u{3000}2026-08-05",
    ] {
      assert_eq!(parse(text), None, "{text:?} must not parse");
    }
  }

  #[test]
  fn fractions_keep_six_digits_and_offsets_keep_their_seconds() {
    assert_eq!(parse("2026-08-05T12:00:00.1234567+00:00"), Some(at("2026-08-05T12:00:00.123456Z")));
    assert_eq!(parse("2026-08-05T12:00:00,5Z"), Some(at("2026-08-05T12:00:00.5Z")));
    assert_eq!(parse("2026-08-05T12:00:00-23:59:59"), Some(at("2026-08-06T11:59:59Z")));
  }

  #[test]
  fn week_dates_read_as_none_so_the_session_keeps_its_files() {
    assert_eq!(parse("2026-W32-3"), None);
    assert_eq!(parse("2026W323"), None);
  }
}
