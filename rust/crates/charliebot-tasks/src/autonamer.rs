//! Session naming after a finished manager round.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use charliebot_agents::backend_types;
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_core::event_types;
use charliebot_registry::backend::LaunchArgs;
use charliebot_registry::context::FeatureContext;
use charliebot_store::metadata::{SessionMetadata, UtcDateTime};
use serde_json::Value;
use time::OffsetDateTime;

const LIGHT_ONESHOT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_COOLDOWN: time::Duration = time::Duration::minutes(30);

/// Select backend pool members for light one-shot work.
pub trait LightBackendPool: Send + Sync {
  fn select(&self, cfg: &CharlieBotConfig, exclude: &[String]) -> Option<BackendOption>;
  fn cool(&self, backend_id: &str, resets_at: Option<UtcDateTime>);
  fn is_quota_error(&self, text: &str) -> bool;
}

/// Round-robin light backend selector owned by one `MasterQueue`.
#[derive(Default)]
pub struct RotatingLightBackendPool {
  state: Mutex<PoolState>,
}

#[derive(Default)]
struct PoolState {
  position: usize,
  cooling_until: HashMap<String, OffsetDateTime>,
}

impl LightBackendPool for RotatingLightBackendPool {
  fn select(&self, cfg: &CharlieBotConfig, exclude: &[String]) -> Option<BackendOption> {
    let members = &cfg.backends.pool;
    let now = OffsetDateTime::now_utc();
    let mut state = self.state.lock().expect("light backend pool mutex is not poisoned");
    for offset in 0..members.len() {
      let index = (state.position + offset) % members.len();
      let member = &members[index];
      if exclude.iter().any(|excluded| excluded == member) {
        tracing::info!(target: "charliebot_tasks", caller = "light_oneshot", member, reason = "excluded by caller", "backend_pool_skipped");
        continue;
      }
      if let Some(until) = state.cooling_until.get(member)
        && *until > now
      {
        tracing::info!(
          target: "charliebot_tasks",
          caller = "light_oneshot",
          member,
          reason = %format!("cooling until {}", until.format(&time::format_description::well_known::Rfc3339).expect("UTC timestamp formats")),
          "backend_pool_skipped"
        );
        continue;
      }
      state.position = (index + 1) % members.len();
      tracing::info!(target: "charliebot_tasks", caller = "light_oneshot", member, position = index, "backend_pool_selected");
      return Some(
        cfg.get_backend_option(member).expect("backends.pool members resolve to configured options").clone(),
      );
    }
    tracing::warn!(target: "charliebot_tasks", caller = "light_oneshot", members = ?members, excluded = ?exclude, "backend_pool_exhausted");
    None
  }

  fn cool(&self, backend_id: &str, resets_at: Option<UtcDateTime>) {
    let now = OffsetDateTime::now_utc();
    let until = resets_at.map(UtcDateTime::into_inner).unwrap_or(now + DEFAULT_COOLDOWN);
    self
      .state
      .lock()
      .expect("light backend pool mutex is not poisoned")
      .cooling_until
      .insert(backend_id.to_string(), until);
    tracing::warn!(
      target: "charliebot_tasks",
      member = backend_id,
      until = %until.format(&time::format_description::well_known::Rfc3339).expect("UTC timestamp formats"),
      reset_known = resets_at.is_some(),
      "backend_pool_cooled"
    );
  }

  fn is_quota_error(&self, text: &str) -> bool {
    const PATTERNS: &[&str] = &[
      "quota exhausted",
      "quota exceeded",
      "insufficient quota",
      "over quota",
      "rate limit exceeded",
      "rate limit reached",
      "rate limit rejected",
      "rate-limit exceeded",
      "rate-limit reached",
      "rate-limit rejected",
      "rate_limited",
      "rate_limit_error",
      "rate limited",
      "too many requests",
      "resource_exhausted",
      "429",
      "out of token",
      "out-of-token",
      "insufficient tokens",
      "tokens exhausted",
      "quota",
      "rate limit",
      "usage limit",
      "budget has been exceeded",
    ];
    let lowered = text.to_lowercase();
    PATTERNS.iter().any(|pattern| lowered.contains(pattern))
  }
}

pub fn iter_light_backends<'a>(
  cfg: &'a CharlieBotConfig,
  pool: &'a dyn LightBackendPool,
) -> impl Iterator<Item = BackendOption> + 'a {
  let mut tried = Vec::new();
  std::iter::from_fn(move || {
    let option = pool.select(cfg, &tried)?;
    tried.push(option.id.clone());
    Some(option)
  })
}

pub async fn name_after_round(
  features: &FeatureContext,
  cfg: &CharlieBotConfig,
  session_id: &str,
  pool: &dyn LightBackendPool,
) -> anyhow::Result<()> {
  let Some(meta) = features.store.get_session(session_id).await? else {
    return Ok(());
  };
  if !is_default_session_name(&meta.name) {
    return Ok(());
  }
  let events = features.store.load_chat_events_sync(session_id)?;
  let user_message = events
    .iter()
    .find(|event| event.get("type").and_then(Value::as_str) == Some(event_types::USER))
    .and_then(|event| event.get("content"))
    .and_then(Value::as_str)
    .unwrap_or_default()
    .to_string();
  let assistant_response = events
    .iter()
    .filter(|event| event.get("type").and_then(Value::as_str) == Some(event_types::ASSISTANT))
    .map(extract_text_from_message)
    .collect::<String>();
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
  pool: &dyn LightBackendPool,
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

  let mut generated = None;
  for option in iter_light_backends(cfg, pool) {
    let attempt = generate_name(features, cfg, &option, &prompt, &system_prompt, &session_meta.id).await;
    let raw = match attempt {
      Ok(raw) => raw,
      Err(error) => {
        tracing::warn!(target: "charliebot_tasks", session_id = %session_meta.id, error = %error, "autonamer_failed");
        if pool.is_quota_error(&error.to_string()) {
          pool.cool(&option.id, None);
        }
        continue;
      }
    };
    let (name, group) = parse_name_and_group(&raw);
    let Some(name) = name.filter(|name| name.chars().count() <= 60) else {
      tracing::warn!(target: "charliebot_tasks", session_id = %session_meta.id, error = "empty or unparseable response", "autonamer_failed");
      continue;
    };
    generated = Some((prefix_session_number_if_default(&session_meta.name, &name), group));
    break;
  }
  let Some((name, group)) = generated else {
    return Ok(());
  };
  let group = group.map(|group| fuzzy_match_group(&group, existing_groups));
  apply_name_to_session(features, session_meta, Some(&name), group.as_deref()).await
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
  let mut built = backend_types::build_backend(Arc::clone(&features.registry), option, cfg, launch)?;
  built.backend.one_shot_text(prompt, system_prompt, Some(LIGHT_ONESHOT_TIMEOUT)).await
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

fn extract_text_from_message(event: &Value) -> String {
  let Some(blocks) = event.get("message").and_then(|message| message.get("content")).and_then(Value::as_array) else {
    return String::new();
  };
  blocks
    .iter()
    .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
    .filter_map(|block| block.get("text").and_then(Value::as_str))
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
}
