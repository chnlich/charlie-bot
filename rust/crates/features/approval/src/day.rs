//! The approval day view: one Pacific day's ranked T0 queue, login sheet and digest.
//!
//! The queue holds every open Waiting on you item across sessions, so every open item appears once. The T0 items
//! come first, ranked by the rank of their rule's consequence test in the approval policy table. An
//! `out-of-scope:<id>` rule ranks with the chao-name test. Then come the review
//! items, then the items tiered t1 or t2 and the items without a tier. Equal ranks keep arrival order. The login
//! sheet holds today's every open user-act item, an item judged on an earlier day included, because the panels list
//! those items only through this sheet. A past day's sheet holds the items whose start time falls in it. The digest
//! holds the day's digest entries and the day's `policy_table_changed` records, each with its undo: an entry's
//! `undo` field, a change record's diff.

use axum::http::StatusCode;
use charliebot_core::approval_digest;
use charliebot_core::approval_policy::{self};
use charliebot_core::pacific::{self, date_text, day_of_stamp};
use charliebot_registry::FeatureContext;
use charliebot_registry::registry::OpenWaitingRow;
use charliebot_store::metadata::{SESSION_NAME_FIELD, UtcDateTime};
use jiff::Timestamp;
use jiff::civil::Date;
use serde_json::{Value, json};

/// A day view that refused: a bad `date` query answers 422, and every other failure is an internal error.
#[derive(Debug)]
pub enum DayError {
  Refused(StatusCode, String),
  Failed(anyhow::Error),
}

impl From<anyhow::Error> for DayError {
  fn from(error: anyhow::Error) -> Self {
    DayError::Failed(error)
  }
}

/// The canonical `YYYY-MM-DD` date one `date` query value holds. A value that is not such a date is a refusal that
/// names the expected form.
pub fn parse_date_param(text: &str) -> Result<Date, DayError> {
  text.parse::<Date>().map_err(|_| {
    DayError::Refused(StatusCode::UNPROCESSABLE_ENTITY, format!("date must be a YYYY-MM-DD PT date, got {text:?}"))
  })
}

/// The key of the day body's ranked queue: the open items, T0 first.
pub const QUEUE: &str = "queue";

/// The key of the day body's login sheet: the day's open user-act items.
pub const LOGIN_SHEET: &str = "login_sheet";

/// The key of the day body's digest: the day's digest entries and change records.
pub const DIGEST: &str = "digest";

/// A day view: the Pacific day it renders, the Pacific day it read as today, and the JSON the API answers. The page
/// uses `today` to refresh only today's view when its tab becomes visible again.
pub struct DayView {
  pub date: Date,
  pub today: Date,
  pub body: Value,
}

/// The day view as the API answers it. A `None` date is today's Pacific day. The table read
/// is a server-side read, so a changed table records its change before the view lists it.
pub async fn approval_day(features: &FeatureContext, date: Option<Date>) -> Result<Value, DayError> {
  Ok(approval_day_view(features, date).await?.body)
}

/// The day view the page renders. A `None` date is today's Pacific day.
pub(crate) async fn approval_day_view(features: &FeatureContext, date: Option<Date>) -> Result<DayView, DayError> {
  approval_day_view_at(features, date, Timestamp::now()).await
}

/// The day view read at one clock: `now` dates the ages and decides today's Pacific day, so the page's rows and its
/// refresh condition both read one clock.
pub(crate) async fn approval_day_view_at(
  features: &FeatureContext,
  date: Option<Date>,
  now: Timestamp,
) -> Result<DayView, DayError> {
  let today = pacific::day_of(now).map_err(|error| DayError::Failed(error.into()))?;
  let date = date.unwrap_or(today);
  let cfg = features.config.get().map_err(|error| DayError::Failed(error.into()))?;
  let home = cfg.charliebot_home.clone();
  let loaded = approval_policy::load_and_record(&home).map_err(|error| DayError::Failed(error.into()))?;
  let reader = features.registry.open_waiting_items_reader().map_err(DayError::Failed)?;
  let rows = reader(features).await.map_err(DayError::Failed)?;
  let queue = queue_rows(&rows, &loaded.table, now);
  let login_sheet = login_sheet_rows(&rows, date, today, now)?;
  let digest = digest_rows(&home, date)?;
  let body = json!({
    "date": date_text(date),
    (QUEUE): queue,
    (LOGIN_SHEET): login_sheet,
    (DIGEST): digest,
  });
  Ok(DayView { date, today, body })
}

/// One open item's row, with the session that holds it and how long ago it started.
fn row_json(row: &OpenWaitingRow, now: Timestamp) -> Value {
  json!({
    "session_id": row.session_id,
    (SESSION_NAME_FIELD): row.session_name,
    "text": row.text,
    "context": row.context,
    "link": row.link,
    "tier": row.tier,
    "rule": row.rule,
    "since": row.since.to_str(),
    "age": age_text(row.since, now),
  })
}

const MINUTE: i64 = 60;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;
const MONTH: i64 = 30 * DAY;
const YEAR: i64 = 365 * DAY;

/// How long ago an item started, in a form a reader takes in at a glance. The forms are `just now`,
/// `12 min ago`, `5 h ago`, `3 d ago`, `2 mo ago`, `1 y ago`.
fn age_text(since: UtcDateTime, now: Timestamp) -> String {
  let seconds = now.as_second() - since.into_inner().unix_timestamp();
  if seconds < MINUTE {
    return "just now".to_string();
  }
  let (count, unit) = if seconds < HOUR {
    (seconds / MINUTE, "min")
  } else if seconds < DAY {
    (seconds / HOUR, "h")
  } else if seconds < MONTH {
    (seconds / DAY, "d")
  } else if seconds < YEAR {
    (seconds / MONTH, "mo")
  } else {
    (seconds / YEAR, "y")
  };
  format!("{count} {unit} ago")
}

/// The tier's order in the queue: the T0 items first, the review items after them, and everything else last.
fn tier_group(tier: Option<&str>) -> u8 {
  match tier {
    Some(approval_policy::TIER_T0) => 0,
    Some(approval_policy::TIER_REVIEW) => 1,
    _ => 2,
  }
}

/// The queue: every open item once, T0 ranked, then review, then the rest, arrival order within one rank. Each T0
/// row carries its rule's rank; a rule the table carries no rank of reads as null, and sorts after every ranked rule.
fn queue_rows(rows: &[OpenWaitingRow], table: &approval_policy::ApprovalPolicyTable, now: Timestamp) -> Vec<Value> {
  let mut keyed: Vec<(u8, i64, charliebot_store::metadata::UtcDateTime, Value)> = rows
    .iter()
    .map(|row| {
      let group = tier_group(row.tier.as_deref());
      let rank = if group == 0 {
        row.rule.as_deref().and_then(|rule| approval_policy::rule_rank(rule, table)).unwrap_or(i64::MAX)
      } else {
        0
      };
      (group, rank, row.since, row_json(row, now))
    })
    .collect();
  keyed.sort_by_key(|keyed| (keyed.0, keyed.1, keyed.2));
  keyed
    .into_iter()
    .map(|(group, rank, _, mut row)| {
      row["rank"] = if group == 0 && rank != i64::MAX { json!(rank) } else { Value::Null };
      row
    })
    .collect()
}

/// The login sheet: today's holds every open user-act item, whatever day judged it, because the panels list those
/// items only through this sheet. A past day's holds the items whose start time falls in it.
fn login_sheet_rows(rows: &[OpenWaitingRow], date: Date, today: Date, now: Timestamp) -> Result<Vec<Value>, DayError> {
  let mut sheet = Vec::new();
  for row in rows {
    if row.tier.as_deref() != Some(approval_policy::TIER_USER_ACT) {
      continue;
    }
    if date != today {
      let day = day_of_stamp(&row.since.to_str()).map_err(|error| DayError::Failed(error.into()))?;
      if day != date {
        continue;
      }
    }
    sheet.push(row_json(row, now));
  }
  Ok(sheet)
}

/// The day's digest: the day's digest entries and the day's `policy_table_changed` records, one list in arrival
/// order. Both stores stamp their records with the same UTC form, so the stamps order as text.
fn digest_rows(home: &std::path::Path, date: Date) -> Result<Vec<Value>, DayError> {
  let day = date_text(date);
  let mut rows: Vec<(String, usize, Value)> = Vec::new();
  let entries = approval_digest::read_entries(home).map_err(|error| DayError::Failed(error.into()))?;
  for (index, entry) in entries.iter().enumerate() {
    if day_text(&entry.time).as_deref() == Some(day.as_str()) {
      let mut row = approval_digest::record_of(entry);
      // The page labels an entry with the first eight characters of the session id, as the panels do.
      row["session_short"] = json!(entry.session.chars().take(8).collect::<String>());
      rows.push((entry.time.clone(), index, row));
    }
  }
  let changes = approval_policy::change_records(home).map_err(|error| DayError::Failed(error.into()))?;
  for (index, record) in changes.iter().enumerate() {
    let Some(time) = record.get("time").and_then(Value::as_str) else { continue };
    if day_text(time).as_deref() == Some(day.as_str()) {
      rows.push((time.to_string(), index, record.clone()));
    }
  }
  rows.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));
  Ok(rows.into_iter().map(|(_, _, row)| row).collect())
}

/// The Pacific day a stored stamp falls in, when the stamp is one.
fn day_text(stamp: &str) -> Option<String> {
  day_of_stamp(stamp).ok().map(date_text)
}

#[cfg(test)]
#[path = "day_tests.rs"]
mod tests;
