//! The approval day view (plan 4 v2 task W3): one Pacific day's ranked T0 queue, login sheet and digest.
//!
//! The queue holds every open Waiting on you item across sessions, so every open item appears once: the T0 items
//! first, ranked by the rank of their rule's consequence test in the approval policy table (an `out-of-scope:<id>`
//! rule ranks with the chao-name test, as plan section 3 orders them), then the review items, then the items tiered
//! t1 or t2 and the items without a tier. Equal ranks keep arrival order. The login sheet holds the day's user-act
//! items. The digest holds the day's digest entries and the day's `policy_table_changed` records, each with its
//! undo: an entry's `undo` field, a change record's diff.

use axum::http::StatusCode;
use charliebot_core::approval_digest;
use charliebot_core::approval_policy::{self};
use charliebot_core::pacific::{self, date_text, day_of_stamp};
use charliebot_registry::FeatureContext;
use charliebot_web_reply::status::{self, OpenWaitingItem};
use jiff::civil::Date;
use serde_json::{Value, json};

/// A day view that refused: a bad `date` query answers 422, the rest is a failure (`day.DayError`).
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

/// The canonical `YYYY-MM-DD` date the `date` query holds, `None` when the query is absent, and a refusal that names
/// the expected form otherwise (`day._parse_date_param`).
pub fn parse_date_param(text: &str) -> Result<Date, DayError> {
  text.parse::<Date>().map_err(|_| {
    DayError::Refused(StatusCode::UNPROCESSABLE_ENTITY, format!("date must be a YYYY-MM-DD PT date, got {text:?}"))
  })
}

/// The day view as the API answers it and the page renders it (`day.approval_day`). A `None` date is today's
/// Pacific day. The table read is a server-side read, so a changed table records its change before the view lists it.
pub async fn approval_day(features: &FeatureContext, date: Option<Date>) -> Result<Value, DayError> {
  let date = match date {
    Some(date) => date,
    None => pacific::day_of(jiff::Timestamp::now()).map_err(|error| DayError::Failed(error.into()))?,
  };
  let cfg = features.config.get().map_err(|error| DayError::Failed(error.into()))?;
  let home = cfg.charliebot_home.clone();
  let loaded = approval_policy::load_and_record(&home).map_err(|error| DayError::Failed(error.into()))?;
  let rows = status::open_waiting_items(features).await.map_err(status_refused)?;
  let queue = queue_rows(&rows, &loaded.table);
  let login_sheet = login_sheet_rows(&rows, date)?;
  let digest = digest_rows(&home, date)?;
  Ok(json!({
    "date": date_text(date),
    "queue": queue,
    "login_sheet": login_sheet,
    "digest": digest,
  }))
}

/// The `WebReplyError` shapes the all-sessions read can raise; none of its session checks run here, so a refusal is
/// a failure (`day._status_refused`).
fn status_refused(error: charliebot_web_reply::status::WebReplyError) -> DayError {
  match error {
    charliebot_web_reply::status::WebReplyError::Refused { status, detail } => DayError::Refused(status, detail),
    charliebot_web_reply::status::WebReplyError::Failed(error) => DayError::Failed(error),
  }
}

/// One open item's row, with the session that holds it.
fn row_json(row: &OpenWaitingItem) -> Value {
  json!({
    "session_id": row.session_id,
    "session_name": row.session_name,
    "text": row.text,
    "context": row.context,
    "link": row.link,
    "tier": row.tier,
    "rule": row.rule,
    "since": row.since.to_str(),
  })
}

/// The tier's order in the queue: the T0 items first, the review items after them, everything else last
/// (`day._tier_group`).
fn tier_group(tier: Option<&str>) -> u8 {
  match tier {
    Some("t0") => 0,
    Some("review") => 1,
    _ => 2,
  }
}

/// The rank of a T0 item's rule in the table (`day._rule_rank`): the consequence test's rank, the chao-name test's
/// rank for an `out-of-scope:<id>` rule (plan section 3 orders the out-of-scope uses with the chao-name ones), and
/// after every ranked rule for a rule the table carries no rank of.
fn rule_rank(rule: Option<&str>, table: &approval_policy::ApprovalPolicyTable) -> i64 {
  let Some(rule) = rule else { return i64::MAX };
  if rule.starts_with("out-of-scope:") {
    return table.consequence_tests.iter().find(|test| test.id == "chao-name").map_or(i64::MAX, |test| test.rank);
  }
  table.consequence_tests.iter().find(|test| test.id == rule).map_or(i64::MAX, |test| test.rank)
}

/// The queue: every open item once, T0 ranked, then review, then the rest, arrival order within one rank
/// (`day._queue_rows`). Each T0 row carries its rule's rank; a rule the table carries no rank of reads as null.
fn queue_rows(rows: &[OpenWaitingItem], table: &approval_policy::ApprovalPolicyTable) -> Vec<Value> {
  let mut keyed: Vec<(u8, i64, charliebot_store::metadata::UtcDateTime, Value)> = rows
    .iter()
    .map(|row| {
      let group = tier_group(row.tier.as_deref());
      let rank = if group == 0 { rule_rank(row.rule.as_deref(), table) } else { 0 };
      (group, rank, row.since, row_json(row))
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

/// The day's login sheet: the user-act items whose start time falls in `date` (`day._login_sheet_rows`).
fn login_sheet_rows(rows: &[OpenWaitingItem], date: Date) -> Result<Vec<Value>, DayError> {
  let mut sheet = Vec::new();
  for row in rows {
    if row.tier.as_deref() != Some("user-act") {
      continue;
    }
    let day = day_of_stamp(&row.since.to_str()).map_err(|error| DayError::Failed(error.into()))?;
    if day == date {
      sheet.push(row_json(row));
    }
  }
  Ok(sheet)
}

/// The day's digest: the day's digest entries and the day's `policy_table_changed` records, one list in arrival
/// order (`day._digest_rows`). Both stores stamp their records with the same UTC form, so the stamps order as text.
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

/// The Pacific day a stored stamp falls in, when the stamp is one (`day._day_text`).
fn day_text(stamp: &str) -> Option<String> {
  day_of_stamp(stamp).ok().map(date_text)
}

#[cfg(test)]
#[path = "day_tests.rs"]
mod tests;
