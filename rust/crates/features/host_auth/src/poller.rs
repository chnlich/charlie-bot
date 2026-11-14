//! The `host_auth` service: one round at start, then one per standing wake period (60 s).

use std::future::Future;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use charliebot_registry::FeatureContext;
use charliebot_registry::context::StartupStop;
use charliebot_registry::wiring::{Phase, Service, ServiceContext};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tokio::task::JoinHandle;

use crate::browser_login;
use crate::config::HostAuthConfig;
use crate::host_auth::{self, RoundSlot, State};
use crate::probe::{awaits_approval, probe_host_fresh};
use crate::timestamps;

/// The standing wake period of the poller loop.
pub const POLL_WAKE_INTERVAL_SEC: u64 = 60;

/// The service's one loop task. Starting creates it, and stopping cancels and drains it.
#[derive(Default)]
pub struct HostAuthPoller {
  task: Mutex<Option<JoinHandle<()>>>,
}

impl Service for HostAuthPoller {
  fn name(&self) -> &'static str {
    "host_auth"
  }

  fn phase(&self) -> Phase {
    Phase::Ready
  }

  fn start<'a>(&'a self, context: ServiceContext) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move {
      let stop = context.features.startup.stop();
      let features = Arc::clone(&context.features);
      *self.task.lock().expect("the poller task lock is never poisoned") =
        Some(tokio::spawn(poll_loop(stop, features)));
      tracing::info!("host_auth_poller_started");
      Ok(())
    })
  }

  fn stop<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move {
      let task = self.task.lock().expect("the poller task lock is never poisoned").take();
      let Some(task) = task else {
        return Ok(());
      };
      task.abort();
      match task.await {
        Ok(()) => {}
        Err(error) if error.is_cancelled() => {}
        Err(error) => return Err(error.into()),
      }
      tracing::info!("host_auth_poller_stopped");
      // Also end every live browser login after the poller stops.
      browser_login::stop_all().await;
      Ok(())
    })
  }
}

/// One round at once, then one per 60 s wake period. A failed tick is logged and the loop continues.
async fn poll_loop(stop: StartupStop, features: Arc<FeatureContext>) {
  poll_loop_with(stop, move || {
    let features = Arc::clone(&features);
    async move { poller_round(&features).await }
  })
  .await;
}

pub async fn poll_loop_with<F, Fut>(stop: StartupStop, mut round: F)
where
  F: FnMut() -> Fut,
  Fut: Future<Output = anyhow::Result<()>>,
{
  if !first_round(&stop, &mut round).await {
    return;
  }
  loop {
    tokio::time::sleep(Duration::from_secs(POLL_WAKE_INTERVAL_SEC)).await;
    if let Err(error) = round().await {
      tracing::error!(error = %format!("{error:#}"), "host_auth_poll_error");
    }
  }
}

/// The startup round after the gate, then the caller's go-ahead for its periodic loop. It returns false when the gate
/// reports a stop.
async fn first_round<F, Fut>(stop: &StartupStop, round: F) -> bool
where
  F: FnOnce() -> Fut,
  Fut: Future<Output = anyhow::Result<()>>,
{
  if !first_round_gate(stop).await {
    return false;
  }
  if let Err(error) = round().await {
    tracing::error!(error = %format!("{error:#}"), "host_auth_poll_error");
  }
  true
}

/// The wait between the ready signal and the first round.
///
/// The server's startup tail still runs after the ready signal, and the cold-boot census that starts at the
/// `server_ready` line visits the home root where `host_auth.json` sits. This wait puts the first state read
/// past that census's visit.
const FIRST_ROUND_DELAY: Duration = Duration::from_millis(300);

/// The wait for readiness and then the startup tail. It returns false when stopping instead.
async fn first_round_gate(stop: &StartupStop) -> bool {
  if !stop.ready().await {
    return false;
  }
  tokio::time::sleep(FIRST_ROUND_DELAY).await;
  true
}

/// One scheduled round: claims RoundSlot, probes due hosts, runs the DM step, then saves.
pub async fn poller_round(features: &FeatureContext) -> anyhow::Result<()> {
  let (state_path, config_path) = host_auth::default_paths()?;
  poller_round_paths(features, &state_path, &config_path).await
}

pub async fn poller_round_paths(
  features: &FeatureContext,
  state_path: &Path,
  config_path: &Path,
) -> anyhow::Result<()> {
  poller_round_with(features, state_path, config_path, probe_host_fresh).await
}

pub async fn poller_round_with<P, F>(
  features: &FeatureContext,
  state_path: &Path,
  config_path: &Path,
  probe: P,
) -> anyhow::Result<()>
where
  P: Fn(String) -> F,
  F: Future<Output = std::io::Result<(i32, String)>>,
{
  poller_round_with_now(features, state_path, config_path, timestamps::utc_now(), probe).await
}

pub async fn poller_round_with_now<P, F>(
  features: &FeatureContext,
  state_path: &Path,
  config_path: &Path,
  now: time::OffsetDateTime,
  probe: P,
) -> anyhow::Result<()>
where
  P: Fn(String) -> F,
  F: Future<Output = std::io::Result<(i32, String)>>,
{
  let Some(_slot) = RoundSlot::claim() else {
    return Ok(());
  };
  let state = host_auth::run_round_with_now(false, state_path, config_path, now, probe).await?;
  dm_step(features, &state, state_path).await?;
  Ok(())
}

/// The DM step: finds every host whose status awaits approval and differs from dm_status.
/// Sends one Slack bot DM naming every listed host and status, and on success sets dm_status.
pub async fn dm_step(features: &FeatureContext, state: &State, state_path: &Path) -> anyhow::Result<()> {
  let cfg = features.config.get()?;
  let host_auth_cfg = cfg.section::<HostAuthConfig>("host_auth");
  if !host_auth_cfg.enable_slack_dm {
    return Ok(());
  }
  let Some(user) = host_auth_cfg.slack_dm_user.as_deref().filter(|u| !u.trim().is_empty()) else {
    return Ok(());
  };

  let mut awaiting = Vec::new();
  for entry in host_auth::host_entries(state)? {
    let alias = host_auth::alias_of(entry)?;
    let Some(status) = host_auth::opt_str(entry, "status")? else {
      continue;
    };
    if !awaits_approval(status) {
      continue;
    }
    let dm_status = host_auth::opt_str(entry, "dm_status")?;
    if dm_status != Some(status) {
      awaiting.push((alias.to_string(), status.to_string()));
    }
  }

  if awaiting.is_empty() {
    return Ok(());
  }

  let items: Vec<String> = awaiting.iter().map(|(alias, status)| format!("{alias} ({status})")).collect();
  let hosts_text = items.join(", ");
  let public_url = cfg.server.public_url.as_deref().unwrap_or("").trim_end_matches('/');
  let text = format!("<@{user}> SSH login approval needed: {hosts_text}.\nApprove on {public_url}/host-auth");

  let host_aliases: Vec<String> = awaiting.iter().map(|(alias, _)| alias.clone()).collect();
  let send_result = match features.registry.user_dm_sender() {
    Ok(user_dm) => user_dm(features, user, &text).await,
    Err(error) => Err(error),
  };

  match send_result {
    Ok(ts) => {
      tracing::info!(ts = %ts, hosts = ?host_aliases, "host_auth_slack_dm_sent");
      let mut updated_state = state.clone();
      {
        let hosts = updated_state.get_mut("hosts").and_then(Value::as_array_mut).context("hosts array in state")?;
        for (alias, status) in &awaiting {
          for entry in hosts.iter_mut() {
            if entry.get("alias").and_then(Value::as_str) == Some(alias.as_str())
              && let Some(obj) = entry.as_object_mut()
            {
              obj.insert("dm_status".to_string(), json!(status));
            }
          }
        }
      }
      host_auth::save_state(&updated_state, state_path)?;
    }
    Err(error) => {
      tracing::error!(hosts = ?host_aliases, error = %format!("{error:#}"), "host_auth_slack_dm_failed");
    }
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use std::path::PathBuf;

  use charliebot_core::config::CharlieBotConfig;
  use charliebot_core::credentials::load_credentials;
  use charliebot_core::hot_reload::HotReload;
  use charliebot_registry::store_hooks::store_hooks;
  use charliebot_registry::{Registry, StartupState};
  use charliebot_store::Store;

  use std::collections::HashMap;
  use std::sync::OnceLock;

  use super::*;
  use crate::host_auth::{empty_state, host_entries, load_state, save_state};
  use crate::probe::{STATUS_NEEDS_INTERACTIVE_AUTH, STATUS_NEEDS_OKTA, STATUS_OK, STATUS_UNREACHABLE};
  use crate::timestamps::{self, isoformat};

  type TestStateMap = Mutex<HashMap<PathBuf, (Arc<Mutex<Vec<(String, String)>>>, Arc<std::sync::atomic::AtomicBool>)>>;

  fn test_states() -> &'static TestStateMap {
    static STATES: OnceLock<TestStateMap> = OnceLock::new();
    STATES.get_or_init(|| Mutex::new(HashMap::new()))
  }

  fn test_lock() -> &'static tokio::sync::Mutex<()> {
    crate::test_support::round_slot_lock()
  }

  fn test_user_dm<'a>(
    features: &'a FeatureContext,
    user: &'a str,
    text: &'a str,
  ) -> BoxFuture<'a, anyhow::Result<String>> {
    let home = features.config.get().map(|c| c.charliebot_home.clone()).unwrap_or_default();
    let user = user.to_string();
    let text = text.to_string();
    Box::pin(async move {
      let (sent, fail) = {
        let map = test_states().lock().expect("test state lock");
        map.get(&home).cloned().expect("test state registered for home")
      };
      if fail.load(std::sync::atomic::Ordering::SeqCst) {
        anyhow::bail!("synthetic Slack API error: connect timed out");
      }
      sent.lock().expect("sent log").push((user, text));
      Ok("1700000000.000100".to_string())
    })
  }

  struct TestEnv {
    _dir: tempfile::TempDir,
    features: Arc<FeatureContext>,
    state_path: PathBuf,
    config_path: PathBuf,
    sent_dms: Arc<Mutex<Vec<(String, String)>>>,
    dm_fail: Arc<std::sync::atomic::AtomicBool>,
  }

  fn setup_test_env(enable_slack_dm: bool, slack_user: Option<&str>) -> TestEnv {
    let dir = tempfile::tempdir().expect("tempdir");
    let state_path = dir.path().join("host_auth.json");
    let config_path = dir.path().join("ssh_config");

    let mut config_yaml = "server:\n  public_url: https://charlie.example.test:18498\n".to_string();
    if enable_slack_dm {
      config_yaml.push_str(&format!(
        "host_auth:\n  enable_slack_dm: true\n  slack_dm_user: {}\n",
        slack_user.unwrap_or("U_TEST_USER")
      ));
    }
    std::fs::write(dir.path().join("config.yaml"), config_yaml).expect("config.yaml writes");
    std::fs::write(dir.path().join("credentials.yaml"), "slack:\n  bot_token: synthetic\n")
      .expect("credentials.yaml writes");

    let sent_dms = Arc::new(Mutex::new(Vec::new()));
    let dm_fail = Arc::new(std::sync::atomic::AtomicBool::new(false));

    test_states()
      .lock()
      .expect("test state lock")
      .insert(dir.path().to_path_buf(), (Arc::clone(&sent_dms), Arc::clone(&dm_fail)));

    let mut registry = Registry::new();
    registry.register_package("features.host_auth", crate::register).expect("host_auth registers");
    registry.register_package("features.slack", |reg| reg.user_dm(test_user_dm)).expect("features.slack registers");

    let registry = Arc::new(registry);
    let config_registry = Arc::clone(&registry);
    let config_home = dir.path().to_path_buf();
    let config = Arc::new(HotReload::new(
      dir.path().join("config.yaml"),
      "config_reload_failed",
      Box::new(move || config_registry.load_config(&config_home)),
    ));
    let credentials_home = dir.path().to_path_buf();
    let credentials = Arc::new(HotReload::new(
      dir.path().join("credentials.yaml"),
      "credentials_reload_failed",
      Box::new(move || load_credentials(&credentials_home)),
    ));
    let cfg: CharlieBotConfig = registry.load_config(dir.path()).expect("the config loads");
    let store = Store::new(cfg, registry.metadata_slots(), store_hooks(Arc::clone(&registry))).expect("store opens");

    let features = Arc::new(FeatureContext {
      config,
      credentials,
      store: Arc::new(store),
      registry,
      startup: Arc::new(StartupState::default()),
    });

    TestEnv { _dir: dir, features, state_path, config_path, sent_dms, dm_fail }
  }

  #[tokio::test(start_paused = true)]
  async fn the_first_round_waits_for_startup_readiness() {
    let stop = StartupStop::default();
    let rounds = std::sync::atomic::AtomicUsize::new(0);
    let mut initial_round = Box::pin(first_round(&stop, || async {
      rounds.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
      Ok::<(), anyhow::Error>(())
    }));

    let completed_before_ready = tokio::select! {
      completed = &mut initial_round => Some(completed),
      _ = tokio::task::yield_now() => None,
    };

    assert_eq!(completed_before_ready, None, "the first round stays behind the startup gate");
    assert_eq!(rounds.load(std::sync::atomic::Ordering::Relaxed), 0, "no state read starts before readiness");
    stop.mark_ready();
    let completed_before_delay = tokio::select! {
      completed = &mut initial_round => Some(completed),
      _ = tokio::task::yield_now() => None,
    };
    assert_eq!(completed_before_delay, None, "the startup tail finishes before the state read");
    assert_eq!(rounds.load(std::sync::atomic::Ordering::Relaxed), 0, "no state read starts during the delay");
    tokio::time::advance(FIRST_ROUND_DELAY).await;
    assert!(initial_round.await, "the first round runs after the startup tail");
    assert_eq!(rounds.load(std::sync::atomic::Ordering::Relaxed), 1, "one state read starts after readiness");
  }

  #[tokio::test(start_paused = true)]
  async fn a_stop_before_readiness_keeps_the_first_round_shut() {
    let stop = StartupStop::default();
    stop.cancel();

    assert!(!first_round_gate(&stop).await, "a stopping process starts no round");
  }

  #[tokio::test(start_paused = true)]
  async fn the_first_round_sleeps_for_one_interval_before_the_next_round() {
    let stop = StartupStop::default();
    stop.mark_ready();
    let rounds = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let round_count = std::sync::Arc::clone(&rounds);
    let task = tokio::spawn(poll_loop_with(stop, move || {
      let round_count = std::sync::Arc::clone(&round_count);
      async move {
        round_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok::<(), anyhow::Error>(())
      }
    }));

    tokio::task::yield_now().await;
    tokio::time::advance(FIRST_ROUND_DELAY).await;
    tokio::task::yield_now().await;
    assert_eq!(rounds.load(std::sync::atomic::Ordering::Relaxed), 1, "startup performs one round");

    let interval = Duration::from_secs(POLL_WAKE_INTERVAL_SEC);
    tokio::time::advance(interval - Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(rounds.load(std::sync::atomic::Ordering::Relaxed), 1, "the next round waits for its interval");
    tokio::time::advance(Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(rounds.load(std::sync::atomic::Ordering::Relaxed), 2, "the next round starts at its interval");

    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
  }

  #[tokio::test]
  async fn ok_to_awaiting_sends_dm() {
    let _lock = test_lock().lock().await;
    let env = setup_test_env(true, Some("U_SYNTHETIC"));
    std::fs::write(&env.config_path, "Host login-a\n  HostName login-a.example.internal\n").unwrap();
    let minute_ago = isoformat(timestamps::utc_now() - time::Duration::minutes(30));
    let mut seed = empty_state();
    seed.insert(
      "hosts".to_string(),
      json!([{
        "alias": "login-a",
        "hostname": "login-a.example.internal",
        "status": STATUS_OK,
        "detail": "exit 0",
        "last_probe_at": minute_ago,
        "last_change_at": minute_ago,
        "last_ok_at": minute_ago,
        "enrolled_observed_at": null,
      }]),
    );
    save_state(&seed, &env.state_path).unwrap();

    let check_out =
      "# Tailscale SSH requires an additional check.\nTo authenticate, visit: https://login.example.net/a\n";
    poller_round_with(&env.features, &env.state_path, &env.config_path, |_alias| async move {
      Ok((1, check_out.to_string()))
    })
    .await
    .unwrap();

    let dms = env.sent_dms.lock().unwrap().clone();
    assert_eq!(dms.len(), 1);
    assert_eq!(dms[0].0, "U_SYNTHETIC");
    assert_eq!(
      dms[0].1,
      "<@U_SYNTHETIC> SSH login approval needed: login-a (needs-interactive-auth).\nApprove on https://charlie.example.test:18498/host-auth"
    );

    let state = load_state(&env.state_path);
    let hosts = host_entries(&state).unwrap();
    assert_eq!(hosts[0]["dm_status"], json!(STATUS_NEEDS_INTERACTIVE_AUTH));
  }

  #[tokio::test]
  async fn awaiting_to_the_same_kind_does_not_send() {
    let _lock = test_lock().lock().await;
    let env = setup_test_env(true, Some("U_SYNTHETIC"));
    std::fs::write(&env.config_path, "Host login-a\n  HostName login-a.example.internal\n").unwrap();
    let minute_ago = isoformat(timestamps::utc_now() - time::Duration::minutes(30));
    let mut seed = empty_state();
    seed.insert(
      "hosts".to_string(),
      json!([{
        "alias": "login-a",
        "hostname": "login-a.example.internal",
        "status": STATUS_NEEDS_INTERACTIVE_AUTH,
        "detail": "requires check",
        "last_probe_at": minute_ago,
        "last_change_at": minute_ago,
        "last_ok_at": null,
        "enrolled_observed_at": null,
        "dm_status": STATUS_NEEDS_INTERACTIVE_AUTH,
      }]),
    );
    save_state(&seed, &env.state_path).unwrap();

    poller_round_with(&env.features, &env.state_path, &env.config_path, |_alias| async move {
      Ok((1, "requires an additional check".to_string()))
    })
    .await
    .unwrap();

    let dms = env.sent_dms.lock().unwrap().clone();
    assert_eq!(dms.len(), 0, "awaiting to same kind does not send");
  }

  #[tokio::test]
  async fn needs_okta_to_needs_interactive_auth_sends() {
    let _lock = test_lock().lock().await;
    let env = setup_test_env(true, Some("U_SYNTHETIC"));
    std::fs::write(&env.config_path, "Host login-a\n  HostName login-a.example.internal\n").unwrap();
    let hours_ago = isoformat(timestamps::utc_now() - time::Duration::hours(7));
    let mut seed = empty_state();
    seed.insert(
      "hosts".to_string(),
      json!([{
        "alias": "login-a",
        "hostname": "login-a.example.internal",
        "status": STATUS_NEEDS_OKTA,
        "detail": "Okta verification required",
        "last_probe_at": hours_ago,
        "last_change_at": hours_ago,
        "last_ok_at": null,
        "enrolled_observed_at": null,
        "dm_status": STATUS_NEEDS_OKTA,
      }]),
    );
    save_state(&seed, &env.state_path).unwrap();

    poller_round_with(&env.features, &env.state_path, &env.config_path, |_alias| async move {
      Ok((1, "requires an additional check\nhttps://login.example.net/a".to_string()))
    })
    .await
    .unwrap();

    let dms = env.sent_dms.lock().unwrap().clone();
    assert_eq!(dms.len(), 1);
    assert!(dms[0].1.contains("login-a (needs-interactive-auth)"));

    let state = load_state(&env.state_path);
    let hosts = host_entries(&state).unwrap();
    assert_eq!(hosts[0]["dm_status"], json!(STATUS_NEEDS_INTERACTIVE_AUTH));
  }

  #[tokio::test]
  async fn unreachable_to_awaiting_sends() {
    let _lock = test_lock().lock().await;
    let env = setup_test_env(true, Some("U_SYNTHETIC"));
    std::fs::write(&env.config_path, "Host login-a\n  HostName login-a.example.internal\n").unwrap();
    let minute_ago = isoformat(timestamps::utc_now() - time::Duration::minutes(30));
    let mut seed = empty_state();
    seed.insert(
      "hosts".to_string(),
      json!([{
        "alias": "login-a",
        "hostname": "login-a.example.internal",
        "status": STATUS_UNREACHABLE,
        "detail": "Connection timed out",
        "last_probe_at": minute_ago,
        "last_change_at": minute_ago,
        "last_ok_at": null,
        "enrolled_observed_at": null,
      }]),
    );
    save_state(&seed, &env.state_path).unwrap();

    poller_round_with(&env.features, &env.state_path, &env.config_path, |_alias| async move {
      Ok((1, "requires an additional check".to_string()))
    })
    .await
    .unwrap();

    let dms = env.sent_dms.lock().unwrap().clone();
    assert_eq!(dms.len(), 1);
    assert!(dms[0].1.contains("login-a (needs-interactive-auth)"));
  }

  #[tokio::test]
  async fn first_probe_that_awaits_sends() {
    let _lock = test_lock().lock().await;
    let env = setup_test_env(true, Some("U_SYNTHETIC"));
    std::fs::write(&env.config_path, "Host login-a\n  HostName login-a.example.internal\n").unwrap();
    save_state(&empty_state(), &env.state_path).unwrap();

    poller_round_with(&env.features, &env.state_path, &env.config_path, |_alias| async move {
      Ok((1, "requires an additional check".to_string()))
    })
    .await
    .unwrap();

    let dms = env.sent_dms.lock().unwrap().clone();
    assert_eq!(dms.len(), 1);
    assert!(dms[0].1.contains("login-a (needs-interactive-auth)"));
  }

  #[tokio::test]
  async fn ok_deletes_dm_status_in_poller() {
    let _lock = test_lock().lock().await;
    let env = setup_test_env(true, Some("U_SYNTHETIC"));
    std::fs::write(&env.config_path, "Host login-a\n  HostName login-a.example.internal\n").unwrap();
    let minute_ago = isoformat(timestamps::utc_now() - time::Duration::minutes(30));
    let mut seed = empty_state();
    seed.insert(
      "hosts".to_string(),
      json!([{
        "alias": "login-a",
        "hostname": "login-a.example.internal",
        "status": STATUS_NEEDS_INTERACTIVE_AUTH,
        "detail": "requires check",
        "last_probe_at": minute_ago,
        "last_change_at": minute_ago,
        "last_ok_at": null,
        "enrolled_observed_at": null,
        "dm_status": STATUS_NEEDS_INTERACTIVE_AUTH,
      }]),
    );
    save_state(&seed, &env.state_path).unwrap();

    poller_round_with(
      &env.features,
      &env.state_path,
      &env.config_path,
      |_alias| async move { Ok((0, "".to_string())) },
    )
    .await
    .unwrap();

    let dms = env.sent_dms.lock().unwrap().clone();
    assert_eq!(dms.len(), 0);

    let state = load_state(&env.state_path);
    let hosts = host_entries(&state).unwrap();
    assert_eq!(hosts[0]["status"], json!(STATUS_OK));
    assert_eq!(hosts[0].get("dm_status"), None);
  }

  #[tokio::test]
  async fn two_hosts_in_one_wake_give_one_dm() {
    let _lock = test_lock().lock().await;
    let env = setup_test_env(true, Some("U_SYNTHETIC"));
    std::fs::write(
      &env.config_path,
      "Host login-a\n  HostName login-a.example.internal\n\nHost login-b\n  HostName login-b.example.internal\n",
    )
    .unwrap();
    save_state(&empty_state(), &env.state_path).unwrap();

    poller_round_with(&env.features, &env.state_path, &env.config_path, |alias: String| async move {
      if alias == "login-a" {
        Ok((1, "requires an additional check".to_string()))
      } else {
        Ok((124, "Okta verification required".to_string()))
      }
    })
    .await
    .unwrap();

    let dms = env.sent_dms.lock().unwrap().clone();
    assert_eq!(dms.len(), 1, "exactly one DM sent for multiple hosts in same wake");
    assert_eq!(
      dms[0].1,
      "<@U_SYNTHETIC> SSH login approval needed: login-a (needs-interactive-auth), login-b (needs-okta).\nApprove on https://charlie.example.test:18498/host-auth"
    );

    let state = load_state(&env.state_path);
    let hosts = host_entries(&state).unwrap();
    assert_eq!(hosts[0]["dm_status"], json!(STATUS_NEEDS_INTERACTIVE_AUTH));
    assert_eq!(hosts[1]["dm_status"], json!(STATUS_NEEDS_OKTA));
  }

  #[tokio::test]
  async fn failed_send_is_retried_on_next_wake() {
    let _lock = test_lock().lock().await;
    let env = setup_test_env(true, Some("U_SYNTHETIC"));
    std::fs::write(&env.config_path, "Host login-a\n  HostName login-a.example.internal\n").unwrap();
    save_state(&empty_state(), &env.state_path).unwrap();

    // Wake 1: Slack fails
    env.dm_fail.store(true, std::sync::atomic::Ordering::SeqCst);
    poller_round_with(&env.features, &env.state_path, &env.config_path, |_alias| async move {
      Ok((1, "requires an additional check".to_string()))
    })
    .await
    .unwrap();

    assert_eq!(env.sent_dms.lock().unwrap().len(), 0);
    let state = load_state(&env.state_path);
    let hosts = host_entries(&state).unwrap();
    assert_eq!(hosts[0]["status"], json!(STATUS_NEEDS_INTERACTIVE_AUTH));
    assert_eq!(hosts[0].get("dm_status"), None, "dm_status left unchanged after failure");

    // Wake 2: Slack succeeds
    env.dm_fail.store(false, std::sync::atomic::Ordering::SeqCst);
    poller_round_with(&env.features, &env.state_path, &env.config_path, |_alias| async move {
      Ok((1, "requires an additional check".to_string()))
    })
    .await
    .unwrap();

    assert_eq!(env.sent_dms.lock().unwrap().len(), 1);
    let state = load_state(&env.state_path);
    let hosts = host_entries(&state).unwrap();
    assert_eq!(hosts[0]["dm_status"], json!(STATUS_NEEDS_INTERACTIVE_AUTH));
  }

  #[tokio::test]
  async fn missing_state_file_gives_one_dm_per_awaiting_host() {
    let _lock = test_lock().lock().await;
    let env = setup_test_env(true, Some("U_SYNTHETIC"));
    std::fs::write(&env.config_path, "Host login-a\n  HostName login-a.example.internal\n").unwrap();
    // Do not create state_path!

    poller_round_with(&env.features, &env.state_path, &env.config_path, |_alias| async move {
      Ok((1, "requires an additional check".to_string()))
    })
    .await
    .unwrap();

    assert_eq!(env.sent_dms.lock().unwrap().len(), 1);

    // Next wake with missing state file resolved
    poller_round_with(&env.features, &env.state_path, &env.config_path, |_alias| async move {
      Ok((1, "requires an additional check".to_string()))
    })
    .await
    .unwrap();

    assert_eq!(env.sent_dms.lock().unwrap().len(), 1, "no second DM sent on subsequent wake");
  }

  #[tokio::test]
  async fn status_cron_gate_writes_is_sent_on_next_wake() {
    let _lock = test_lock().lock().await;
    let env = setup_test_env(true, Some("U_SYNTHETIC"));
    std::fs::write(&env.config_path, "Host login-a\n  HostName login-a.example.internal\n").unwrap();
    save_state(&empty_state(), &env.state_path).unwrap();

    // Cron gate probes alias and writes verdict to state file without calling DM hook
    host_auth::probe_alias_with(&env.state_path, &env.config_path, "login-a", |_alias| async move {
      Ok((1, "requires an additional check".to_string()))
    })
    .await
    .unwrap();

    assert_eq!(env.sent_dms.lock().unwrap().len(), 0, "cron gate sends no DM");
    let state = load_state(&env.state_path);
    assert_eq!(host_entries(&state).unwrap()[0].get("dm_status"), None);

    // Poller wake runs
    poller_round_with(&env.features, &env.state_path, &env.config_path, |_alias| async move {
      Ok((1, "requires an additional check".to_string()))
    })
    .await
    .unwrap();

    assert_eq!(env.sent_dms.lock().unwrap().len(), 1, "poller wake sends the DM");
    let state = load_state(&env.state_path);
    assert_eq!(host_entries(&state).unwrap()[0]["dm_status"], json!(STATUS_NEEDS_INTERACTIVE_AUTH));
  }

  #[test]
  fn poller_and_probe_now_use_fresh_argv() {
    use charliebot_core::ssh;
    let alias = "synthetic-box";
    let argv = ssh::fresh_argv(alias, &["true"]);
    assert_eq!(
      argv,
      vec!["ssh", "-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "-n", "synthetic-box", "true"]
    );
  }

  #[tokio::test(start_paused = true)]
  async fn paused_clock_off_grid_probe_and_lost_slot_sends_within_1765s() {
    let _lock = test_lock().lock().await;
    let env = setup_test_env(true, Some("U_SYNTHETIC"));
    std::fs::write(&env.config_path, "Host login-a\n  HostName login-a.example.internal\n").unwrap();

    // At T = 0: an off-grid probe occurs, recording ok.
    let t0 = timestamps::parse("2026-09-15T08:00:00+00:00").unwrap();
    let mut seed = empty_state();
    seed.insert(
      "hosts".to_string(),
      json!([{
        "alias": "login-a",
        "hostname": "login-a.example.internal",
        "status": STATUS_OK,
        "detail": "exit 0",
        "last_probe_at": isoformat(t0),
        "last_change_at": isoformat(t0),
        "last_ok_at": isoformat(t0),
        "enrolled_observed_at": null,
      }]),
    );
    save_state(&seed, &env.state_path).unwrap();

    // Expiry happens at T = 1s. Next probe is due at T = 1620s.
    // At T = 1620s, a poller wake arrives, but RoundSlot is held by another task.
    let held_slot = RoundSlot::claim().expect("slot acquired by foreign task");

    // Advance to T = 1620s
    tokio::time::advance(Duration::from_secs(1620)).await;
    let t_1620 = t0 + time::Duration::seconds(1620);

    // Wake 1 at T = 1620s tries to claim slot, fails, and skips.
    let state_1 =
      poller_round_with_now(&env.features, &env.state_path, &env.config_path, t_1620, |_alias| async move {
        panic!("probe should not run when slot is held");
      })
      .await;
    assert!(state_1.is_ok(), "skipped wake returns Ok(())");
    assert_eq!(env.sent_dms.lock().unwrap().len(), 0, "no DM sent when slot was contested");

    // Foreign task releases slot
    drop(held_slot);

    // Advance to next 60s wake at T = 1680s
    tokio::time::advance(Duration::from_secs(60)).await;
    let t_1680 = t0 + time::Duration::seconds(1680);

    // Wake 2 runs: probe takes 20s (PROBE_TIMEOUT) and returns needs-interactive-auth
    let probe_fn = |_alias| async move {
      tokio::time::sleep(Duration::from_secs(20)).await;
      Ok((1, "requires an additional check".to_string()))
    };

    let wake_start = tokio::time::Instant::now();
    poller_round_with_now(&env.features, &env.state_path, &env.config_path, t_1680, probe_fn).await.unwrap();

    let dms = env.sent_dms.lock().unwrap().clone();
    assert_eq!(dms.len(), 1, "DM sent on second wake");

    // Total elapsed time from T = 0: 1620 + 60 + 20 = 1700s <= 1765s!
    let total_elapsed = Duration::from_secs(1680) + wake_start.elapsed();
    assert!(total_elapsed <= Duration::from_secs(1765), "DM sent within 1765s: {:?}", total_elapsed);
  }
}
