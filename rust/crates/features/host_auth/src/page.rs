//! The standing page (`src/features/host_auth/api.py` `host_auth_page`, `_page_rows`, `_summary`, `_format_remaining`):
//! the template context built from the state and rendered through the shared template handle.

use anyhow::Context;
use axum::response::Response;
use charliebot_core::text_repr;
use charliebot_registry::FeatureContext;
use charliebot_registry::templating::Templating;
use http::request::Parts;
use serde_json::{Map, Value, json};
use time::OffsetDateTime;

use crate::host_auth::{self, BLOCKED_BACKOFF_SEC, PROBE_INTERVAL_SEC, State, TRUST_CACHE_TTL_SEC};
use crate::probe::{STATUS_NEEDS_INTERACTIVE_AUTH, STATUS_NEEDS_OKTA, STATUS_OK, STATUS_UNREACHABLE};
use crate::timestamps::{self, LocalFormat};

const TEMPLATE: &str = "host_auth.html";
/// A countdown under a day is highlighted.
const AMBER_UNDER_SEC: f64 = 24.0 * 3600.0;

/// Render the host-auth page with request data available to the registered template globals.
pub fn render(
  features: &FeatureContext,
  state: &State,
  now: OffsetDateTime,
  request: &Parts,
) -> Result<Response, (String, String)> {
  let context = context(state, now).map_err(|error| ("Exception".to_string(), error.to_string()))?;
  let templating =
    features.startup.require::<Templating>().map_err(|error| ("RuntimeError".to_string(), error.to_string()))?;
  templating
    .render_page(features, TEMPLATE, &context, request)
    .map_err(|error| (error.kind().to_string(), error.to_string()))
}

/// The template context of `host_auth_page`.
fn context(state: &State, now: OffsetDateTime) -> anyhow::Result<Value> {
  let rows = host_auth::host_entries(state)?
    .into_iter()
    .map(|entry| page_row(entry, now))
    .collect::<anyhow::Result<Vec<_>>>()?;
  let summary = summary(state, now)?;
  let last_probe = timestamps::local_timestamp(host_auth::opt_str(state, "probed_at")?, LocalFormat::DateZone)?;
  Ok(json!({
    "probe_running": state.get("probe_running").is_some_and(text_repr::is_truthy),
    "hosts": rows,
    "summary": summary,
    "last_probe": last_probe,
    "interval_label": format!("{} min", PROBE_INTERVAL_SEC / 60),
    "backoff_label": format!("{} h", BLOCKED_BACKOFF_SEC / 3600),
    "ttl_label": format!("{} days", TRUST_CACHE_TTL_SEC / 86400),
  }))
}

/// `_page_rows`: one row for one state entry, with the deadline column's mode resolved. The column counts only for a
/// host that answers and carries a baseline; a passed deadline stops counting and keeps its baseline.
fn page_row(entry: &Map<String, Value>, now: OffsetDateTime) -> anyhow::Result<Value> {
  let alias = entry.get("alias").and_then(Value::as_str).context("a host entry has no string alias")?;
  let status = entry.get("status").and_then(Value::as_str).with_context(|| format!("host {alias} has no status"))?;
  let (status_label, status_class) = status_presentation(status)?;
  let detail = host_auth::opt_str(entry, "detail")?.unwrap_or("");
  let baseline = host_auth::opt_str(entry, "enrolled_observed_at")?;
  let hostname = entry.get("hostname").with_context(|| format!("host {alias} has no hostname"))?;
  let baseline_text = timestamps::local_timestamp(baseline, LocalFormat::DateZone)?;
  let last_probe = timestamps::local_timestamp(host_auth::opt_str(entry, "last_probe_at")?, LocalFormat::Clock)?;

  let mut row = json!({
    "alias": alias,
    "hostname": hostname,
    "status": status,
    "status_label": status_label,
    "status_class": status_class,
    "detail_lines": detail.split('\n').collect::<Vec<_>>(),
    "renew_cmd": format!("ssh {alias} true"),
    "has_url": detail.contains("http"),
    "baseline": baseline_text,
    "last_probe": last_probe,
    "deadline_kind": "not_applicable",
    "remaining": null,
    "expires": null,
    "under_24h": false,
  });
  if status == STATUS_OK {
    let estimate = host_auth::derive_estimate(entry, now)?;
    row["expires"] = json!(timestamps::local_timestamp(estimate.expires_at.as_deref(), LocalFormat::DateZone)?);
    if baseline.is_none() {
      row["deadline_kind"] = json!("no_baseline");
    } else {
      let remaining = estimate.remaining_sec.context("an estimate with a baseline has a remaining time")?;
      if remaining <= 0.0 {
        row["deadline_kind"] = json!("expired");
      } else {
        row["deadline_kind"] = json!("counting");
        row["remaining"] = json!(format_remaining(remaining));
        row["under_24h"] = json!(remaining < AMBER_UNDER_SEC);
      }
    }
  } else if status == STATUS_NEEDS_OKTA {
    row["deadline_kind"] = json!("pending_okta");
  }
  Ok(row)
}

/// The label and the CSS class of each status.
fn status_presentation(status: &str) -> anyhow::Result<(&'static str, &'static str)> {
  match status {
    STATUS_OK => Ok(("Direct", "ok")),
    STATUS_NEEDS_OKTA => Ok(("Needs Okta", "okta")),
    STATUS_NEEDS_INTERACTIVE_AUTH => Ok(("Needs interactive auth", "iact")),
    STATUS_UNREACHABLE => Ok(("Unreachable", "down")),
    other => anyhow::bail!("no status label for {other:?}"),
  }
}

/// `_summary`: the hosts held right now, and the `ok` host with the soonest positive estimate.
fn summary(state: &State, now: OffsetDateTime) -> anyhow::Result<Value> {
  let entries = host_auth::host_entries(state)?;
  let mut blocked = Vec::new();
  for entry in &entries {
    if entry.get("status").and_then(Value::as_str) == Some(STATUS_NEEDS_OKTA) {
      blocked.push(entry.get("alias").cloned().unwrap_or(Value::Null));
    }
  }
  let mut soonest: Option<(f64, &str)> = None;
  for entry in &entries {
    if entry.get("status").and_then(Value::as_str) != Some(STATUS_OK) {
      continue;
    }
    let Some(remaining) = host_auth::derive_estimate(entry, now)?.remaining_sec else {
      continue;
    };
    if remaining <= 0.0 {
      continue;
    }
    if soonest.is_none_or(|(best, _)| remaining < best) {
      let alias = entry.get("alias").and_then(Value::as_str).context("a host entry has no string alias")?;
      soonest = Some((remaining, alias));
    }
  }
  Ok(json!({
    "blocked": blocked,
    "soonest_alias": soonest.map(|(_, alias)| alias),
    "soonest_remaining": soonest.map(|(remaining, _)| format_remaining(remaining)),
  }))
}

/// `_format_remaining`: the countdown in its two most significant units.
pub fn format_remaining(seconds: f64) -> String {
  let total = seconds as i64;
  let (days, rest) = (total / 86400, total % 86400);
  let (hours, rest) = (rest / 3600, rest % 3600);
  let minutes = rest / 60;
  if days != 0 {
    format!("{days} d {hours} h")
  } else if hours != 0 {
    format!("{hours} h {minutes} m")
  } else {
    format!("{minutes} m")
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::host_auth::empty_state;

  fn state_with(hosts: Value) -> State {
    let mut state = empty_state();
    state.insert("hosts".to_string(), hosts);
    state
  }

  fn host(alias: &str, status: &str, overrides: Value) -> Value {
    let mut entry = json!({
      "alias": alias,
      "hostname": format!("{alias}.example.internal"),
      "status": status,
      "detail": "",
      "last_probe_at": "2026-09-15T07:30:00+00:00",
      "last_change_at": null,
      "last_ok_at": null,
      "enrolled_observed_at": null,
    });
    if let (Value::Object(base), Value::Object(extra)) = (&mut entry, overrides) {
      base.extend(extra);
    }
    entry
  }

  #[test]
  fn the_remaining_time_shows_its_two_largest_units() {
    assert_eq!(format_remaining(3.0 * 86400.0 + 21.0 * 3600.0 + 59.0), "3 d 21 h");
    assert_eq!(format_remaining(12.0 * 3600.0 + 36.0 * 60.0 + 5.0), "12 h 36 m");
    assert_eq!(format_remaining(9.0 * 60.0 + 30.0), "9 m");
  }

  #[test]
  fn the_deadline_column_follows_the_baseline_and_the_clock() {
    let now = timestamps::parse("2026-09-15T08:00:00+00:00").expect("the stamp parses");
    let counting = host("login-a", STATUS_OK, json!({"enrolled_observed_at": "2026-09-15T07:00:00+00:00"}));
    let expired = host("login-b", STATUS_OK, json!({"enrolled_observed_at": "2026-09-01T07:00:00+00:00"}));
    let no_baseline = host("login-c", STATUS_OK, json!({}));
    let held = host("login-d", STATUS_NEEDS_OKTA, json!({}));
    let state = state_with(json!([counting, expired, no_baseline, held]));
    let rows = host_auth::host_entries(&state)
      .expect("the hosts read")
      .into_iter()
      .map(|entry| page_row(entry, now).expect("the row builds"))
      .collect::<Vec<_>>();
    let kinds: Vec<&str> = rows.iter().map(|row| row["deadline_kind"].as_str().expect("a kind")).collect();
    assert_eq!(kinds, ["counting", "expired", "no_baseline", "pending_okta"]);
    assert_eq!(rows[0]["remaining"], json!("6 d 23 h"));
    assert_eq!(rows[0]["under_24h"], json!(false));
  }

  #[test]
  fn the_summary_names_the_held_hosts_and_the_soonest_expiry() {
    let now = timestamps::parse("2026-09-15T08:00:00+00:00").expect("the stamp parses");
    let state = state_with(json!([
      host("login-a", STATUS_OK, json!({"enrolled_observed_at": "2026-09-15T07:00:00+00:00"})),
      host("login-b", STATUS_OK, json!({"enrolled_observed_at": "2026-09-14T07:00:00+00:00"})),
      host("login-c", STATUS_NEEDS_OKTA, json!({})),
    ]));
    let summary = summary(&state, now).expect("the summary builds");
    assert_eq!(summary["blocked"], json!(["login-c"]));
    assert_eq!(summary["soonest_alias"], json!("login-b"));
    assert_eq!(summary["soonest_remaining"], json!("5 d 23 h"));
  }

  #[test]
  fn the_page_context_contains_the_template_rows() {
    let state =
      state_with(json!([host("login-held", STATUS_NEEDS_OKTA, json!({"detail": "Okta verification required"}))]));
    let now = timestamps::parse("2026-09-15T08:00:00+00:00").expect("the stamp parses");
    let page = context(&state, now).expect("the context builds");
    assert_eq!(page["hosts"][0]["alias"], json!("login-held"));
    assert_eq!(page["hosts"][0]["detail_lines"], json!(["Okta verification required"]));
    assert_eq!(page["hosts"][0]["deadline_kind"], json!("pending_okta"));
  }
}
