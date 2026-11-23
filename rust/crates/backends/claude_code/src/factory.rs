//! The registered cc-claude backend factory.

use anyhow::{Context, bail};
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_registry::backend::{
  AgentBackend, BackendFactory, CLAUDE_CONFIG_DIR_FIELD, EXTRA_FLAGS_BACKEND_KWARG, LaunchArgs,
};

use charliebot_agents::transport::cgroup::SessionCgroupConfig;

use super::ClaudeCodeBackend;
use super::{claude_config, options};

pub struct ClaudeCodeFactory;

impl BackendFactory for ClaudeCodeFactory {
  fn one_shot_is_tool_less(&self) -> bool {
    true
  }

  fn build(
    &self,
    option: Option<&BackendOption>,
    cfg: &CharlieBotConfig,
    launch: LaunchArgs,
  ) -> anyhow::Result<Box<dyn AgentBackend>> {
    let launch = launch_with_kwargs(launch)?;
    let cgroup_config = SessionCgroupConfig::from_config(cfg);
    let enable_subagents = claude_config::enable_subagents(cfg);
    let backend = match option {
      None => ClaudeCodeBackend::from_launch(None, None, false, enable_subagents, None, launch, cgroup_config)?,
      Some(option) => {
        let model = option.required_model("backend").map_err(anyhow::Error::msg)?;
        let effort = options::optional_text(&option.type_fields, options::EFFORT_KEY).map_err(anyhow::Error::msg)?;
        let fast_mode = options::fast_mode(&option.type_fields).map_err(anyhow::Error::msg)?;
        let config_dir = launch_config_dir(&launch)?;
        ClaudeCodeBackend::from_launch(
          Some(model),
          effort,
          fast_mode,
          enable_subagents,
          config_dir,
          launch,
          cgroup_config,
        )?
      }
    };
    Ok(Box::new(backend))
  }
}

/// The launch's `extra_flags` kwarg, taken into the launch's flags when the launch carries none of its own.
pub fn launch_with_kwargs(launch: LaunchArgs) -> anyhow::Result<LaunchArgs> {
  let mut launch = launch;
  if launch.extra_flags.is_empty()
    && let Some(value) = launch.backend_kwargs.get(EXTRA_FLAGS_BACKEND_KWARG).filter(|value| !value.is_null())
  {
    launch.extra_flags = value
      .as_array()
      .context("backend_kwargs.extra_flags must be an array")?
      .iter()
      .map(|value| value.as_str().context("backend_kwargs.extra_flags entries must be strings"))
      .collect::<anyhow::Result<Vec<_>>>()?
      .into_iter()
      .map(str::to_string)
      .collect();
  }
  Ok(launch)
}

/// The account directory the launch names: `claude_config_dir`, else the `claude_account`'s `config_dir`.
pub fn launch_config_dir(launch: &LaunchArgs) -> anyhow::Result<Option<String>> {
  let value = launch
    .backend_kwargs
    .get(CLAUDE_CONFIG_DIR_FIELD)
    .or_else(|| launch.backend_kwargs.get("claude_account").and_then(|account| account.get("config_dir")));
  match value {
    None | Some(serde_json::Value::Null) => Ok(None),
    Some(serde_json::Value::String(value)) => Ok(Some(value.clone())),
    Some(_) => bail!("{CLAUDE_CONFIG_DIR_FIELD} must be a string"),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn factory_declares_its_native_one_shot_tool_less() {
    assert!(ClaudeCodeFactory.one_shot_is_tool_less());
  }
}
