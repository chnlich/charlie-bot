//! The status page: the template context built from the table's rows, rendered through the shared template handle.

use axum::response::Response;
use charliebot_compat::responses::Failure;
use charliebot_registry::FeatureContext;
use charliebot_registry::templating::{STATIC_ASSET_VERSION_KEY, Templating};
use http::request::Parts;
use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::config::BrowserLoginConfig;
use crate::host_auth::{self, BLOCKED_BACKOFF_SEC, PROBE_INTERVAL_SEC, State};
use crate::rows;
use crate::timestamps::{self, LocalFormat};

const TEMPLATE: &str = "host_auth.html";

/// The host-auth page, rendered with request data available to the registered template globals.
pub fn render(
  features: &FeatureContext,
  state: &State,
  now: OffsetDateTime,
  request: &Parts,
) -> Result<Response, Failure> {
  let cfg = features.config.get().map_err(|error| Failure::new("RuntimeError", error.to_string()))?;
  let logins: Vec<BrowserLoginConfig> = crate::config::browser_logins(&cfg);
  let mut context = context(state, now, &logins).map_err(|error| Failure::new("Exception", error.to_string()))?;
  let templating =
    features.startup.require::<Templating>().map_err(|error| Failure::new("RuntimeError", error.to_string()))?;
  // The concurrency badge's script is the page's only static asset, so it carries the version that busts its cache.
  let asset_version = templating.static_asset_version().map_err(|error| Failure::new("OSError", error.to_string()))?;
  if let Some(object) = context.as_object_mut() {
    object.insert(STATIC_ASSET_VERSION_KEY.to_string(), Value::String(asset_version));
  }
  templating
    .render_page(features, TEMPLATE, &context, request)
    .map_err(|error| Failure::new(error.kind(), error.to_string()))
}

/// The template context: the table's rows and the facts line. The header names its rows in the page script, from the
/// state each row carries.
fn context(state: &State, now: OffsetDateTime, logins: &[BrowserLoginConfig]) -> anyhow::Result<Value> {
  let rows = rows::rows(state, logins, now)?;
  let last_probe = timestamps::local_timestamp(host_auth::opt_str(state, "probed_at")?, LocalFormat::DateZone)?;
  Ok(json!({
    "rows": rows,
    "last_probe": last_probe,
    "interval_label": format!("{} min", PROBE_INTERVAL_SEC / 60),
    "backoff_label": format!("{} h", BLOCKED_BACKOFF_SEC / 3600),
  }))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::host_auth::empty_state;

  fn state_with_hosts(hosts: Value, logins: Value) -> State {
    let mut state = empty_state();
    state.insert("hosts".to_string(), hosts);
    state.insert("logins".to_string(), logins);
    state
  }

  fn login(name: &str, host: Option<&str>) -> BrowserLoginConfig {
    BrowserLoginConfig {
      name: name.to_string(),
      label: format!("{name} label"),
      host: host.map(str::to_string),
      login_command: vec!["/usr/bin/true".to_string()],
      approval_url_pattern: r"https://\S*".to_string(),
      readback_command: vec!["/usr/bin/true".to_string()],
      use_pty: false,
      enter_after_pattern: None,
      user_code_pattern: None,
      input_prompt: None,
      lifetime_sec: None,
    }
  }

  #[test]
  fn the_rows_that_need_attention_come_first() {
    let now = timestamps::parse("2026-11-18T00:30:00+00:00").expect("the stamp parses");
    let state = state_with_hosts(
      json!([{"alias": "host1", "hostname": "host1.example.internal", "status": "router-rejected", "detail": "", "last_probe_at": "2026-11-18T00:29:00+00:00"}]),
      json!({"gh": {"status": "broken", "detail": "missing file: /x"}, "aws": {"status": "ok"}}),
    );
    let logins = [login("gh", None), login("aws", None), login("router", Some("host1"))];
    let page = context(&state, now, &logins).expect("the context builds");
    let states: Vec<&Value> = page["rows"].as_array().expect("the rows").iter().map(|row| &row["state"]).collect();
    assert_eq!(
      states,
      [&json!("needs_user"), &json!("broken"), &json!("ok")],
      "the rows that need attention come first"
    );
  }

  #[test]
  fn the_facts_line_reads_the_probe_clock_from_the_state() {
    let now = timestamps::parse("2026-11-18T00:30:00+00:00").expect("the stamp parses");
    let mut state = empty_state();
    state.insert("probed_at".to_string(), json!("2026-11-18T08:19:00.676000+00:00"));
    let page = context(&state, now, &[]).expect("the context builds");
    assert!(page["last_probe"].as_str().is_some_and(|text| !text.is_empty()));
    assert_eq!(page["interval_label"], json!("27 min"));
    assert_eq!(page["backoff_label"], json!("6 h"));
  }
}
