//! The live capture step of the switch: the `View:` trailers it reads from the commits the switch moved across, and
//! the exit the switch reports when a capture fails.

use crate::drain_line;
use crate::world::World;

#[test]
#[ignore = "integration"]
fn test_view_trailers_names_each_view_of_each_commit_in_the_range() {
  let world = World::new();
  let before = world.git(&world.clone, &["rev-parse", "HEAD"]);
  world.git(&world.clone, &["commit", "-q", "--allow-empty", "-m", "Open the status panel\n\nView: /?session=abc"]);
  let first = world.git(&world.clone, &["rev-parse", "--short", "HEAD"]);
  world.git(
    &world.clone,
    &["commit", "-q", "--allow-empty", "-m", "Add the plan tray\n\nView: /?session=abc\nView: /plans"],
  );
  let second = world.git(&world.clone, &["rev-parse", "--short", "HEAD"]);

  let run = world.clone_command(&format!("view_trailers {before} HEAD"));

  assert_eq!(run.code, 0, "{}", run.describe());
  // `git rev-list` lists the newest commit first, and each commit's View lines follow its sha.
  assert_eq!(run.stdout.trim(), format!("{second} /?session=abc\n{second} /plans\n{first} /?session=abc"));
}

#[test]
#[ignore = "integration"]
fn test_a_commit_without_a_view_trailer_captures_nothing() {
  let world = World::new();
  let before = world.git(&world.clone, &["rev-parse", "HEAD"]);
  world.git(&world.clone, &["commit", "-q", "--allow-empty", "-m", "No page in this change"]);

  let run = world.clone_command(&format!("view_trailers {before} HEAD"));

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(run.stdout.trim().is_empty(), "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn test_a_failed_live_capture_exits_7_and_keeps_the_new_release_serving() {
  let mut world = World::new();
  world.seed_live_run("run-live", Some("claude-opus"));
  world.record_outcome("run-live", "success");
  let pid = world.live[0].id();

  let run = world.switch_forward_with(pid, &drain_line("run-live"), ":", "return 1");

  assert_eq!(run.code, 7, "{}", world.report(&run));
  assert!(!world.root().join("restored").exists(), "the switch did not switch back: {}", world.report(&run));
  assert!(run.stdout.contains("live screenshot capture failed"), "{}", world.report(&run));
  assert!(!run.stdout.contains("SWITCH COMPLETE:"), "{}", world.report(&run));
}
