//! The approval page's render: one seeded day renders the three lists, the session links and the day navigation.

use std::path::Path;

use axum::Router;
use axum::body::to_bytes;
use charliebot_registry::templating::Templating;
use http::{Method, Request};
use jiff::Timestamp;
use jiff::civil::Date;
use serde_json::json;
use tower::ServiceExt;

use crate::test_support::Home;

fn repo_root() -> &'static Path {
  Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(4).expect("the crate lies under the repo")
}

async fn seeded_home() -> (Home, String) {
  let home = Home::new();
  let session = home.manager("Alpha").await;
  home
    .set_web_status(
      &session,
      json!({
        "now": "Running",
        "waiting": [
          {"text": "Delete the backup volume", "link": null, "since": "2026-10-30T16:00:00Z", "since_exact": true, "tier": "t0", "rule": "irreversible"},
          {"text": "Log into the console", "link": null, "since": "2026-10-30T14:00:00Z", "since_exact": true, "tier": "user-act", "rule": null, "executor": "chao", "approval_rule_refs": []},
        ],
        "updated_at": "2026-10-30T16:30:00Z",
      }),
    )
    .await;
  charliebot_core::approval_digest::add(
    &home.root(),
    charliebot_core::approval_digest::DigestCall {
      session: "seed-session",
      policy: "t2",
      step: "Chose the layout.",
      undo: "Re-run with the other reading.",
      scope_check: None,
      link: None,
      now: "2026-10-30T18:30:00+00:00",
    },
  )
  .expect("the digest entry records");
  home
    .features
    .startup
    .insert(Templating::new(repo_root(), &["features.approval"]))
    .expect("the templating inserts once");
  (home, session)
}

async fn render(home: &Home, uri: &str) -> (http::StatusCode, String) {
  let router: Router = crate::page::routes(&home.features);
  let request = Request::builder().method(Method::GET).uri(uri).body(axum::body::Body::empty()).unwrap();
  let response = router.oneshot(request).await.expect("the router is infallible");
  let status = response.status();
  let bytes = to_bytes(response.into_body(), usize::MAX).await.expect("the body reads");
  (status, String::from_utf8(bytes.to_vec()).expect("the page is UTF-8"))
}

/// The page rendered from a view read at `now`, so the age test asserts against a fixed clock.
async fn render_at(home: &Home, date: Option<Date>, now: Timestamp) -> (http::StatusCode, String) {
  let view = crate::day::approval_day_view_at(&home.features, date, None, now).await.expect("the view reads");
  let (parts, _) =
    Request::builder().method(Method::GET).uri("/approval").body(()).expect("the request builds").into_parts();
  let response = super::render(&home.features, view, &parts).expect("the page renders");
  let status = response.status();
  let bytes = to_bytes(response.into_body(), usize::MAX).await.expect("the body reads");
  (status, String::from_utf8(bytes.to_vec()).expect("the page is UTF-8"))
}

#[tokio::test]
#[ignore = "integration"]
async fn the_page_renders_the_day_s_three_lists_and_moves_between_days() {
  let (home, session) = seeded_home().await;

  let (status, body) = render(&home, "/approval?date=2026-10-30").await;

  assert_eq!(status, 200, "{body}");
  assert!(body.contains("2026-10-30"), "{body}");
  assert!(body.contains("Delete the backup volume"), "{body}");
  assert!(body.contains("Log into the console"), "{body}");
  assert!(body.contains("Chose the layout."), "{body}");
  assert!(body.contains("Undo: Re-run with the other reading."), "{body}");
  // Each item links to its session, and the view moves between days.
  assert!(body.contains(&format!("/?session={session}")), "{body}");
  assert!(body.contains("/approval?date=2026-10-29"), "{body}");
  assert!(body.contains("/approval?date=2026-10-31"), "{body}");

  // A past day's queue lists every open item; the actions only you can do and the digest are the day's.
  let (_, other) = render(&home, "/approval?date=2026-01-01").await;
  assert!(other.contains("Delete the backup volume"), "{other}");
  assert!(!other.contains("Chose the layout."), "{other}");
  // The day sits in a span that refuses a line break, so a phone never splits 2026-01-01.
  assert!(other.contains("No actions only you can do on <span class=\"day\">2026-01-01</span>."), "{other}");
  assert!(
    other.contains("No digest entries and no policy table changes on <span class=\"day\">2026-01-01</span>."),
    "{other}"
  );
}

/// The three section bodies in rendered order: the actions only you can do, the queue, the digest. Each split names the
/// section's badge, so the helper also holds the order and the numbering.
fn sections(body: &str) -> (&str, &str, &str) {
  let rest = body.split_once("<h2><span class=\"n\">1</span>").expect("the first section renders").1;
  let (actions_only_you_can_do, rest) =
    rest.split_once("<h2><span class=\"n\">2</span>").expect("the second section renders");
  let (queue, digest) =
    rest.split_once("<h2 class=\"digest-heading\"><span class=\"n\">3</span>").expect("the third section renders");
  (actions_only_you_can_do, queue, digest)
}

/// The count the Queue chip shows.
fn queue_chip(body: &str) -> usize {
  let count = body
    .split_once("Queue &middot; ")
    .expect("the Queue chip renders")
    .1
    .split('<')
    .next()
    .expect("the chip holds a count");
  count.trim().parse().expect("the Queue chip holds a number")
}

#[tokio::test]
#[ignore = "integration"]
async fn today_s_queue_defers_user_act_rows_to_the_actions_only_you_can_do_and_a_past_day_keeps_them() {
  let (home, _session) = seeded_home().await;
  let now: Timestamp = "2026-10-30T17:00:00Z".parse().expect("the fixed clock parses");
  let today: Date = "2026-10-30".parse().expect("the fixed day parses");
  let past: Date = "2026-10-29".parse().expect("the fixed past day parses");

  let (status, body) = render_at(&home, Some(today), now).await;
  assert_eq!(status, 200, "{body}");
  let (actions_only_you_can_do, queue, _digest) = sections(&body);
  // The user-act item is one row, and today it sits in the actions only you can do rather than the queue.
  assert!(actions_only_you_can_do.contains("Log into the console"), "{body}");
  assert!(!queue.contains("Log into the console"), "{body}");
  assert!(queue.contains("Delete the backup volume"), "{body}");

  // A past day's queue lists the same item, as it did before this change.
  let (status, body) = render_at(&home, Some(past), now).await;
  assert_eq!(status, 200, "{body}");
  let (_actions_only_you_can_do, queue, _digest) = sections(&body);
  assert!(queue.contains("Log into the console"), "{body}");
}

#[tokio::test]
#[ignore = "integration"]
async fn the_queue_chip_counts_the_rows_the_queue_lists() {
  let (home, _session) = seeded_home().await;
  let now: Timestamp = "2026-10-30T17:00:00Z".parse().expect("the fixed clock parses");
  let today: Date = "2026-10-30".parse().expect("the fixed day parses");
  let past: Date = "2026-10-29".parse().expect("the fixed past day parses");

  for (date, expected) in [(Some(today), 1), (Some(past), 2)] {
    let (status, body) = render_at(&home, date, now).await;
    assert_eq!(status, 200, "{body}");
    let (_actions_only_you_can_do, queue, _digest) = sections(&body);
    let rows = queue.matches("<li").count();
    assert_eq!(rows, expected, "{body}");
    assert_eq!(queue_chip(&body), rows, "{body}");
  }
}

#[tokio::test]
#[ignore = "integration"]
async fn a_mistyped_date_refuses_before_any_read() {
  let (home, _session) = seeded_home().await;

  let (status, body) = render(&home, "/approval?date=soon").await;

  assert_eq!(status, 422, "{body}");
  assert!(body.contains("YYYY-MM-DD"), "{body}");
}

#[tokio::test]
#[ignore = "integration"]
async fn a_queue_row_and_an_actions_only_you_can_do_row_show_their_age() {
  let home = Home::new();
  let session = home.manager("Alpha").await;
  home
    .set_web_status(
      &session,
      json!({
        "now": "Running",
        "waiting": [
          {"text": "Delete the backup volume", "link": null, "since": "2026-10-30T17:00:00Z", "since_exact": true, "tier": "t0", "rule": "irreversible"},
          {"text": "Log into the console", "link": null, "since": "2026-11-02T12:00:00Z", "since_exact": true, "tier": "user-act", "rule": null, "executor": "chao", "approval_rule_refs": []},
        ],
        "updated_at": "2026-11-02T17:00:00Z",
      }),
    )
    .await;
  home
    .features
    .startup
    .insert(Templating::new(repo_root(), &["features.approval"]))
    .expect("the templating inserts once");

  let now: Timestamp = "2026-11-02T17:00:00Z".parse().expect("the fixed clock parses");
  let today: Date = "2026-11-02".parse().expect("the fixed day parses");
  let (status, body) = render_at(&home, Some(today), now).await;

  assert_eq!(status, 200, "{body}");
  // The T0 item started exactly three days before the clock, so its one queue row reads 3 d ago.
  assert_eq!(body.matches("3 d ago").count(), 1, "{body}");
  // The user-act item started five hours before the clock. Today it is listed among the actions only you can do.
  assert_eq!(body.matches("5 h ago").count(), 1, "{body}");

  // A past day's view reads the same item in the queue and in that day's actions only you can do.
  let later: Timestamp = "2026-11-03T17:00:00Z".parse().expect("the fixed clock parses");
  let (status, body) = render_at(&home, Some(today), later).await;
  assert_eq!(status, 200, "{body}");
  assert_eq!(body.matches("4 d ago").count(), 1, "{body}");
  assert_eq!(body.matches("1 d ago").count(), 2, "{body}");
}

#[tokio::test]
#[ignore = "integration"]
async fn today_s_view_refreshes_when_the_tab_becomes_visible_and_a_past_day_does_not() {
  let home = Home::new();
  home
    .features
    .startup
    .insert(Templating::new(repo_root(), &["features.approval"]))
    .expect("the templating inserts once");
  let now: Timestamp = "2026-11-02T17:00:00Z".parse().expect("the fixed clock parses");
  let today: Date = "2026-11-02".parse().expect("the fixed day parses");
  let past: Date = "2026-10-30".parse().expect("the fixed past day parses");

  for date in [None, Some(today)] {
    let (status, body) = render_at(&home, date, now).await;
    assert_eq!(status, 200, "{body}");
    // The badge fragment's inlined script also listens for the visibility event, and the shared prologue reloads a
    // page the browser restores, so the reload on becoming visible is the string only today's view carries.
    assert!(body.contains("visibilityState === 'visible') location.reload()"), "{body}");
  }

  let (status, body) = render_at(&home, Some(past), now).await;
  assert_eq!(status, 200, "{body}");
  assert!(!body.contains("visibilityState === 'visible') location.reload()"), "{body}");
}
