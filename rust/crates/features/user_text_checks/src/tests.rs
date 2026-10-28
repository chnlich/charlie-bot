use std::fs;

use charliebot_compat::file_urls::ScanContext;
use charliebot_registry::Registry;
use tempfile::TempDir;
use uuid::Uuid;

use super::*;

fn loaded_checks(config_text: &str) -> (TempDir, UserTextChecks) {
  let home = tempfile::tempdir().expect("the synthetic home exists");
  let sessions = home.path().join("sessions");
  fs::create_dir(&sessions).expect("the sessions directory exists");
  fs::write(home.path().join("config.yaml"), config_text).expect("the synthetic config writes");
  let mut registry = Registry::new();
  registry.register_package("tests.user_text_checks", register).expect("the user text checks package registers");
  let config = registry.load_config(home.path()).expect("the synthetic config loads");
  let checks = UserTextChecks::new(config.section::<UserTextChecksConfig>(CONFIG_SECTION), home.path())
    .expect("the checker reads the synthetic session directories");
  (home, checks)
}

fn context(home: &TempDir) -> ScanContext {
  ScanContext {
    server_port: 12345,
    charliebot_home: home.path().to_path_buf(),
    session_dir: home.path().join("sessions/current"),
    user_home: home.path().to_path_buf(),
  }
}

fn add_session(home: &TempDir, id: &str) {
  fs::create_dir(home.path().join("sessions").join(id)).expect("the synthetic session directory exists");
}

fn violations(checks: &UserTextChecks, home: &TempDir, text: &str) -> Vec<TextViolation> {
  checks.check_reply(text, &context(home)).expect("the synthetic text checks")
}

fn count_named(violations: &[TextViolation], name: &str) -> usize {
  violations.iter().filter(|violation| violation.check == name).count()
}

#[test]
fn config_loader_accepts_the_section_and_empty_patterns_match_nothing() {
  let (home, checks) = loaded_checks("user_text_checks: {}\n");

  assert!(violations(&checks, &home, "internal id zz123").is_empty());
}

#[test]
fn invalid_pattern_refuses_config_load_with_the_pattern_named() {
  let home = tempfile::tempdir().expect("the synthetic home exists");
  fs::write(home.path().join("config.yaml"), "user_text_checks:\n  internal_id_patterns: ['[']\n")
    .expect("the synthetic config writes");
  let mut registry = Registry::new();
  registry.register_package("tests.user_text_checks", register).expect("the user text checks package registers");

  let error = registry.load_config(home.path()).expect_err("the invalid pattern fails config load").to_string();

  assert!(error.contains("pattern \"[\""), "{error}");
}

#[test]
fn tracker_id_needs_a_linear_id_within_40_characters() {
  let (home, checks) = loaded_checks("user_text_checks:\n  internal_id_patterns: ['zz\\d{3}']\n");
  for (text, expected) in [
    (format!("ABC-12{}zz123", " ".repeat(40)), 0),
    (format!("ABC-12{}zz123", " ".repeat(41)), 1),
    (format!("zz123{}ABC-12", " ".repeat(40)), 0),
    (format!("zz123{}ABC-12", " ".repeat(41)), 1),
  ] {
    let count = count_named(&violations(&checks, &home, &text), TRACKER_ID_NEARBY);
    assert_eq!(count, expected, "{text}");
  }
}

#[test]
fn page_tracker_blocks_include_table_rows_code_spans_and_pre_text() {
  let (home, checks) = loaded_checks("user_text_checks:\n  internal_id_patterns: ['zz\\d{3}']\n");
  let at_40 = format!("<table><tr><td>ABC-12</td><td>{}zz123</td></tr></table>", " ".repeat(40));
  let at_41 = format!("<table><tr><td>ABC-12</td><td>{}zz123</td></tr></table>", " ".repeat(41));
  let with_code = "<p><code>zz234</code></p><pre>zz345</pre>";

  assert_eq!(count_named(&checks.check_page(&at_40).unwrap(), TRACKER_ID_NEARBY), 0);
  assert_eq!(count_named(&checks.check_page(&at_41).unwrap(), TRACKER_ID_NEARBY), 1);
  let hits = checks.check_page(with_code).unwrap();
  assert_eq!(count_named(&hits, TRACKER_ID_NEARBY), 2);
  assert!(hits.iter().all(|hit| hit.block.as_deref().is_some_and(|block| block.contains(&hit.snippet))));
  assert!(home.path().join("sessions").is_dir());
}

#[test]
fn session_ids_skip_links_urls_paths_and_long_code_but_check_bare_code() {
  let (home, _) = loaded_checks("");
  let id = Uuid::new_v4().to_string();
  add_session(&home, &id);
  let checks = UserTextChecks::new(&UserTextChecksConfig::default(), home.path()).unwrap();
  let cases = [
    (format!("[job · {}](https://example.invalid/?session={id})", &id[..8]), 0),
    (format!("[job · {}](https://example.invalid/?session={id}#reply)", &id[..8]), 0),
    (id[..8].to_string(), 1),
    (format!("the id is {id}"), 1),
    (format!("https://example.invalid/sessions/{id}"), 0),
    (format!("(file:///{id})"), 0),
    (format!("/tmp/sessions/{id}/metadata.json"), 0),
    (format!("`{id} run-command`"), 0),
    (format!("`{id}`"), 1),
    (format!("```\n{id}\n```"), 0),
  ];
  for (text, expected) in cases {
    assert_eq!(count_named(&violations(&checks, &home, &text), SESSION_LINKED), expected, "{text}");
  }
}

#[test]
fn session_ids_in_pre_and_exact_page_code_follow_the_exemptions() {
  let (home, _) = loaded_checks("");
  let id = Uuid::new_v4().to_string();
  add_session(&home, &id);
  let checks = UserTextChecks::new(&UserTextChecksConfig::default(), home.path()).unwrap();
  let pre = format!("<pre>{id}</pre>");
  let bare_code = format!("<p><code>{id}</code></p>");
  let linked = format!(r#"<p><a href="https://example.invalid/?session={id}">{id}</a></p>"#);

  assert_eq!(count_named(&checks.check_page(&pre).unwrap(), SESSION_LINKED), 0);
  assert_eq!(count_named(&checks.check_page(&bare_code).unwrap(), SESSION_LINKED), 1);
  assert_eq!(count_named(&checks.check_page(&linked).unwrap(), SESSION_LINKED), 0);
}

#[test]
fn ambiguous_short_prefix_is_not_a_session_id() {
  let (home, _) = loaded_checks("");
  let first = Uuid::from_u128(0xdeadbeef000000000000000000000001).to_string();
  let second = Uuid::from_u128(0xdeadbeef000000000000000000000002).to_string();
  add_session(&home, &first);
  add_session(&home, &second);
  let checks = UserTextChecks::new(&UserTextChecksConfig::default(), home.path()).unwrap();

  assert_eq!(count_named(&violations(&checks, &home, "deadbeef"), SESSION_LINKED), 0);
}

#[test]
fn reply_link_renderable_checks_tsv_and_file_server_markdown_only() {
  let (home, checks) = loaded_checks("");
  let context = context(&home);
  let external_tsv = "[data](https://example.invalid/data.TSV?download=1)";
  let external_markdown = "[source](https://github.com/example/project/README.MD)";
  let file_server_markdown = "[source](https://example.invalid/absolute_filepath/tmp/readme.MD?raw=1)";

  assert_eq!(
    count_named(&checks.check_reply(external_tsv, &context).unwrap(), LINK_RENDERABLE),
    1,
    "{:?}",
    file_urls::scan_link_targets(external_tsv)
  );
  assert_eq!(count_named(&checks.check_reply(external_markdown, &context).unwrap(), LINK_RENDERABLE), 0);
  assert_eq!(count_named(&checks.check_reply(file_server_markdown, &context).unwrap(), LINK_RENDERABLE), 1);
}

#[test]
fn page_link_renderable_checks_file_server_markdown_and_tsv_not_external_markdown() {
  let (_, checks) = loaded_checks("");
  let html = concat!(
    "<p><a href=\"https://github.com/example/project/README.md\">source</a> ",
    "<a href=\"/absolute_filepath/tmp/readme.MD?raw=1\">raw</a> ",
    "<a href=\"https://example.invalid/export.TSV?download=1\">table</a></p>",
  );

  let hits = checks.check_page(html).unwrap();

  assert_eq!(count_named(&hits, LINK_RENDERABLE), 2);
  assert!(hits.iter().all(|hit| hit.block.as_deref() == Some("source raw table")));
}
