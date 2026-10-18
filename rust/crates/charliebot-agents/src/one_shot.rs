//! Tool-capable single-prompt agent execution.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use charliebot_core::event_types;
use futures_util::future::BoxFuture;
use serde_json::Value;
use tokio::time;

use crate::backend_types::BuiltBackend;
use crate::launch_loop::ChildEnvPolicy;
use crate::runner::{AgentRunner, RunRequest};

/// Run a built backend in a temporary working directory and collect its assistant text.
pub async fn agent_one_shot_text(
  built: BuiltBackend,
  prompt: &str,
  system_prompt: &str,
  timeout: Option<Duration>,
) -> anyhow::Result<String> {
  let agent_environment = crate::agent_environment::startup_agent_environment()?.as_ref().clone();
  agent_one_shot_text_with_environment(built, prompt, system_prompt, timeout, agent_environment).await
}

async fn agent_one_shot_text_with_environment(
  mut built: BuiltBackend,
  prompt: &str,
  system_prompt: &str,
  timeout: Option<Duration>,
  agent_environment: crate::agent_environment::AgentBaseEnvironment,
) -> anyhow::Result<String> {
  built.launch.instructions_file = None;
  built.launch.instructions_content = None;
  let cwd = charliebot_core::tempdir::mkdtemp("charliebot-oneshot-")?;
  let result = Arc::new(Mutex::new(OneShotResult::default()));
  let event_result = Arc::clone(&result);
  let on_event = Arc::new(move |event: Value| -> BoxFuture<'static, anyhow::Result<()>> {
    let result = Arc::clone(&event_result);
    Box::pin(async move {
      let mut result = result.lock().expect("one-shot result lock is not poisoned");
      match event.get("type").and_then(Value::as_str) {
        Some(event_types::RESULT) => result.saw_result = true,
        Some(event_types::ASSISTANT) if !result.saw_result => {
          result.text.push_str(&assistant_text(&event));
        }
        _ => {}
      }
      Ok(())
    })
  });

  let runner = AgentRunner::new(built.backend, built.launch.clone());
  let mut request = RunRequest::new(
    frame_system_prompt(system_prompt, prompt),
    cwd.path().to_path_buf(),
    agent_environment,
    crate::transport::cgroup::system_cgroup_base_dir(),
    built.registry,
  );
  request.child_env_policy = ChildEnvPolicy::Inherited;
  request.on_event = on_event;
  match timeout {
    Some(limit) => match time::timeout(limit, runner.run(request)).await {
      Ok(result) => {
        result.context("running tool-capable one-shot backend")?;
      }
      Err(error) => {
        runner.terminate().await.context("terminating timed-out one-shot backend")?;
        return Err(error.into());
      }
    },
    None => {
      runner.run(request).await.context("running tool-capable one-shot backend")?;
    }
  }
  let result = result.lock().expect("one-shot result lock is not poisoned");
  Ok(result.text.trim().to_string())
}

#[derive(Default)]
struct OneShotResult {
  text: String,
  saw_result: bool,
}

pub(crate) fn frame_system_prompt(system_prompt: &str, prompt: &str) -> String {
  format!("<system-instructions>\n{system_prompt}\n</system-instructions>\n\n{prompt}")
}

pub(crate) fn assistant_text(event: &Value) -> String {
  let Some(blocks) = event.get("message").and_then(|message| message.get("content")).and_then(Value::as_array) else {
    return String::new();
  };
  blocks
    .iter()
    .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
    .filter_map(|block| block.get("text").and_then(Value::as_str))
    .collect()
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::path::Path;

  use charliebot_registry::Registry;
  use charliebot_registry::backend::{AgentBackend, Env, LaunchArgs, Translate, TransportKind};
  use serde_json::json;

  struct ScriptedBackend {
    script: String,
  }

  impl AgentBackend for ScriptedBackend {
    fn instructions_target(&self) -> Option<(&'static str, &'static str)> {
      None
    }

    fn prepare_transport(&mut self, _log_dir: &Path) -> anyhow::Result<()> {
      Ok(())
    }

    fn build_command(&self, _prompt: &str) -> anyhow::Result<Vec<String>> {
      Ok(vec!["/bin/sh".to_string(), "-c".to_string(), self.script.clone()])
    }

    fn prepare_env(&self, env: Env) -> anyhow::Result<Env> {
      Ok(env)
    }

    fn stdin_prompt(&self, _prompt: &str) -> Option<String> {
      None
    }

    fn translator(&self) -> Box<dyn Translate> {
      Box::new(IdentityTranslate)
    }

    fn transport(&self) -> TransportKind {
      TransportKind::RawLog
    }

    fn one_shot_text<'a>(
      &'a mut self,
      _prompt: &'a str,
      _system: &'a str,
      _timeout: Option<Duration>,
    ) -> BoxFuture<'a, anyhow::Result<String>> {
      Box::pin(async { anyhow::bail!("native one-shot is unused in this test") })
    }
  }

  struct IdentityTranslate;

  impl Translate for IdentityTranslate {
    fn translate(&mut self, event: Value) -> anyhow::Result<Vec<Value>> {
      Ok(vec![event])
    }
  }

  #[test]
  fn assistant_text_reads_text_blocks_only() {
    assert_eq!(
      assistant_text(
        &json!({"message":{"content":[{"type":"thinking","thinking":"hidden"},{"type":"text","text":"visible"}]}})
      ),
      "visible"
    );
    assert_eq!(frame_system_prompt("system", "user"), "<system-instructions>\nsystem\n</system-instructions>\n\nuser");
  }

  #[tokio::test]
  async fn agent_one_shot_text_collects_assistant_text_until_result() {
    let root = tempfile::tempdir().unwrap();
    let instructions_file = root.path().join("instructions.md");
    let script = concat!(
      "cat <<'EVENTS'\n",
      "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"first\"}]}}\n",
      "{\"type\":\"result\",\"usage\":{}}\n",
      "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"ignored\"}]}}\n",
      "EVENTS\n"
    )
    .to_string();
    let built = BuiltBackend {
      backend: Box::new(ScriptedBackend { script }),
      launch: LaunchArgs {
        instructions_content: Some("must not be written".to_string()),
        instructions_file: Some(instructions_file.clone()),
        ..LaunchArgs::default()
      },
      registry: Arc::new(Registry::new()),
    };

    let answer = agent_one_shot_text_with_environment(
      built,
      "prompt",
      "system",
      None,
      crate::agent_environment::AgentBaseEnvironment {
        values: Env::from([
          ("PATH".to_string(), "/synthetic/agent-entry-points:/synthetic/system-bin:/bin:/usr/bin".to_string()),
          ("PYDANTIC_DISABLE_PLUGINS".to_string(), "__all__".to_string()),
        ]),
      },
    )
    .await
    .unwrap();

    assert_eq!(answer, "first");
    assert!(!instructions_file.exists());
  }
}
