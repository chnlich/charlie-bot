//! Replays one Waiting on you item through the server's waiting-item judge.
//!
//! ```text
//! CHARLIEBOT_HOME=<profile home> cargo run -p charliebot-cli --example waiting_judge_replay -- \
//!   <fixture.json> [--judge <option-id>]
//! ```
//!
//! The fixture holds one incident's session text: the item, the Goal, the user messages, the memo as recovered and,
//! when the plan names one, a repository contract. It stays outside the repository. The example builds a scratch home
//! that links every entry of `CHARLIEBOT_HOME` except its sessions and state, so the judge reads today's config,
//! credentials and memory; `state/backend_availability.json` is copied in, so a member failure the replay records
//! lands in the scratch home. The replay session is written under the scratch home, which is deleted when the
//! example exits. The judge makes the call the server makes on its call class, and the verdict and its reason are
//! printed for a human to assess. `--judge` names the one option that class runs on in place of the host's rotation,
//! for a replay that must not depend on a rotation member that fails.
//!
//! The agent environment is built from this process and the checkout's gh and ssh guard wrappers.

use std::env;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use charliebot_agents::agent_environment;
use charliebot_agents::backend_availability::BackendAvailability;
use charliebot_agents::backend_pool::BackendPool;
use charliebot_agents::gh_wrapper;
use charliebot_agents::model_pool::RotationPositions;
use charliebot_agents::ssh_guard;
use charliebot_core::RepoRoot;
use charliebot_core::config::{
  CONFIG_FILENAME, CREDENTIALS_FILENAME, CallTarget, CallsConfig, CharlieBotConfig, ConfigError,
};
use charliebot_core::credentials::load_credentials;
use charliebot_core::event_types::{self, CONFIG_RELOAD_FAILED_EVENT, CREDENTIALS_RELOAD_FAILED_EVENT};
use charliebot_core::hot_reload::HotReload;
use charliebot_registry::FeatureContext;
use charliebot_registry::context::StartupState;
use charliebot_registry::store_hooks::store_hooks;
use charliebot_store::metadata::TaskProfile;
use charliebot_store::{CreateTask, Store, TaskCaller, layout};
use charliebot_web_reply::metadata::WaitingItem;
use charliebot_web_reply::waiting_judge::{self, ContextText, JUDGE_CALL_CLASS, Judgement};
use serde_json::{Map, Value, json};

fn main() -> Result<()> {
  tracing_subscriber::fmt().with_writer(std::io::stderr).init();
  let mut args = env::args().skip(1);
  let mut fixture = None;
  let mut judge = None;
  while let Some(arg) = args.next() {
    match arg.as_str() {
      "--judge" => judge = Some(args.next().context("--judge takes a backends.options id")?),
      _ if fixture.is_none() => fixture = Some(PathBuf::from(arg)),
      _ => bail!("unexpected argument {arg:?}"),
    }
  }
  let fixture = fixture.context("usage: waiting_judge_replay <fixture.json> [--judge <option-id>]")?;
  let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
  runtime.block_on(replay(&fixture, judge.as_deref()))
}

/// The fixture's value under `name`: each recorded field keeps its text under `value`.
fn recorded<'a>(doc: &'a Value, name: &str) -> Option<&'a Value> {
  doc.get(name)?.get("value")
}

async fn replay(fixture: &PathBuf, judge: Option<&str>) -> Result<()> {
  let text = fs::read_to_string(fixture).with_context(|| format!("reading {}", fixture.display()))?;
  let doc: Value = serde_json::from_str(&text).with_context(|| format!("parsing {}", fixture.display()))?;
  let item: WaitingItem =
    serde_json::from_value(recorded(&doc, "waiting_item").context("the fixture records no waiting_item")?.clone())
      .context("the waiting_item is not an item")?;
  let goal = recorded(&doc, "goal").and_then(Value::as_str).context("the fixture records no goal text")?;
  let user_messages = recorded(&doc, "user_messages")
    .and_then(Value::as_array)
    .context("the fixture records no user_messages list")?
    .iter()
    .map(|message| {
      message.get("text").and_then(Value::as_str).map(str::to_string).context("a user message has no text")
    })
    .collect::<Result<Vec<_>>>()?;
  let memo = recorded(&doc, "memo").and_then(Value::as_str);
  let contexts = match doc.get("repository_contract").and_then(Value::as_array) {
    Some(entries) => entries
      .iter()
      .map(|entry| {
        let source = entry.get("source").context("a repository contract has no source")?;
        let path = source.get("path").and_then(Value::as_str).context("a repository contract has no path")?;
        let lines = source.get("lines").and_then(Value::as_str).context("a repository contract has no lines")?;
        let text = entry.get("value").and_then(Value::as_str).context("a repository contract has no value")?;
        Ok(ContextText { label: format!("{path}:{lines}"), text: text.to_string() })
      })
      .collect::<Result<Vec<_>>>()?,
    None => Vec::new(),
  };

  let real_home = PathBuf::from(env::var_os("CHARLIEBOT_HOME").context("CHARLIEBOT_HOME names the profile home")?);
  let scratch = tempfile::Builder::new().prefix("waiting-judge-replay-").tempdir().context("the scratch home")?;
  let home = scratch.path().to_path_buf();
  for entry in fs::read_dir(&real_home).with_context(|| format!("reading {}", real_home.display()))? {
    let entry = entry?;
    if entry.file_name() != "sessions" && entry.file_name() != "state" {
      symlink(entry.path(), home.join(entry.file_name()))?;
    }
  }
  // Backend availability reads stay today's, but a member failure the replay records lands in the scratch home, not
  // the live state the server reads.
  let live_availability = charliebot_core::home::backend_availability_path(&real_home);
  if live_availability.is_file() {
    fs::create_dir_all(charliebot_core::home::state_dir(&home))?;
    fs::copy(&live_availability, charliebot_core::home::backend_availability_path(&home))
      .context("seeding the scratch backend availability")?;
  }

  let features = features_over(&home, judge)?;
  let meta = features
    .store
    .create_task(
      CreateTask {
        request_id: "waiting-judge-replay".to_string(),
        task_parent_id: None,
        profile: TaskProfile::Manager,
        task: None,
        name: Some("waiting-judge-replay".to_string()),
        backend: None,
        group: None,
        session_id: None,
      },
      TaskCaller::Operator,
      Map::new(),
    )
    .await
    .map_err(|error| anyhow!("creating the replay session: {error}"))?;
  let session_id = meta.id;
  match memo {
    Some(memo) => {
      let path = layout::memo_path(&features.store.session_dir(&session_id));
      fs::create_dir_all(path.parent().context("the memo has a session directory")?)?;
      fs::write(path, memo)?;
    }
    None => eprintln!("memo: not recovered for this fixture; the judge reads an empty memo"),
  }
  for message in &user_messages {
    features
      .store
      .append_chat_event(&session_id, json!({"type": event_types::USER, "content": message}))
      .await
      .map_err(|error| anyhow!("appending a user message: {error}"))?;
  }

  println!("item: {}", item.text);
  let page = charliebot_compat::file_urls::host_file(item.link.as_deref()).map_err(|error| anyhow!("{error}"))?;
  let page_state = match page {
    Some(path) if charliebot_compat::file_urls::is_file(&path)? => format!("read at {}", path.display()),
    Some(path) => format!("absent at {}", path.display()),
    None => "none".to_string(),
  };
  println!("linked page: {page_state}");
  println!("user messages: {}", user_messages.len());
  println!("memo: {}", if memo.is_some() { "recovered" } else { "empty" });
  println!("context texts: {}", contexts.iter().map(|context| context.label.as_str()).collect::<Vec<_>>().join(", "));
  match waiting_judge::judge_item(&features, &session_id, Some(goal), &item, &contexts).await {
    Ok(Judgement::Refused { rule, reason }) => println!("verdict: refuse\nreason: {reason}\ncited: {rule}"),
    Ok(Judgement::Accepted { judged_kind, reason }) => {
      let kind = judged_kind.map_or_else(|| "none".to_string(), |kind| format!("{kind:?}"));
      println!("verdict: accept\nreason: {reason}\njudged_kind: {kind}");
    }
    Err(error) => bail!("the judge reached no verdict: {error:?}"),
  }
  Ok(())
}

/// The config with the judge's call class pinned to `judge` when one is named. The pin replaces every call target, since
/// the judge reads only its own class; a name that is no configured option fails here.
fn judged_config(mut cfg: CharlieBotConfig, judge: Option<&str>) -> Result<CharlieBotConfig, ConfigError> {
  let Some(option) = judge else {
    return Ok(cfg);
  };
  if !cfg.backends.options.iter().any(|configured| configured.id == option) {
    return Err(ConfigError(format!("--judge {option:?} is not a configured backends.options id")));
  }
  cfg.backends.calls = CallsConfig::from_iter([(JUDGE_CALL_CLASS, CallTarget::Backend(option.to_string()))]);
  Ok(cfg)
}

/// The feature context the server builds, over the scratch home: the config and credentials reload from it, and the
/// backend pool, environment and checkout root the judge's model call and effective prompt read are in the startup
/// state.
fn features_over(home: &Path, judge: Option<&str>) -> Result<FeatureContext> {
  let registry = Arc::new(charliebot_packages::registry()?);
  let cfg = judged_config(registry.load_config(home)?, judge)?;
  let reload_registry = Arc::clone(&registry);
  let reload_home = home.to_path_buf();
  let reload_judge = judge.map(str::to_string);
  let config = HotReload::new(
    home.join(CONFIG_FILENAME),
    CONFIG_RELOAD_FAILED_EVENT,
    Box::new(move || judged_config(reload_registry.load_config(&reload_home)?, reload_judge.as_deref())),
  );
  let credential_home = home.to_path_buf();
  let credentials = HotReload::new(
    home.join(CREDENTIALS_FILENAME),
    CREDENTIALS_RELOAD_FAILED_EVENT,
    Box::new(move || load_credentials(&credential_home)),
  );
  let store = Store::new(cfg, registry.metadata_slots(), store_hooks(Arc::clone(&registry)))?;
  let startup = StartupState::default();
  startup.insert(BackendAvailability::load(home)?).map_err(|error| anyhow!("{error}"))?;
  startup.insert(RotationPositions::default()).map_err(|error| anyhow!("{error}"))?;
  let pool = BackendPool::new(
    startup.require::<BackendAvailability>().map_err(|error| anyhow!("{error}"))?,
    Arc::clone(&registry),
    startup.require::<RotationPositions>().map_err(|error| anyhow!("{error}"))?,
  );
  startup.insert(pool).map_err(|error| anyhow!("{error}"))?;
  // The example sits in `target/<profile>/examples`; the agent CLI `charliebot` is built in `target/<profile>`.
  let exe = env::current_exe()?;
  let exe_dir =
    exe.parent().and_then(|examples| examples.parent()).context("the example binary has a profile directory")?;
  // The wrappers live in this checkout, the repository the example is built from.
  let repo_root = fs::canonicalize(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../.."))?;
  let base = agent_environment::agent_base_environment(
    exe_dir,
    &gh_wrapper::dir_under(&repo_root),
    &ssh_guard::dir_under(&repo_root),
  )?;
  startup.insert(base).map_err(|error| anyhow!("{error}"))?;
  // The effective prompt the judge reads comes from this checkout's `prompts/`.
  let checkout = RepoRoot::new(Some(repo_root.into_os_string())).map_err(|error| anyhow!("{error}"))?;
  startup.insert(checkout).map_err(|error| anyhow!("{error}"))?;
  Ok(FeatureContext {
    config: Arc::new(config),
    credentials: Arc::new(credentials),
    store: Arc::new(store),
    registry,
    startup: Arc::new(startup),
  })
}
