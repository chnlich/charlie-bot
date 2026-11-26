//! The five-field cron scan and the one schedule check that the task loader and the write routes share.
//!
//! The module is a leaf: it reads a cron expression in an IANA timezone and answers its next fire. Both
//! `config::validate_task` and the scheduler use it, so every entry point runs one implementation without an import
//! cycle between those two modules.

use std::collections::HashSet;

use charliebot_store::metadata::UtcDateTime;
use chrono::{Datelike, Offset};

/// The next cron occurrence in the configured time zone.
pub(crate) fn next_fire_iso(expression: &str, timezone: &str, now_utc: UtcDateTime) -> anyhow::Result<String> {
  let timezone = timezone.parse::<chrono_tz::Tz>()?;
  let inner = now_utc.into_inner();
  let now = chrono::DateTime::<chrono::Utc>::from_timestamp(inner.unix_timestamp(), inner.nanosecond())
    .ok_or_else(|| anyhow::anyhow!("UTC timestamp is outside the supported range"))?;
  isoformat_text(&CronExpression::parse(expression)?.next_after(now.with_timezone(&timezone))?)
}

/// The one schedule check: a five-field cron expression that has a fire inside the eight-year horizon, in an IANA
/// timezone. Every task file load and every write route runs it through `config::validate_task`. Returns the next
/// fire after now.
pub(crate) fn validate_schedule(cron: &str, timezone: &str) -> Result<String, String> {
  next_fire_iso(cron, timezone, UtcDateTime::now()).map_err(|error| format!("{error:#}"))
}

pub(crate) fn isoformat_text<Tz>(moment: &chrono::DateTime<Tz>) -> anyhow::Result<String>
where
  Tz: chrono::TimeZone,
  Tz::Offset: Offset,
{
  let instant = time::OffsetDateTime::from_unix_timestamp(moment.timestamp())?
    .replace_microsecond(moment.timestamp_subsec_micros())?;
  let offset = time::UtcOffset::from_whole_seconds(moment.offset().fix().local_minus_utc())?;
  Ok(charliebot_core::timefmt::isoformat(instant.to_offset(offset)))
}

#[derive(Clone)]
struct CronField {
  values: HashSet<u32>,
  ordered: Vec<u32>,
  wildcard: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct CronSearchWork {
  calendar_days: usize,
  local_candidates: usize,
}

/// The next-fire search horizon in calendar days: eight years of 366 days.
const NEXT_FIRE_HORIZON_DAYS: i64 = 8 * 366;

pub(crate) struct CronExpression {
  minute: CronField,
  hour: CronField,
  day: CronField,
  month: CronField,
  weekday: CronField,
}

impl CronExpression {
  pub(crate) fn parse(expression: &str) -> anyhow::Result<Self> {
    let fields = expression.split_whitespace().collect::<Vec<_>>();
    if fields.len() != 5 {
      anyhow::bail!("cron expression must have five fields: {expression:?}");
    }
    Ok(Self {
      minute: parse_field(fields[0], 0, 59, &[])?,
      hour: parse_field(fields[1], 0, 23, &[])?,
      day: parse_field(fields[2], 1, 31, &[])?,
      month: parse_field(fields[3], 1, 12, &MONTH_NAMES)?,
      weekday: parse_field(fields[4], 0, 7, &WEEKDAY_NAMES)?,
    })
  }

  pub(crate) fn next_after<Tz: chrono::TimeZone + Clone>(
    &self,
    base: chrono::DateTime<Tz>,
  ) -> anyhow::Result<chrono::DateTime<Tz>> {
    // The next fire is the earliest matching local time on the UTC minute grid. The scan stops at the eight-year
    // horizon (`NEXT_FIRE_HORIZON_DAYS`); an expression with no fire inside it is an error.
    self.next_after_with_work(base).0
  }

  /// The next fire and the work its search did, reported on the error path too.
  fn next_after_with_work<Tz: chrono::TimeZone + Clone>(
    &self,
    base: chrono::DateTime<Tz>,
  ) -> (anyhow::Result<chrono::DateTime<Tz>>, CronSearchWork) {
    let base_utc = base.with_timezone(&chrono::Utc);
    let timezone = base.timezone().clone();
    let start = base_utc.timestamp().div_euclid(60) * 60 + 60;
    let end = start + NEXT_FIRE_HORIZON_DAYS * 24 * 60 * 60;
    let first_date = chrono::DateTime::from_timestamp(start, 0)
      .expect("a bounded Unix minute is representable")
      .with_timezone(&timezone)
      .date_naive();
    let start_local = chrono::DateTime::from_timestamp(start, 0)
      .expect("a bounded Unix minute is representable")
      .with_timezone(&timezone);
    let mut work = CronSearchWork::default();
    let mut best: Option<chrono::DateTime<Tz>> = None;

    for day_offset in 0..=(NEXT_FIRE_HORIZON_DAYS + 3) {
      let date = first_date + chrono::Duration::days(day_offset);
      work.calendar_days += 1;
      // IANA offsets stay within 24 hours, giving a conservative UTC floor for local candidates on this date.
      let date_floor =
        date.and_hms_opt(0, 0, 0).expect("midnight is a valid naive datetime").and_utc().timestamp() - 24 * 60 * 60;
      if date_floor >= end {
        break;
      }
      if let Some(next) = best.as_ref()
        && date_floor > next.with_timezone(&chrono::Utc).timestamp()
      {
        return (Ok(next.clone()), work);
      }
      if !self.date_matches(date) {
        continue;
      }
      if date == start_local.date_naive() && self.minute.values.len() == 60 && self.hour.values.len() == 24 {
        let first = chrono::DateTime::from_timestamp(start, 0)
          .expect("a bounded Unix minute is representable")
          .with_timezone(&timezone);
        return (Ok(first), work);
      }
      for hour in &self.hour.ordered {
        for minute in &self.minute.ordered {
          work.local_candidates += 1;
          let Some(local_time) = date.and_hms_opt(*hour, *minute, 0) else { continue };
          let mut consider = |candidate: chrono::DateTime<Tz>| {
            let timestamp = candidate.with_timezone(&chrono::Utc).timestamp();
            if timestamp >= start
              && timestamp < end
              && timestamp.rem_euclid(60) == 0
              && best.as_ref().is_none_or(|current| timestamp < current.with_timezone(&chrono::Utc).timestamp())
            {
              best = Some(candidate);
            }
          };
          match timezone.from_local_datetime(&local_time) {
            chrono::LocalResult::Single(candidate) => consider(candidate),
            chrono::LocalResult::Ambiguous(first, second) => {
              consider(first);
              consider(second);
            }
            chrono::LocalResult::None => {}
          }
        }
      }
    }
    let next = best.ok_or_else(|| anyhow::anyhow!("cron expression has no fire within eight years"));
    (next, work)
  }

  fn date_matches(&self, date: chrono::NaiveDate) -> bool {
    let day_match = self.day.values.contains(&date.day());
    let weekday = date.weekday().num_days_from_sunday();
    let weekday_match = self.weekday.values.contains(&weekday) || (weekday == 0 && self.weekday.values.contains(&7));
    let date_matches = match (self.day.wildcard, self.weekday.wildcard) {
      (true, true) => true,
      (true, false) => weekday_match,
      (false, true) => day_match,
      (false, false) => day_match || weekday_match,
    };
    self.month.values.contains(&date.month()) && date_matches
  }
}

const MONTH_NAMES: [(&str, u32); 12] = [
  ("jan", 1),
  ("feb", 2),
  ("mar", 3),
  ("apr", 4),
  ("may", 5),
  ("jun", 6),
  ("jul", 7),
  ("aug", 8),
  ("sep", 9),
  ("oct", 10),
  ("nov", 11),
  ("dec", 12),
];
const WEEKDAY_NAMES: [(&str, u32); 7] =
  [("sun", 0), ("mon", 1), ("tue", 2), ("wed", 3), ("thu", 4), ("fri", 5), ("sat", 6)];

fn parse_field(field: &str, min: u32, max: u32, names: &[(&str, u32)]) -> anyhow::Result<CronField> {
  let wildcard = field == "*" || field == "?";
  let mut values = HashSet::new();
  for member in field.split(',') {
    let (range, step) = match member.split_once('/') {
      Some((range, step)) => {
        let step = step.parse::<u32>().map_err(|error| anyhow::anyhow!("invalid cron step in {field:?}: {error}"))?;
        (range, step)
      }
      None => (member, 1),
    };
    if step == 0 {
      anyhow::bail!("invalid cron step in {field:?}");
    }
    let (start, end) = if range == "*" || range == "?" {
      (min, max)
    } else if let Some((start, end)) = range.split_once('-') {
      (parse_cron_value(start, names)?, parse_cron_value(end, names)?)
    } else {
      let value = parse_cron_value(range, names)?;
      (value, if member.contains('/') { max } else { value })
    };
    if start < min || end > max || start > end {
      anyhow::bail!("cron field {field:?} is outside {min}..={max}");
    }
    values.extend((start..=end).step_by(step as usize));
  }
  if values.is_empty() {
    anyhow::bail!("cron field {field:?} has no values");
  }
  let mut ordered = values.iter().copied().collect::<Vec<_>>();
  ordered.sort_unstable();
  Ok(CronField { values, ordered, wildcard })
}

fn parse_cron_value(value: &str, names: &[(&str, u32)]) -> anyhow::Result<u32> {
  if let Some((_, number)) = names.iter().find(|(name, _)| value.eq_ignore_ascii_case(name)) {
    return Ok(*number);
  }
  value.parse().map_err(|_| anyhow::anyhow!("invalid cron field value {value:?}"))
}

#[cfg(test)]
mod tests {
  use chrono::{TimeZone, Timelike, Utc};

  use super::{CronExpression, isoformat_text};

  #[test]
  fn cron_expression_uses_minute_grid_and_croniter_day_union() {
    let expression = CronExpression::parse("0 8 1 * mon").unwrap();
    let base = Utc.with_ymd_and_hms(2026, 6, 2, 8, 0, 0).unwrap();
    assert_eq!(expression.next_after(base).unwrap().to_rfc3339(), "2026-06-08T08:00:00+00:00");
  }

  #[test]
  fn sparse_cron_checks_calendar_dates_within_the_eight_year_horizon() {
    let expression = CronExpression::parse("0 0 1 * *").unwrap();
    let base = Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap();

    let (next, work) = expression.next_after_with_work(base);
    let next = next.unwrap();

    assert_eq!(next.to_rfc3339(), "2026-02-01T00:00:00+00:00");
    assert!(work.calendar_days <= 33, "{work:?}");
    assert_eq!(work.local_candidates, 1);

    // Jan 1 is 364 days out, inside the eight-year horizon.
    let yearly = CronExpression::parse("0 0 1 1 *").unwrap();
    assert_eq!(yearly.next_after(base).unwrap().to_rfc3339(), "2027-01-01T00:00:00+00:00");

    // Feb 30 never occurs: the search stops at the horizon, within its calendar-day cap.
    let never = CronExpression::parse("0 0 30 2 *").unwrap();
    let (result, work) = never.next_after_with_work(base);
    assert!(result.unwrap_err().to_string().contains("no fire within eight years"));
    assert!(work.calendar_days <= 8 * 366 + 4, "{work:?}");
  }

  #[test]
  fn next_fire_more_than_49_days_out_resolves_in_the_task_timezone() {
    let timezone = "America/Los_Angeles".parse::<chrono_tz::Tz>().unwrap();

    let expression = CronExpression::parse("0 3 1 1 *").unwrap();
    let base = Utc.with_ymd_and_hms(2026, 11, 8, 3, 6, 0).unwrap().with_timezone(&timezone);
    assert_eq!(expression.next_after(base).unwrap().to_rfc3339(), "2027-01-01T03:00:00-08:00");

    let expression = CronExpression::parse("0 17 15 12 *").unwrap();
    let base = Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap().with_timezone(&timezone);
    assert_eq!(expression.next_after(base).unwrap().to_rfc3339(), "2026-12-15T17:00:00-08:00");
  }

  #[test]
  fn cron_stamps_use_microseconds_and_keep_the_task_timezone_offset() {
    let timezone = "America/Los_Angeles".parse::<chrono_tz::Tz>().unwrap();
    let whole_seconds = timezone.with_ymd_and_hms(2026, 10, 20, 2, 26, 6).single().unwrap();
    let fractional = whole_seconds.with_nanosecond(685_829_123).unwrap();

    assert_eq!(isoformat_text(&whole_seconds).unwrap(), "2026-10-20T02:26:06-07:00");
    assert_eq!(isoformat_text(&fractional).unwrap(), "2026-10-20T02:26:06.685829-07:00");
  }
}
