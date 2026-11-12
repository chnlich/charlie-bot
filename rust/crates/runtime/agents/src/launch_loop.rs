//! The lifecycle loop shared by manager turns and task runs.

use std::env;
use std::path::PathBuf;

use anyhow::Result;
pub use charliebot_core::run_token::SESSION_ID_ENV;
use charliebot_registry::Registry;
use charliebot_registry::backend::{BackendLifecycle, Env, Launch, LaunchContext, LaunchWatch};
use futures_util::future::BoxFuture;

/// Whether launch-loop child edits apply to a process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChildEnvPolicy {
  /// Remove the inherited session id and apply the registered package edits.
  RegisteredEdits,
  /// Keep the startup agent environment unchanged before backend preparation.
  Inherited,
}

/// One process result: the process exit code and captured stderr tail.
pub type ProcessResult = (i32, String);

/// Runs one planned process and lets the watch request termination on an event.
pub trait RunProcess: Send {
  fn run<'a>(
    &'a mut self,
    launch: &'a Launch,
    watch: Option<&'a mut dyn LaunchWatch>,
    relays: usize,
  ) -> BoxFuture<'a, Result<ProcessResult>>;
}

/// Place, run and relay processes until the lifecycle ends the round.
pub async fn run_launches<P, N, R>(
  ctx: &LaunchContext,
  lifecycle: &dyn BackendLifecycle,
  run_process: &mut P,
  mut native_id: N,
  mut on_relay: R,
) -> Result<usize>
where
  P: RunProcess,
  N: FnMut() -> Option<String>,
  R: FnMut(usize),
{
  let mut launch = lifecycle.place(ctx).await?;
  let mut relays = 0;
  let mut exit_code = 1;
  let mut completed = false;
  let round_result: Result<()> = async {
    loop {
      let mut watch = lifecycle.watch(ctx, &launch);
      let watch_ref = watch.as_mut().map(|value| value.as_mut() as &mut dyn LaunchWatch);
      let (code, stderr) = run_process.run(&launch, watch_ref, relays).await?;
      exit_code = code;
      if !watch.as_ref().is_some_and(|value| value.wants_next(exit_code, &stderr)) {
        completed = true;
        break Ok(());
      }
      let id = native_id();
      let next_launch = lifecycle.next_launch(ctx, &launch, id.as_deref()).await?;
      relays += 1;
      on_relay(relays);
      launch = next_launch;
    }
  }
  .await;
  let id = native_id();
  let cleanup = lifecycle.after_round(ctx, id.as_deref(), completed && exit_code == 0).await;
  match cleanup {
    Err(error) => Err(error),
    Ok(()) => round_result.map(|()| relays),
  }
}

/// Copy the agent base environment, remove its inherited session identity and apply registered edits.
pub fn child_env(inherited: &Env, registry: &Registry) -> Env {
  child_env_with_policy(inherited, registry, ChildEnvPolicy::RegisteredEdits)
}

/// Copy the startup environment and apply the selected launch-loop child edits.
pub fn child_env_with_policy(inherited: &Env, registry: &Registry, policy: ChildEnvPolicy) -> Env {
  let mut values = inherited.clone();
  match policy {
    ChildEnvPolicy::RegisteredEdits => {
      values.remove(SESSION_ID_ENV);
      registry.apply_child_env(&mut values);
    }
    ChildEnvPolicy::Inherited => {}
  }
  values
}

/// Build one child environment in launch order: package edits, session id, then per-run additions.
pub fn build_child_env(
  inherited: &Env,
  registry: &Registry,
  policy: ChildEnvPolicy,
  session_id: Option<&str>,
  overlay: &Env,
) -> Env {
  let mut values = child_env_with_policy(inherited, registry, policy);
  if let Some(session_id) = session_id {
    values.insert(SESSION_ID_ENV.to_string(), session_id.to_string());
  }
  apply_overlay(&mut values, overlay);
  values
}

/// Adds the per-run overlay to a child environment. Each name replaces its value, except PATH. An overlay PATH entry
/// that the child lacks goes first, so the entries that the package edits put in front keep their order.
fn apply_overlay(values: &mut Env, overlay: &Env) {
  for (name, value) in overlay {
    if name != "PATH" {
      values.insert(name.clone(), value.clone());
      continue;
    }
    let current: Vec<PathBuf> = env::split_paths(values.get("PATH").map_or("", String::as_str))
      .filter(|entry| !entry.as_os_str().is_empty())
      .collect();
    let added: Vec<PathBuf> =
      env::split_paths(value).filter(|entry| !entry.as_os_str().is_empty() && !current.contains(entry)).collect();
    let joined = env::join_paths(added.into_iter().chain(current)).expect("a PATH entry holds no path separator");
    values.insert(name.clone(), joined.into_string().expect("a PATH entry is valid Unicode"));
  }
}

/// True when the lifecycle can produce notices from a completed round.
pub fn has_round_notices(lifecycle: &dyn BackendLifecycle) -> bool {
  lifecycle.has_round_notices()
}

#[cfg(test)]
mod tests {
  use super::*;

  fn edit_child_env(env: &mut Env) {
    env.remove("CLAUDECODE");
    env.insert("CHILD_EDIT".to_string(), "1".to_string());
  }

  #[test]
  fn child_environment_removes_session_then_applies_edits() {
    let inherited = Env::from([
      (SESSION_ID_ENV.to_string(), "stale".to_string()),
      ("CLAUDECODE".to_string(), "1".to_string()),
      ("KEPT".to_string(), "yes".to_string()),
    ]);
    let mut registry = Registry::new();
    registry.register_package("test.runtime", |registry| registry.child_env("claude", edit_child_env)).unwrap();

    let result = child_env(&inherited, &registry);

    assert!(!result.contains_key(SESSION_ID_ENV));
    assert!(!result.contains_key("CLAUDECODE"));
    assert_eq!(result["CHILD_EDIT"], "1");
    assert_eq!(result["KEPT"], "yes");
    assert!(inherited.contains_key("CLAUDECODE"));
  }

  #[test]
  fn an_overlay_path_entry_the_child_lacks_goes_first_and_the_edited_order_stays() {
    let mut values = Env::from([("PATH".to_string(), "/guard:/server-bin:/usr/bin".to_string())]);
    let overlay = Env::from([("PATH".to_string(), "/stash:/server-bin:/usr/bin".to_string())]);

    apply_overlay(&mut values, &overlay);

    assert_eq!(values["PATH"], "/stash:/guard:/server-bin:/usr/bin");
  }
}
