//! The approval page's render: one seeded day renders the three lists, the session links and the day navigation.

use std::path::Path;

use axum::Router;
use axum::body::to_bytes;
use charliebot_registry::templating::Templating;
use http::{Method, Request};
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
          {"text": "Log into the console", "link": null, "since": "2026-10-30T14:00:00Z", "since_exact": true, "tier": "user-act", "rule": null},
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
    .insert(Templating::new(repo_root(), &["src.features.approval"]))
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

#[tokio::test]
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

  // The queue lists every open item whatever the day; the login sheet and the digest are the day's.
  let (_, other) = render(&home, "/approval?date=2026-01-01").await;
  assert!(other.contains("Delete the backup volume"), "{other}");
  assert!(!other.contains("Chose the layout."), "{other}");
  assert!(other.contains("No actions that only you can do on 2026-01-01."), "{other}");
  assert!(other.contains("No digest entries and no policy table changes on 2026-01-01."), "{other}");
}

#[tokio::test]
async fn a_mistyped_date_refuses_before_any_read() {
  let (home, _session) = seeded_home().await;

  let (status, body) = render(&home, "/approval?date=soon").await;

  assert_eq!(status, 422, "{body}");
  assert!(body.contains("YYYY-MM-DD"), "{body}");
}
