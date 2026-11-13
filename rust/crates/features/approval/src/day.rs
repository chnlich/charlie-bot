//! The approval day view: one Pacific day's ranked T0 queue, the actions only you can do, and the digest.
//!
//! The queue holds every open Waiting on you item across sessions, so every open item appears once. The T0 items
//! come first, ranked by the rank of their rule's consequence test in the approval policy table. An
//! `out-of-scope:<id>` rule ranks with the chao-name test. Then come the review
//! items, then the items tiered t1 or t2 and the items without a tier. Equal ranks keep arrival order.
//!
//! Actions only you can do hold every open user-act item of the day, an item judged on an earlier day included,
//! because the status panels count those items through this list. A past day's list holds the items whose start time
//! falls in that day. A session scope limits the list to the session and its open descendants, the holders the
//! status panel counts.
//!
//! The digest holds the day's digest entries and the day's `policy_table_changed` records, each with its undo: an
//! entry's `undo` field, a change record's diff.

use std::collections::HashSet;

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

/// The key of the day body's actions only you can do: the day's open user-act items.
pub const ACTIONS_ONLY_YOU_CAN_DO: &str = "actions_only_you_can_do";

/// The key of the day body's digest: the day's digest entries and change records.
pub const DIGEST: &str = "digest";

/// A day view: the Pacific day it renders, the Pacific day it read as today, the optional session scope, and the JSON
/// the API answers. The page uses `today` to refresh only today's view when its tab becomes visible again.
pub struct DayView {
  pub date: Date,
  pub today: Date,
  pub session: Option<String>,
  pub body: Value,
}

/// The day view as the API answers it. A `None` date is today's Pacific day. A `None` session lists the user-act items
/// host-wide; a `Some` session limits them to that session and its open descendants. The table read
/// is a server-side read, so a changed table records its change before the view lists it.
pub async fn approval_day(
  features: &FeatureContext,
  date: Option<Date>,
  session: Option<&str>,
) -> Result<Value, DayError> {
  Ok(approval_day_view(features, date, session).await?.body)
}

/// The day view the page renders. A `None` date is today's Pacific day.
pub(crate) async fn approval_day_view(
  features: &FeatureContext,
  date: Option<Date>,
  session: Option<&str>,
) -> Result<DayView, DayError> {
  approval_day_view_at(features, date, session, Timestamp::now()).await
}

/// The day view read at one clock: `now` dates the ages and decides today's Pacific day, so the page's rows and its
/// refresh condition both read one clock.
pub(crate) async fn approval_day_view_at(
  features: &FeatureContext,
  date: Option<Date>,
  session: Option<&str>,
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
  let holders = session.map(|session_id| session_holders(features, session_id)).transpose()?;
  let actions_only_you_can_do = actions_only_you_can_do_rows(&rows, holders.as_ref(), date, today, now)?;
  let digest = digest_rows(&home, date)?;
  let body = json!({
    "date": date_text(date),
    (QUEUE): queue,
    (ACTIONS_ONLY_YOU_CAN_DO): actions_only_you_can_do,
    (DIGEST): digest,
  });
  Ok(DayView { date, today, session: session.map(ToString::to_string), body })
}

/// One open item's row, with the session that holds it and how long ago it started.
fn row_json(row: &OpenWaitingRow, now: Timestamp) -> Value {
  let age_seconds = now.as_second() - row.since.into_inner().unix_timestamp();
  let needs_recheck = !row.is_derived && (row.tier.is_none() || age_seconds > 14 * DAY);
  let tier_label = match row.tier.as_deref() {
    Some("t0" | "t1" | "t2") => "decision",
    Some(approval_policy::TIER_USER_ACT) => "user-act",
    Some(approval_policy::TIER_REVIEW) => "review",
    _ => "untiered",
  };
  json!({
    "session_id": row.session_id,
    (SESSION_NAME_FIELD): row.session_name,
    "text": row.text,
    "context": row.context,
    "link": row.link,
    "tier": row.tier,
    "tier_label": tier_label,
    "needs_recheck": needs_recheck,
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

/// The holders a session scope lists: the session and its open descendants. A session the store does not hold is a
/// refusal that names it.
fn session_holders(features: &FeatureContext, session_id: &str) -> Result<HashSet<String>, DayError> {
  let descendants = features.store.open_descendants(session_id).map_err(|error| match error {
    charliebot_store::StoreError::NotFound(msg) => DayError::Refused(StatusCode::NOT_FOUND, msg),
    other => DayError::Failed(other.into()),
  })?;
  let mut holders = HashSet::from([session_id.to_string()]);
  holders.extend(descendants.into_iter().map(|meta| meta.id));
  Ok(holders)
}

/// The actions only you can do of one day: today's list holds every open user-act item, whatever day judged it, and a
/// past day's list holds the items whose start time falls in that day. Only the holders' items count when `holders`
/// is `Some`.
fn actions_only_you_can_do_rows(
  rows: &[OpenWaitingRow],
  holders: Option<&HashSet<String>>,
  date: Date,
  today: Date,
  now: Timestamp,
) -> Result<Vec<Value>, DayError> {
  let mut items = Vec::new();
  for row in rows {
    if row.tier.as_deref() != Some(approval_policy::TIER_USER_ACT) {
      continue;
    }
    if holders.is_some_and(|holders| !holders.contains(&row.session_id)) {
      continue;
    }
    if date != today {
      let day = day_of_stamp(&row.since.to_str()).map_err(|error| DayError::Failed(error.into()))?;
      if day != date {
        continue;
      }
    }
    items.push(row_json(row, now));
  }
  Ok(items)
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
