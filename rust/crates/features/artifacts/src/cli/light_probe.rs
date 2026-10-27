//! `cold_read.run_light_probe`: the foreground cold read's backend rotation (`src/runtime/cold_read.py`).
//!
//! Each pool member is tried in rotation order with no request timeout; a failure is recorded as
//! `Type: message`, a quota error also cools the member, and the first answer ends the rotation.

use std::sync::Arc;

use charliebot_agents::backend_pool::{self, BackendPool};
use charliebot_agents::backend_types;
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_registry::backend::{Env, LaunchArgs};
use charliebot_registry::{RegisterFn, Registry};

/// One rotation's outcome: the failed attempts in order, then the answering backend and its verbatim answer.
pub(crate) struct LightProbeResult {
  pub attempts: Vec<(String, String)>,
  pub backend_id: Option<String>,
  pub answer: Option<String>,
}

/// The backend packages of `src/app/packages/mod.rs`, in its registration order.
const BACKEND_PACKAGES: &[(&str, RegisterFn)] = &[
  ("src.backends.openai_compatible", charliebot_agents::backends::openai_compatible::register),
  ("src.backends.antigravity", charliebot_agents::backends::antigravity::register),
  ("src.backends.charlie_code", charliebot_agents::backends::charlie_code::register),
  ("src.backends.claude_code", charliebot_agents::backends::claude_code::register),
  ("src.backends.codex", charliebot_agents::backends::codex::register),
  ("src.backends.gemini", charliebot_agents::backends::gemini::register),
  ("src.backends.anthropic_compatible", charliebot_agents::backends::anthropic_compatible::register),
  ("src.backends.opencode", charliebot_agents::backends::opencode::register),
];

/// Send `prompt` to each available light backend in pool rotation order, with no request timeout.
pub(crate) fn run_light_probe(
  cfg: &CharlieBotConfig,
  prompt: &str,
  system_prompt: &str,
) -> Result<LightProbeResult, String> {
  if cfg.backends.pool.is_empty() {
    return Err("no light backends resolvable: backends.pool is empty".to_string());
  }
  let registry = Arc::new(backend_registry()?);
  let pool = BackendPool::default();
  let runtime = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
    .map_err(|error| format!("could not start the probe runtime: {error}"))?;
  let env: Env = std::env::vars().collect();
  let mut attempts = Vec::new();
  let mut tried: Vec<String> = Vec::new();
  loop {
    let option = match pool.select(cfg, "light_oneshot", &tried).map_err(|error| format!("{error:#}"))? {
      Some(option) => option,
      None => break,
    };
    tried.push(option.id.clone());
    match one_member(&runtime, &registry, cfg, &option, &env, prompt, system_prompt) {
      Ok(answer) => {
        return Ok(LightProbeResult { attempts, backend_id: Some(option.id), answer: Some(answer) });
      }
      Err((label, message)) => {
        if backend_pool::is_quota_error(&message) {
          pool.cool(&option.id, None);
        }
        attempts.push((option.id, format!("{label}: {message}")));
      }
    }
  }
  Ok(LightProbeResult { attempts, backend_id: None, answer: None })
}

/// Build one member and ask it; the error carries the label Python's exception class would print.
fn one_member(
  runtime: &tokio::runtime::Runtime,
  registry: &Arc<Registry>,
  cfg: &CharlieBotConfig,
  option: &BackendOption,
  env: &Env,
  prompt: &str,
  system_prompt: &str,
) -> Result<String, (&'static str, String)> {
  let built = backend_types::build_backend(Arc::clone(registry), option, cfg, LaunchArgs::default())
    .map_err(|error| ("ValueError", format!("{error:#}")))?;
  runtime
    .block_on(charliebot_agents::one_shot_text(built, env, prompt, system_prompt, None))
    .map_err(|error| ("RuntimeError", format!("{error:#}")))
}

/// The registry of the backend packages: `build_backend` needs each type's registered factory.
fn backend_registry() -> Result<Registry, String> {
  let mut registry = Registry::new();
  for &(package, register) in BACKEND_PACKAGES {
    registry.register_package(package, register).map_err(|error| format!("{error}"))?;
  }
  Ok(registry)
}
