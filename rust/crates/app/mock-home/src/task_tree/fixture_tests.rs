//! The serve fixture's backend, checked against the release trial's config. `trial.sh` writes that config, so the test
//! reads the pools from its heredoc, the same text a trial's server loads.

use super::*;

/// The `(id, label)` pairs of the pools in the release trial's config, read from the `backends.pools` section of the
/// heredoc that `trial.sh` writes. A row whose backend id names one of these shows that pool's label, not its id.
fn trial_pool_labels() -> Vec<(&'static str, &'static str)> {
  const TRIAL_SCRIPT: &str = include_str!("../../../../../../scripts/release/trial.sh");
  let mut pools: Vec<(&'static str, &'static str)> = Vec::new();
  let mut in_pools = false;
  for line in TRIAL_SCRIPT.lines() {
    if line == "  pools:" {
      in_pools = true;
    } else if line == "  calls:" {
      break;
    } else if in_pools {
      if let Some(id) = line.strip_prefix("    - id: ") {
        pools.push((id, ""));
      } else if let (Some(label), Some(pool)) = (line.strip_prefix("      label: "), pools.last_mut()) {
        pool.1 = label;
      }
    }
  }
  pools
}

/// The serve fixture's row names a backend that the release trial's config declares as a labelled pool, so its
/// sidebar row paints that label in full. A backend id the trial config lacks would paint its raw id.
#[test]
fn the_serve_fixture_backend_resolves_to_a_trial_pool_label() {
  let (_, _, sizes) = session::planned_home(super::tests::SCALED_SESSIONS, 1, Language::Mixed);
  let plan = HomePlan::new(&sizes, 1, Language::Mixed, Shape::ServeTaskTree);
  let fixture = plan.serve_fixture.expect("the serve plan holds its fixture row");
  let backend = plan.sessions[fixture.row].backend;
  let label = trial_pool_labels().into_iter().find(|(id, _)| *id == backend).map(|(_, label)| label);
  assert!(label.is_some_and(|label| !label.is_empty()), "the trial config declares no labelled pool `{backend}`");
}
