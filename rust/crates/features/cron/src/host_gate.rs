//! The host gate of a scheduled task, and the Waiting on you items that the gate's hosts derive.
//!
//! A worker fire of a task whose backend names a host (`host_auth_alias`) probes that host before it creates a worker.
//! A task on a `backends.pools` id is judged on the member the pool selects for the fire, read without moving the
//! pool's rotation. The gate and the fire then agree on the member.
//! A host that awaits the user's approval holds the fire: the fire records one skip and starts nothing, and no session
//! wakes. Nothing is stored for the hold. The Waiting on you read derives one user-act item per holder session and host
//! from the host state and the enabled tasks. The item shows while the host awaits approval and is gone once it does
//! not. Master and handler tasks start no worker, so they probe no host.

use std::collections::HashSet;

use anyhow::Context;
use charliebot_agents::model_pool;
use charliebot_core::approval_policy::{EXECUTOR_CHAO, TIER_USER_ACT};
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_registry::FeatureContext;
use charliebot_registry::registry::{HostLogin, OpenWaitingRow};
use charliebot_store::metadata::UtcDateTime;
use futures_util::future::BoxFuture;

use crate::config::{ScheduledTaskConfig, TaskMode, effective_backend};
use crate::loader;

/// The page where the user approves a host's login, which the derived item links to.
const HOST_AUTH_LINK: &str = "/host-auth";

/// The reason a fire of `task` waits, or `None` when the fire runs. A host that awaits approval holds the fire. A host
/// that answers ok or is unreachable does not, so an unreachable host fails the round as it does without the gate.
pub(crate) async fn hold_reason(
  features: &FeatureContext,
  task: &ScheduledTaskConfig,
  cfg: &CharlieBotConfig,
) -> anyhow::Result<Option<String>> {
  let Some(alias) = task_host(features, task, cfg).await? else {
    return Ok(None);
  };
  let probe = features.registry.host_probe_reader()?;
  let login = probe(features, &alias).await?;
  Ok(login.awaits_approval.then(|| format!("host {} is {}: {}", login.alias, login.status, login.detail)))
}

/// The derived Waiting on you items (`registry.derived_waiting_items`).
pub(crate) fn derived_waiting_rows(features: &FeatureContext) -> BoxFuture<'_, anyhow::Result<Vec<OpenWaitingRow>>> {
  Box::pin(derived_rows(features))
}

/// The alias of the host that a fire of `task` logs into, from its backend's `host_auth_alias`. `None` for a master or
/// handler task, and for a backend type that names no host.
async fn task_host(
  features: &FeatureContext,
  task: &ScheduledTaskConfig,
  cfg: &CharlieBotConfig,
) -> anyhow::Result<Option<String>> {
  if task.mode == Some(TaskMode::Master) || task.handler.is_some() {
    return Ok(None);
  }
  let Some(backend) = fire_backend_id(features, task, cfg).await? else {
    return Ok(None);
  };
  let option = cfg
    .get_backend_option(&backend)
    .ok_or_else(|| anyhow::anyhow!("scheduled task '{}' backend '{backend}' is not configured", task.name))?;
  option_host(features, option).map_err(|error| anyhow::anyhow!("scheduled task '{}': {error}", task.name))
}

/// The alias of the host that `option` logs into, from its backend type's `host_auth_alias`. `None` for a backend type
/// that names no host.
fn option_host(features: &FeatureContext, option: &BackendOption) -> Result<Option<String>, String> {
  let Some(read_alias) = features.registry.backend_host_alias_reader(&option.backend_type) else {
    return Ok(None);
  };
  read_alias(&option.type_fields)
}

/// The backend a fire of `task` runs on: the task's configured option, or for a pool the member the pool selects for
/// the fire. The pool's selection is read without moving its rotation, so the fire's own selection picks the same
/// member. `None` when the pool has no selectable member, since the fire then ends with the exhausted error and runs
/// nothing.
async fn fire_backend_id(
  features: &FeatureContext,
  task: &ScheduledTaskConfig,
  cfg: &CharlieBotConfig,
) -> anyhow::Result<Option<String>> {
  let configured = effective_backend(task, cfg)?;
  let selector = model_pool::Selector::from_features(features, cfg)?;
  Ok(selector.peek(&configured, &[])?.map(|option| option.id))
}

/// One user-act item per holder session and host that awaits approval, for each enabled task on that host whose bound
/// session is open.
async fn derived_rows(features: &FeatureContext) -> anyhow::Result<Vec<OpenWaitingRow>> {
  let cfg = features.config.get()?;
  let (snapshot, _reloaded) = loader::read_snapshot(features, &cfg)?;
  let mut hosted = Vec::new();
  for task in snapshot.tasks.iter().filter(|task| task.enabled) {
    if let Some(alias) = task_host(features, task, &cfg).await? {
      hosted.push((task, alias));
    }
  }
  if hosted.is_empty() {
    return Ok(Vec::new());
  }
  let read_logins = features.registry.host_logins_reader()?;
  let logins = read_logins(features)?;
  let mut seen = HashSet::new();
  let mut rows = Vec::new();
  for (task, alias) in hosted {
    let Some(bound) = task.session_id.as_deref() else {
      continue;
    };
    if features.store.task_state(bound)? != "open" {
      continue;
    }
    let Some(login) = logins.iter().find(|login| login.alias == alias).filter(|login| login.awaits_approval) else {
      continue;
    };
    let (holder_id, holder_name) = holder_of(features, bound).await?;
    if seen.insert((holder_id.clone(), alias)) {
      rows.push(wait_row(holder_id, holder_name, login)?);
    }
  }
  Ok(rows)
}

/// The session that holds a bound task's waits: the bound session's parent while that parent is open, else the bound
/// session itself. A session that cron auto-binds has no parent.
async fn holder_of(features: &FeatureContext, bound: &str) -> anyhow::Result<(String, String)> {
  let meta = features.store.load_meta(bound).await?.with_context(|| format!("bound session {bound} does not exist"))?;
  if let Some(parent_id) = meta.task_parent_id.as_deref()
    && features.store.task_state(parent_id)? == "open"
  {
    let parent =
      features.store.load_meta(parent_id).await?.with_context(|| format!("task parent {parent_id} does not exist"))?;
    return Ok((parent.id, parent.name));
  }
  Ok((meta.id, meta.name))
}

/// The derived item of one host for the session that holds it. Its context is the probe's first line, and its start
/// time is the moment the host's verdict last changed.
fn wait_row(holder_id: String, holder_name: String, login: &HostLogin) -> anyhow::Result<OpenWaitingRow> {
  Ok(OpenWaitingRow {
    session_id: holder_id,
    session_name: holder_name,
    text: format!("Approve the ssh login to {} on {HOST_AUTH_LINK}", login.alias),
    context: Some(login.detail.lines().next().unwrap_or_default().to_string()),
    link: Some(HOST_AUTH_LINK.to_string()),
    tier: Some(TIER_USER_ACT.to_string()),
    rule: None,
    executor: Some(EXECUTOR_CHAO.to_string()),
    approval_rule_refs: None,
    since: UtcDateTime::parse(&login.last_change_at)?,
    is_derived: true,
  })
}

#[cfg(test)]
mod tests {
  use charliebot_agents::backend_availability;
  use charliebot_registry::registry::HostLogin;
  use charliebot_store::metadata::UtcDateTime;
  use serde_json::json;

  use super::{derived_waiting_rows, fire_backend_id};
  use crate::config::ScheduledTaskConfig;
  use crate::test_support::{
    archive_task, auto_session, child_manager, feature_context_with_auto_host_backends,
    feature_context_with_host_backends, manager, set_host_login, write_prompted_task,
  };

  const SINCE: &str = "2026-11-01T21:43:00-07:00";
  const DETAIL: &str = "Synthetic login check\nhttps://login.example.internal/check";

  fn verdict(alias: &str, status: &str, since: &str) -> HostLogin {
    HostLogin {
      alias: alias.to_string(),
      status: status.to_string(),
      detail: DETAIL.to_string(),
      last_change_at: since.to_string(),
      awaits_approval: matches!(status, "needs-interactive-auth" | "needs-okta"),
    }
  }

  fn task(session_id: &str, backend: &str, enabled: bool) -> serde_json::Value {
    json!({
      "cron": "0 3 * * *", "mode": "worker", "enabled": enabled,
      "timezone": "UTC", "session_id": session_id, "backend": backend,
    })
  }

  fn backend_task(backend: &str) -> ScheduledTaskConfig {
    ScheduledTaskConfig {
      name: "selection-test".to_string(),
      cron: "0 3 * * *".to_string(),
      prompt_file: None,
      repo: None,
      backend: Some(backend.to_string()),
      timezone: "UTC".to_string(),
      enabled: true,
      project: None,
      allow_failure: false,
      session_id: None,
      mode: None,
      prompt: Some("synthetic task".to_string()),
      handler: None,
      steps: None,
    }
  }

  #[tokio::test]
  async fn model_and_singleton_pool_fires_peek_the_same_member_and_cooling() {
    let (_home, features) = feature_context_with_auto_host_backends(&[], &["synthetic"]);
    let cfg = features.config.get().unwrap();
    let model = fire_backend_id(&features, &backend_task("synthetic"), &cfg).await.unwrap();
    let pool = fire_backend_id(&features, &backend_task("auto"), &cfg).await.unwrap();
    assert_eq!(model, pool);
    assert_eq!(model.as_deref(), Some("synthetic"));

    backend_availability::backend_availability(&features)
      .unwrap()
      .record("synthetic", None, backend_availability::REASON_QUOTA_ERROR, "synthetic quota error")
      .unwrap();
    assert_eq!(fire_backend_id(&features, &backend_task("synthetic"), &cfg).await.unwrap(), None);
    assert_eq!(fire_backend_id(&features, &backend_task("auto"), &cfg).await.unwrap(), None);
  }

  #[tokio::test]
  async fn a_host_wait_derives_one_item_per_holder_and_host_and_clears_when_the_host_answers_ok() {
    let (_home, features) = feature_context_with_host_backends(&[
      ("synthetic-host", "synthetic-login-host"),
      ("synthetic-other", "synthetic-other-host"),
    ]);
    let home = features.config.get().unwrap().charliebot_home.clone();
    let root = manager(&features, "derive-root").await.unwrap();
    let node_a = child_manager(&features, &root.id, "derive-node-a").await.unwrap();
    let node_b = child_manager(&features, &root.id, "derive-node-b").await.unwrap();
    write_prompted_task(&home, "task-a", &task(&node_a.id, "synthetic-host", true));
    write_prompted_task(&home, "task-b", &task(&node_b.id, "synthetic-host", true));
    write_prompted_task(&home, "task-c", &task(&node_b.id, "synthetic-other", true));

    assert!(derived_waiting_rows(&features).await.unwrap().is_empty(), "no host has answered");
    set_host_login(&home, verdict("synthetic-login-host", "needs-interactive-auth", SINCE));
    let rows = derived_waiting_rows(&features).await.unwrap();
    assert_eq!(rows.len(), 1, "two enabled tasks on one host under one parent share one item: {rows:?}");
    let row = &rows[0];
    assert_eq!(row.session_id, root.id);
    assert_eq!(row.text, "Approve the ssh login to synthetic-login-host on /host-auth");
    assert_eq!(row.context.as_deref(), Some("Synthetic login check"));
    assert_eq!(row.link.as_deref(), Some("/host-auth"));
    assert_eq!(row.tier.as_deref(), Some("user-act"));
    assert_eq!(row.executor.as_deref(), Some("chao"));
    assert_eq!(row.rule, None);
    assert_eq!(row.since.to_str(), UtcDateTime::parse(SINCE).unwrap().to_str());

    set_host_login(&home, verdict("synthetic-other-host", "needs-okta", SINCE));
    let rows = derived_waiting_rows(&features).await.unwrap();
    assert_eq!(rows.len(), 2, "a second host waiting adds its own item: {rows:?}");
    assert!(rows.iter().all(|row| row.session_id == root.id));

    set_host_login(&home, verdict("synthetic-login-host", "ok", "2026-11-02T08:00:00+00:00"));
    let rows = derived_waiting_rows(&features).await.unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].text, "Approve the ssh login to synthetic-other-host on /host-auth");

    write_prompted_task(&home, "task-c", &task(&node_b.id, "synthetic-other", false));
    assert!(derived_waiting_rows(&features).await.unwrap().is_empty(), "a disabled task holds no item");
  }

  #[tokio::test]
  async fn a_bound_session_without_an_open_parent_holds_its_own_item() {
    let (_home, features) = feature_context_with_host_backends(&[("synthetic-host", "synthetic-login-host")]);
    let home = features.config.get().unwrap().charliebot_home.clone();
    let root = manager(&features, "orphan-root").await.unwrap();
    let node = child_manager(&features, &root.id, "orphan-node").await.unwrap();
    write_prompted_task(&home, "orphan", &task(&node.id, "synthetic-host", true));
    set_host_login(&home, verdict("synthetic-login-host", "needs-interactive-auth", SINCE));

    let rows = derived_waiting_rows(&features).await.unwrap();
    assert_eq!(rows.iter().map(|row| row.session_id.as_str()).collect::<Vec<_>>(), [root.id.as_str()]);

    archive_task(&features, &root.id).await;
    let rows = derived_waiting_rows(&features).await.unwrap();
    assert_eq!(rows.iter().map(|row| row.session_id.as_str()).collect::<Vec<_>>(), [node.id.as_str()], "{rows:?}");
  }

  #[tokio::test]
  async fn a_pool_task_is_gated_on_the_member_the_pool_selects_for_its_fire() {
    let (_home, features) = feature_context_with_auto_host_backends(
      &[("synthetic-host", "synthetic-login-host")],
      &["synthetic-host", "synthetic"],
    );
    let home = features.config.get().unwrap().charliebot_home.clone();
    let node = auto_session(&features, "auto-first-fire").await.unwrap();
    write_prompted_task(&home, "auto-first", &task(&node.id, "auto", true));
    set_host_login(&home, verdict("synthetic-login-host", "needs-interactive-auth", SINCE));

    let rows = derived_waiting_rows(&features).await.unwrap();
    assert_eq!(
      rows.iter().map(|row| row.session_id.as_str()).collect::<Vec<_>>(),
      [node.id.as_str()],
      "the pool selects its first member, which names the host that awaits approval: {rows:?}"
    );

    backend_availability::backend_availability(&features)
      .unwrap()
      .record("synthetic-host", None, backend_availability::REASON_QUOTA_ERROR, "spent")
      .unwrap();
    let rows = derived_waiting_rows(&features).await.unwrap();
    assert!(
      rows.is_empty(),
      "the pool selects 'synthetic' once the first member is out, and it names no host: {rows:?}"
    );
  }

  #[tokio::test]
  async fn a_pool_task_whose_selected_member_names_no_host_holds_no_item() {
    let (_home, features) = feature_context_with_auto_host_backends(
      &[("synthetic-host", "synthetic-login-host")],
      &["synthetic", "synthetic-host"],
    );
    let home = features.config.get().unwrap().charliebot_home.clone();
    let node = auto_session(&features, "auto-plain-first").await.unwrap();
    write_prompted_task(&home, "auto-plain-first", &task(&node.id, "auto", true));
    set_host_login(&home, verdict("synthetic-login-host", "needs-interactive-auth", SINCE));

    let rows = derived_waiting_rows(&features).await.unwrap();
    assert!(
      rows.is_empty(),
      "the pool selects 'synthetic', which names no host, so the host's wait holds no item: {rows:?}"
    );
  }

  #[tokio::test]
  async fn a_rotate_pool_task_reads_its_member_without_moving_the_rotation() {
    let (_home, features) = feature_context_with_host_backends(&[("synthetic-host", "synthetic-login-host")]);
    let home = features.config.get().unwrap().charliebot_home.clone();
    let node = manager(&features, "rotate-gate-node").await.unwrap();
    write_prompted_task(&home, "rotate-gate", &task(&node.id, "pool", true));
    set_host_login(&home, verdict("synthetic-login-host", "needs-interactive-auth", SINCE));

    // The rotation's first member is `synthetic`, which names no host. A read that moved the rotation would name
    // `synthetic-login-host` on the second read and hold the task.
    assert!(derived_waiting_rows(&features).await.unwrap().is_empty());
    assert!(derived_waiting_rows(&features).await.unwrap().is_empty(), "a read leaves the rotation in place");
  }

  #[tokio::test]
  async fn an_unknown_backend_id_still_fails_the_derived_rows() {
    let (_home, features) = feature_context_with_host_backends(&[("synthetic-host", "synthetic-login-host")]);
    let home = features.config.get().unwrap().charliebot_home.clone();
    let node = manager(&features, "unknown-backend-node").await.unwrap();
    write_prompted_task(&home, "unknown-backend", &task(&node.id, "missing-backend", true));

    let error = derived_waiting_rows(&features).await.unwrap_err().to_string();
    assert!(error.contains("missing-backend") && error.contains("backends.options"), "{error}");
  }
}
