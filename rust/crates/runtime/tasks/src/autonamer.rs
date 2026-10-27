//! Session naming after a finished manager round.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use charliebot_agents::agent_environment::AgentBaseEnvironment;
use charliebot_agents::backend_pool::{self, BackendPool};
use charliebot_agents::backend_types;
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_core::event_types;
use charliebot_registry::backend::LaunchArgs;
use charliebot_registry::context::FeatureContext;
use charliebot_store::compact::CompactValue;
use charliebot_store::metadata::SessionMetadata;
use serde_json::Value;

const LIGHT_ONESHOT_TIMEOUT: Duration = Duration::from_secs(30);

pub fn iter_light_backends<'a>(
  cfg: &'a CharlieBotConfig,
  pool: Arc<BackendPool>,
) -> impl Iterator<Item = anyhow::Result<BackendOption>> + 'a {
  let mut tried = Vec::new();
  let mut failed = false;
  std::iter::from_fn(move || {
    if failed {
      return None;
    }
    match pool.select(cfg, "light_oneshot", &tried) {
      Ok(Some(option)) => {
        tried.push(option.id.clone());
        Some(Ok(option))
      }
      Ok(None) => None,
      Err(error) => {
        failed = true;
        Some(Err(error))
      }
    }
  })
}

pub async fn name_after_round(
  features: &FeatureContext,
  cfg: &CharlieBotConfig,
  session_id: &str,
  pool: Arc<BackendPool>,
) -> anyhow::Result<()> {
  let Some(meta) = features.store.get_session(session_id).await? else {
    return Ok(());
  };
  if !is_default_session_name(&meta.name) {
    return Ok(());
  }
  // Python reads the cached event list in `asyncio.to_thread` (autonamer.py:215).
  let store = Arc::clone(&features.store);
  let history_session_id = session_id.to_string();
  let (user_message, assistant_response) = tokio::task::spawn_blocking(move || {
    let events = store.load_chat_events_shared_sync(&history_session_id)?;
    let user_message = events
      .iter()
      .find(|event| event.get("type").and_then(CompactValue::as_str) == Some(event_types::USER))
      .and_then(|event| event.get("content"))
      .and_then(CompactValue::as_str)
      .unwrap_or_default()
      .to_string();
    let assistant_response = events
      .iter()
      .filter(|event| event.get("type").and_then(CompactValue::as_str) == Some(event_types::ASSISTANT))
      .map(|event| extract_text_from_message(event))
      .collect::<String>();
    Ok::<_, charliebot_store::StoreError>((user_message, assistant_response))
  })
  .await
  .context("joining autonamer history reader")??;
  if assistant_response.is_empty() {
    return Ok(());
  }
  let groups = features.store.list_group_names()?;
  if let Err(error) = maybe_auto_name(features, cfg, &meta, &user_message, &assistant_response, &groups, pool).await {
    tracing::warn!(target: "charliebot_tasks", session_id = %session_id, error = %error, "autonamer_failed");
  }
  Ok(())
}

pub async fn maybe_auto_name(
  features: &FeatureContext,
  cfg: &CharlieBotConfig,
  session_meta: &SessionMetadata,
  user_message: &str,
  assistant_response: &str,
  existing_groups: &[String],
  pool: Arc<BackendPool>,
) -> anyhow::Result<()> {
  if !is_default_session_name(&session_meta.name) {
    return Ok(());
  }
  let groups_clause = build_groups_clause(existing_groups);
  let title_instruction = format!(
    "Generate a short, descriptive title (3-6 words) and assign a group for this conversation.\n\
     Return ONLY valid JSON: {{\"name\": \"<title>\", \"group\": \"<group>\"}}\n\
     No explanation, no markdown fences, no extra text.\n\
     Use only names that appear verbatim in the conversation (project, repo, tool names); never invent abbreviations, codes, or new spellings.\n\
     {groups_clause}"
  );
  let system_prompt = format!(
    "You are a title and group generator. Output ONLY valid JSON: {{\"name\": \"<title>\", \"group\": \"<group>\"}}. \
     The name should be 3-6 words, no quotes or punctuation at the end. The group should be a short category (1-3 words). \
     {groups_clause} Use only names that appear verbatim in the conversation (project, repo, tool names); \
     never invent abbreviations, codes, or new spellings. Do not attempt to answer or act on the user's question - just generate the JSON."
  );
  let prompt = format!(
    "{title_instruction}\n\nGenerate a title and group for this conversation:\n\nUser: {}\n\nAssistant: {}",
    take_chars(user_message, 500),
    take_chars(assistant_response, 300)
  );
  if cfg.backends.pool.is_empty() {
    tracing::warn!(target: "charliebot_tasks", reason = "empty_backends_pool", session_id = %session_meta.id, "autonamer_skipped");
    return Ok(());
  }

  let prompt = prompt.as_str();
  let system_prompt = system_prompt.as_str();
  let session_id = session_meta.id.as_str();
  let generated = generate_from_light_backends(cfg, session_id, Arc::clone(&pool), |option| async move {
    generate_name(features, cfg, &option, prompt, system_prompt, session_id).await
  })
  .await?;
  let Some((name, group)) = generated else {
    return Ok(());
  };
  let name = prefix_session_number_if_default(&session_meta.name, &name);
  let group = group.map(|group| fuzzy_match_group(&group, existing_groups));
  apply_name_to_session(features, session_meta, Some(&name), group.as_deref()).await
}

async fn generate_from_light_backends<F, Fut>(
  cfg: &CharlieBotConfig,
  session_id: &str,
  pool: Arc<BackendPool>,
  mut generate: F,
) -> anyhow::Result<Option<(String, Option<String>)>>
where
  F: FnMut(BackendOption) -> Fut,
  Fut: Future<Output = anyhow::Result<String>>,
{
  for option in iter_light_backends(cfg, Arc::clone(&pool)) {
    let option = option?;
    let backend_id = option.id.clone();
    match generate(option).await {
      Ok(raw) => {
        let (name, group) = parse_name_and_group(&raw);
        let Some(name) = name.filter(|name| name.chars().count() <= 60) else {
          tracing::warn!(target: "charliebot_tasks", session_id, error = "empty or unparseable response", "autonamer_failed");
          continue;
        };
        return Ok(Some((name, group)));
      }
      Err(error) => {
        tracing::warn!(target: "charliebot_tasks", session_id, error = %error, "autonamer_failed");
        if backend_pool::is_quota_error(&error.to_string()) {
          pool.cool(&backend_id, None);
        }
      }
    }
  }
  Ok(None)
}

async fn generate_name(
  features: &FeatureContext,
  cfg: &CharlieBotConfig,
  option: &BackendOption,
  prompt: &str,
  system_prompt: &str,
  session_id: &str,
) -> anyhow::Result<String> {
  let launch = LaunchArgs { cgroup_session_id: Some(session_id.to_string()), ..LaunchArgs::default() };
  let built = backend_types::build_backend(Arc::clone(&features.registry), option, cfg, launch)?;
  let base_env = features.startup.require::<AgentBaseEnvironment>()?;
  charliebot_agents::one_shot_text(built, &base_env.values, prompt, system_prompt, Some(LIGHT_ONESHOT_TIMEOUT)).await
}

async fn apply_name_to_session(
  features: &FeatureContext,
  session_meta: &SessionMetadata,
  name: Option<&str>,
  group: Option<&str>,
) -> anyhow::Result<()> {
  let Some(name) = name else { return Ok(()) };
  let Some(current) = features.store.get_session(&session_meta.id).await? else {
    tracing::warn!(target: "charliebot_tasks", session_id = %session_meta.id, "autonamer_session_missing");
    return Ok(());
  };
  if !is_default_session_name(&current.name) {
    return Ok(());
  }
  features.store.rename_session(&session_meta.id, name).await?;
  features
    .store
    .broadcast_only(
      &charliebot_store::session_channel(&session_meta.id),
      serde_json::json!({"type": event_types::SESSION_RENAMED, "name": name}),
    )
    .await?;
  features
    .store
    .broadcast_sidebar(
      &session_meta.id,
      event_types::SESSION_RENAMED,
      serde_json::Map::from_iter([("name".to_string(), serde_json::json!(name))]),
    )
    .await?;
  tracing::info!(target: "charliebot_tasks", session_id = %session_meta.id, name, "session_auto_named");

  let Some(group) = group else { return Ok(()) };
  if let Some(current) = features.store.get_session(&session_meta.id).await?
    && current.group.is_none()
  {
    features.store.set_group(&session_meta.id, Some(group)).await?;
    features
      .store
      .broadcast_sidebar(
        &session_meta.id,
        event_types::SESSION_GROUP_CHANGED,
        serde_json::Map::from_iter([("group".to_string(), serde_json::json!(group))]),
      )
      .await?;
    tracing::info!(target: "charliebot_tasks", session_id = %session_meta.id, group, "session_auto_grouped");
  }
  Ok(())
}

pub fn is_default_session_name(name: &str) -> bool {
  regex::Regex::new(r"^[CE]*(Session \d+|\d+: )$").expect("default session-name expression is valid").is_match(name)
}

pub fn prefix_session_number_if_default(session_name: &str, name: &str) -> String {
  let expression = regex::Regex::new(r"^([CE]*)Session (\d+)$").expect("session-number expression is valid");
  let captures = expression.captures(session_name);
  let Some(captures) = captures else { return name.to_string() };
  format!("{}{}: {name}", &captures[1], &captures[2])
}

pub fn build_groups_clause(existing_groups: &[String]) -> String {
  if existing_groups.is_empty() {
    "Choose a short, descriptive group name (1-3 words).".to_string()
  } else {
    format!(
      "Prefer reusing one of these existing groups: [{}]. Only create a new group if none fit.",
      existing_groups.join(", ")
    )
  }
}

pub fn parse_name_and_group(raw: &str) -> (Option<String>, Option<String>) {
  let stripped = strip_markdown_fences(raw.trim());
  let value: Value = match serde_json::from_str(stripped.trim()) {
    Ok(value) => value,
    Err(error) => {
      tracing::debug!(target: "charliebot_tasks", error = %error, "autonamer_non_json_response");
      return (None, None);
    }
  };
  let Some(object) = value.as_object() else {
    tracing::warn!(target: "charliebot_tasks", response_type = %json_type_name(&value), "autonamer_unexpected_json_type");
    return (None, None);
  };
  let name = nonempty_string(object.get("name"));
  let group = nonempty_string(object.get("group"));
  (name, group)
}

pub fn fuzzy_match_group(group: &str, existing_groups: &[String]) -> String {
  existing_groups
    .iter()
    .find(|existing| existing.to_lowercase() == group.to_lowercase())
    .cloned()
    .unwrap_or_else(|| group.to_string())
}

fn strip_markdown_fences(text: &str) -> &str {
  let expression =
    regex::Regex::new(r"(?s)^```(?:json)?\s*\n?(.*?)\n?\s*```$").expect("markdown-fence expression is valid");
  let Some(captures) = expression.captures(text) else { return text };
  captures.get(1).map_or(text, |body| body.as_str())
}

fn extract_text_from_message(event: &CompactValue) -> String {
  let Some(blocks) = event.get("message").and_then(|message| message.get("content")).and_then(CompactValue::as_array)
  else {
    return String::new();
  };
  blocks
    .iter()
    .filter(|block| block.get("type").and_then(CompactValue::as_str) == Some("text"))
    .filter_map(|block| block.get("text").and_then(CompactValue::as_str))
    .collect()
}

fn nonempty_string(value: Option<&Value>) -> Option<String> {
  value.and_then(Value::as_str).map(str::trim).filter(|text| !text.is_empty()).map(str::to_string)
}

fn take_chars(value: &str, limit: usize) -> String {
  value.chars().take(limit).collect()
}

fn json_type_name(value: &Value) -> &'static str {
  match value {
    Value::Null => "NoneType",
    Value::Bool(_) => "bool",
    Value::Number(number) if number.is_i64() || number.is_u64() => "int",
    Value::Number(_) => "float",
    Value::String(_) => "str",
    Value::Array(_) => "list",
    Value::Object(_) => "dict",
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use charliebot_core::config::{
    BackendsConfig, PathsConfig, RoutingConfig, Sections, ServerConfig, TelegramConfig, UiConfig,
  };
  use serde_yaml::Mapping;
  use std::path::PathBuf;
  use std::sync::Mutex;

  /// A configuration whose two options `ids` are also the pool, with the routing section `routing`.
  fn light_cfg(ids: [&str; 2], routing: Option<RoutingConfig>) -> CharlieBotConfig {
    let options = ids
      .into_iter()
      .map(|id| BackendOption {
        id: id.to_string(),
        label: id.to_string(),
        model: "synthetic-model".to_string(),
        prompt_overlay: None,
        backend_type: "codex".to_string(),
        type_fields: Mapping::new(),
      })
      .collect();
    CharlieBotConfig {
      charliebot_home: PathBuf::from("/synthetic/home"),
      headless_chrome_bin: "/synthetic/chrome".to_string(),
      server: ServerConfig {
        host: "127.0.0.1".to_string(),
        port: 18650,
        subprocess_buffer_limit_mb: 1,
        session_memory_max_mb: 0,
        session_swap_max_mb: 0,
        session_tasks_max: 0,
        min_free_disk_gib: 0,
      },
      paths: PathsConfig {
        workspace_dirs: Vec::new(),
        worktree_dir: "/synthetic/worktrees".to_string(),
        scratch_dir: "/synthetic/scripts".to_string(),
      },
      backends: BackendsConfig { pool: ids.map(str::to_string).to_vec(), routing, options },
      ui: UiConfig { home_services: Vec::new() },
      telegram: TelegramConfig { chat_id: None },
      sections: Sections::default(),
    }
  }

  #[test]
  fn default_names_keep_the_session_number_and_clone_prefix() {
    assert!(is_default_session_name("Session 7"));
    assert!(is_default_session_name("ECSession 7"));
    assert!(is_default_session_name("7: "));
    assert!(!is_default_session_name("7: Topic"));
    assert_eq!(prefix_session_number_if_default("ECSession 7", "Topic"), "EC7: Topic");
    assert_eq!(prefix_session_number_if_default("renamed", "Topic"), "Topic");
  }

  #[test]
  fn name_parse_accepts_fences_and_fuzzy_groups_preserve_casing() {
    assert_eq!(
      parse_name_and_group("```json\n{\"name\":\" Topic \",\"group\":\"rust\"}\n```"),
      (Some("Topic".to_string()), Some("rust".to_string()))
    );
    assert_eq!(fuzzy_match_group("rust", &["Rust".to_string(), "Research".to_string()]), "Rust");
    assert_eq!(parse_name_and_group("[]"), (None, None));
  }

  #[tokio::test]
  async fn quota_from_a_light_backend_cools_the_shared_pool_before_the_next_attempt() {
    let cfg = light_cfg(["light-a", "light-b"], None);
    let pool = Arc::new(BackendPool::default());
    let calls = Arc::new(Mutex::new(Vec::new()));
    let generated = generate_from_light_backends(&cfg, "session-a", Arc::clone(&pool), |option| {
      let calls = Arc::clone(&calls);
      async move {
        calls.lock().expect("scripted call log lock is not poisoned").push(option.id.clone());
        if option.id == "light-a" {
          Err(anyhow::anyhow!("429 quota exceeded"))
        } else {
          Ok(r#"{"name":"Synthetic title","group":"Research"}"#.to_string())
        }
      }
    })
    .await
    .unwrap();

    assert_eq!(*calls.lock().unwrap(), ["light-a".to_string(), "light-b".to_string()]);
    assert_eq!(generated, Some(("Synthetic title".to_string(), Some("Research".to_string()))));
    assert_eq!(pool.select(&cfg, "light_oneshot", &[]).unwrap().unwrap().id, "light-b");
  }

  #[test]
  fn session_naming_never_starts_the_router() {
    // Python: tests/test_backend_router.py:474.
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("router-started");
    let command = vec![
      "/bin/sh".to_string(),
      "-c".to_string(),
      "printf started > \"$1\"; exit 3".to_string(),
      "router".to_string(),
      marker.display().to_string(),
    ];
    let cfg = light_cfg(["fake", "fake2"], Some(RoutingConfig { command }));
    let names = iter_light_backends(&cfg, Arc::new(BackendPool::default()))
      .map(|option| option.map(|option| option.id))
      .collect::<anyhow::Result<Vec<_>>>()
      .unwrap();

    assert_eq!(names, ["fake", "fake2"]);
    assert!(!marker.exists(), "session naming started the router");
  }
}
