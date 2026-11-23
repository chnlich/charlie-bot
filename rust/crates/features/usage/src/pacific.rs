//! The usage page's calendar. Every date and stamp the page prints is in America/Los_Angeles, whatever zone the server
//! runs in. The last week is the Pacific calendar week before the one `now` falls in: [Monday 00:00, next Monday
//! 00:00) in that zone. That week is 169 hours when the clocks change inside it. Core's `pacific` owns the zone and
//! the Pacific day of a stamp.
use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{Span, Timestamp};
use time::OffsetDateTime;

use charliebot_core::pacific::zone;

pub use charliebot_core::pacific::day_text_of_stamp as day_of_ts;

/// A Pacific calendar week: `[start, end)`, both ends at 00:00 on `monday` in America/Los_Angeles. The one exception is
/// `to_date`, whose end is the ledger's last capture instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Week {
  pub monday: Date,
  pub start: Timestamp,
  pub end: Timestamp,
}

impl Week {
  /// The Pacific calendar week `now` falls in, whole: [Monday 00:00, next Monday 00:00).
  pub fn containing(now: Timestamp) -> anyhow::Result<Week> {
    let today = now.to_zoned(zone()?).date();
    let this_monday = today.checked_sub(Span::new().days(i64::from(today.weekday().to_monday_zero_offset())))?;
    Week::starting(this_monday)
  }

  /// The last full week before the one `now` falls in.
  pub fn last_before(now: Timestamp) -> anyhow::Result<Week> {
    Week::containing(now)?.previous()
  }

  /// This week up to the ledger's last capture, `as_of`: [Monday 00:00, as_of). A capture that predates this Monday
  /// leaves the window empty. With no capture for this host, the window is empty too.
  pub fn to_date(self, as_of: Option<Timestamp>) -> Week {
    let end = as_of.map_or(self.start, |as_of| as_of.max(self.start));
    Week { end, ..self }
  }

  /// The week before this one.
  pub fn previous(self) -> anyhow::Result<Week> {
    Week::starting(self.monday.checked_sub(Span::new().days(7))?)
  }

  fn starting(monday: Date) -> anyhow::Result<Week> {
    let zone = zone()?;
    let start = monday.at(0, 0, 0, 0).to_zoned(zone.clone())?.timestamp();
    let end = monday.checked_add(Span::new().days(7))?.at(0, 0, 0, 0).to_zoned(zone)?.timestamp();
    Ok(Week { monday, start, end })
  }

  /// The week as the page prints it: its first and last Pacific days, both inclusive.
  pub fn label(self) -> anyhow::Result<String> {
    let last_day = self.monday.checked_add(Span::new().days(6))?;
    Ok(format!("{} → {}", self.monday, last_day))
  }

  /// The UTC instant `text` a stored `ts` compares against: the 19-character prefix every stored `ts` shares, to the
  /// second. Stored stamps carry a fraction and a `Z` or `+00:00`, and this bound orders them correctly.
  pub fn utc_text(moment: Timestamp) -> String {
    moment.to_zoned(TimeZone::UTC).strftime("%Y-%m-%dT%H:%M:%S").to_string()
  }

  /// The UTC calendar day `moment` falls in, as the aggregate's `day` column holds it.
  pub fn utc_day(moment: Timestamp) -> String {
    moment.to_zoned(TimeZone::UTC).strftime("%Y-%m-%d").to_string()
  }
}

/// The instant of a stored capture stamp, to the nanosecond.
pub fn instant_of(moment: OffsetDateTime) -> anyhow::Result<Timestamp> {
  Ok(Timestamp::from_nanosecond(moment.unix_timestamp_nanos())?)
}

/// `%Y-%m-%d %H:%M %Z` of an instant in Pacific time: the zone abbreviation is PDT or PST.
pub fn stamp(moment: Timestamp) -> anyhow::Result<String> {
  Ok(moment.to_zoned(zone()?).strftime("%Y-%m-%d %H:%M %Z").to_string())
}

/// The Pacific zone abbreviation at `moment`: PDT or PST.
pub fn zone_label(moment: Timestamp) -> anyhow::Result<String> {
  Ok(moment.to_zoned(zone()?).strftime("%Z").to_string())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn at(text: &str) -> Timestamp {
    text.parse().expect("the instant parses")
  }

  #[test]
  fn last_week_spans_the_fall_change_as_169_hours() {
    // At noon on a Tuesday after the fall clock change, the last week runs Monday to Monday: 169 hours long.
    let week = Week::last_before(at("2026-11-03T20:00:00Z")).expect("the week resolves");
    assert_eq!(week.start, at("2026-10-26T07:00:00Z"));
    assert_eq!(week.end, at("2026-11-02T08:00:00Z"));
    assert_eq!(week.end.duration_since(week.start).as_secs(), 169 * 3600);
  }

  #[test]
  fn a_monday_midnight_opens_the_week_it_starts() {
    // Midnight on a Monday, Pacific time, opens the week it starts, so last week is the one before it.
    assert_eq!(
      Week::last_before(at("2026-10-26T07:00:00Z")).expect("the week resolves").start,
      at("2026-10-19T07:00:00Z")
    );
    // One second earlier is still Sunday in Los Angeles, so last week ends on the current week's Monday.
    assert_eq!(
      Week::last_before(at("2026-10-26T06:59:59Z")).expect("the week resolves").start,
      at("2026-10-12T07:00:00Z")
    );
  }

  #[test]
  fn this_week_opens_on_its_monday_after_the_spring_change() {
    // The clocks went forward on 2026-03-08, so the Monday after it opens PDT. Last week, [03-02, 03-09), is 167 hours.
    let now = at("2026-03-10T20:00:00Z");
    let week = Week::containing(now).expect("the week resolves");
    assert_eq!(week.start, at("2026-03-09T07:00:00Z"));
    assert_eq!(week.end, at("2026-03-16T07:00:00Z"));
    let last = Week::last_before(now).expect("the week resolves");
    assert_eq!(last.end, week.start);
    assert_eq!(last.end.duration_since(last.start).as_secs(), 167 * 3600);
  }

  #[test]
  fn to_date_ends_at_the_capture_and_never_before_its_monday() {
    let week = Week::containing(at("2026-03-10T20:00:00Z")).expect("the week resolves");
    assert_eq!(week.to_date(Some(at("2026-03-10T19:30:00Z"))).end, at("2026-03-10T19:30:00Z"));
    // A capture at the Monday's first instant leaves the window empty, and so does one from the Sunday before it.
    assert_eq!(week.to_date(Some(at("2026-03-09T07:00:00Z"))).end, week.start);
    assert_eq!(week.to_date(Some(at("2026-03-09T06:59:59Z"))).end, week.start);
    // With no capture for this host, nothing is counted.
    assert_eq!(week.to_date(None).end, week.start);
  }

  #[test]
  fn previous_week_abuts_this_one_and_labels_its_days() {
    let last = Week::last_before(at("2026-10-30T20:00:00Z")).expect("the week resolves");
    let previous = last.previous().expect("the previous week resolves");
    assert_eq!(previous.end, last.start);
    assert_eq!(last.label().expect("the label formats"), "2026-10-19 → 2026-10-25");
  }

  #[test]
  fn stored_stamps_name_their_pacific_day() {
    // 06:30Z on 10-26 is still 10-25 in Los Angeles, which a UTC day prefix would call 10-26.
    assert_eq!(day_of_ts("2026-10-26T06:30:00.123Z").expect("the stamp reads"), "2026-10-25");
    assert_eq!(day_of_ts("2026-10-26T07:00:00+00:00").expect("the stamp reads"), "2026-10-26");
    assert!(day_of_ts("2026-10-26").is_err());
  }
}
