//! The cron editor's Backend dropdown: the modal part lists `picker_entries`, the list the New Session dropdown lists.

use std::path::Path;

use charliebot_core::config::{BackendOption, BackendsConfig, CallsConfig, PoolConfig, PoolMember, PoolRule};
use charliebot_registry::templating::Templating;
use regex::Regex;
use serde_json::{Value, json};
use serde_yaml::Mapping;

use crate::test_support::feature_context;

fn repo_root() -> &'static Path {
  Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(4).expect("the crate lies under the repo")
}

fn option(id: &str, visible: bool) -> BackendOption {
  BackendOption {
    id: id.to_string(),
    label: format!("{id} label"),
    model: format!("{id}-model"),
    prompt_overlay: None,
    billing: None,
    backend_type: "synthetic".to_string(),
    type_fields: Mapping::new(),
    visible,
  }
}

fn pool(id: &str, member: &str, visible: bool) -> PoolConfig {
  PoolConfig {
    id: id.to_string(),
    label: format!("{id} label"),
    rule: PoolRule::Priority,
    members: vec![PoolMember { backend: member.to_string() }],
    visible,
  }
}

/// The `(value, label)` pairs of the `<option>` elements in the `<select id="...">` the page renders.
fn select_options(html: &str, id: &str) -> Vec<(String, String)> {
  let start = html.find(&format!("<select id=\"{id}\"")).expect("the page renders the select");
  let end = start + html[start..].find("</select>").expect("the select closes");
  let option = Regex::new(r#"<option value="([^"]*)"[^>]*>([^<]*)</option>"#).expect("the option pattern compiles");
  option.captures_iter(&html[start..end]).map(|found| (found[1].to_string(), found[2].to_string())).collect()
}

#[test]
fn the_backend_dropdown_lists_each_visible_option_and_pool_in_picker_order() {
  let (_home, features) = feature_context();
  let backends = BackendsConfig {
    routing: None,
    options: vec![option("claude-visible", true), option("claude-hidden", false), option("codex-visible", true)],
    pools: vec![pool("pool-hidden", "codex-visible", false), pool("pool-visible", "claude-visible", true)],
    calls: CallsConfig::default(),
  };
  let entries: Vec<Value> =
    backends.picker_entries().into_iter().map(|(id, label)| json!({"id": id, "label": label})).collect();
  let (parts, _) = axum::http::Request::builder().uri("/").body(()).expect("the request builds").into_parts();
  let templating = Templating::new(repo_root(), &["features.cron"]);
  let html = templating
    .render_page_template(&features, "cron/page_modals.html", &json!({"picker_entries": entries}), &parts)
    .expect("the cron modal renders");

  let options = select_options(&html, "cron-backend");
  let (inherit_value, _) = options.first().expect("the dropdown leads with the inherit option");
  assert_eq!(inherit_value, "");
  let listed: Vec<(&str, &str)> = options[1..].iter().map(|(id, label)| (id.as_str(), label.as_str())).collect();
  assert_eq!(
    listed,
    [
      ("claude-visible", "claude-visible label"),
      ("codex-visible", "codex-visible label"),
      ("pool-visible", "pool-visible label")
    ],
  );

  let page_html = templating
    .render_page_template(
      &features,
      "index.html",
      &json!({
        "hostname": "test-host",
        "sessions_root": "/tmp/sessions",
        "static_asset_version": "v1",
        "active_session": Value::Null,
        "event_count": 0,
        "session_bootstrap_json": "{}",
        "initial_sessions_json": "[]",
        "load_errors": [],
        "backend_options": [],
        "backend_compact": {},
        "backend_pools": [],
        "picker_entries": entries,
        "active_pool_member": Value::Null,
        "default_backend_id": "claude-visible",
        "active_backend": "claude-visible",
        "active_backend_label": "claude-visible label",
        "auth_enabled": false,
        "version": "0.1.0",
        "pending_draft": Value::Null,
      }),
      &parts,
    )
    .expect("the index page renders");
  let new_session_options = select_options(&page_html, "new-session-backend");
  let page_cron_options = select_options(&page_html, "cron-backend");
  assert_eq!(
    page_cron_options[1..],
    new_session_options[..],
    "the cron dropdown options match new session options in value, label, and order"
  );
}
