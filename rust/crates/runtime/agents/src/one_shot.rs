//! Tool-capable single-prompt agent execution.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use charliebot_core::event_types;
use futures_util::future::BoxFuture;
use serde_json::Value;
use tokio::time;

use crate::backend_types::BuiltBackend;
use crate::launch_loop::ChildEnvPolicy;
use crate::runner::{AgentRunner, EventFn, RunRequest};

/// The final answer of one run of a built backend with `base_env` in a temporary working directory.
pub async fn agent_one_shot_text(
  mut built: BuiltBackend,
  base_env: &charliebot_registry::backend::Env,
  prompt: &str,
  system_prompt: &str,
  timeout: Option<Duration>,
) -> anyhow::Result<String> {
  built.launch.instructions_file = None;
  built.launch.instructions_content = None;
  let cwd = charliebot_core::tempdir::mkdtemp("charliebot-oneshot-")?;
  let result = FinalResult::default();
  let runner = AgentRunner::new(&built.backend_type, built.backend, built.launch.clone());
  let mut request = RunRequest::new(
    frame_system_prompt(system_prompt, prompt),
    cwd.path().to_path_buf(),
    crate::agent_environment::AgentBaseEnvironment { values: base_env.clone() },
    crate::transport::cgroup::SessionCgroupConfig::disabled(),
    built.registry,
  );
  request.child_env_policy = ChildEnvPolicy::Inherited;
  request.on_event = result.on_event();
  match timeout {
    Some(limit) => match time::timeout(limit, runner.run(request)).await {
      Ok(run) => {
        run.context("running tool-capable one-shot backend")?;
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
  result.text()
}

/// The final answer of a run: the `result` text of its first result event. Each backend's translator writes its final
/// answer into that event, and Claude Code writes it there itself, so the assistant text before it is never joined.
/// Cloning shares the collector, so the event callback and the reader see the same result.
#[derive(Clone, Default)]
pub struct FinalResult {
  /// The first result event of the run, or `None` before one arrives.
  result_event: Arc<Mutex<Option<Value>>>,
}

impl FinalResult {
  /// The event callback a run feeds its translated events to.
  pub fn on_event(&self) -> EventFn {
    let result_event = Arc::clone(&self.result_event);
    Arc::new(move |event: Value| -> BoxFuture<'static, anyhow::Result<()>> {
      let result_event = Arc::clone(&result_event);
      Box::pin(async move {
        if event.get("type").and_then(Value::as_str) == Some(event_types::RESULT) {
          let mut slot = result_event.lock().expect("final result lock is not poisoned");
          slot.get_or_insert(event);
        }
        Ok(())
      })
    })
  }

  /// The final answer, trimmed. It is empty when the result event's text is empty, which each caller judges. Fails when
  /// the run emitted no result event, or when the result event carries no text field.
  pub fn text(&self) -> anyhow::Result<String> {
    let slot = self.result_event.lock().expect("final result lock is not poisoned");
    let Some(event) = slot.as_ref() else {
      bail!("the run ended without a final result event");
    };
    match event.get("result").and_then(Value::as_str) {
      Some(text) => Ok(text.trim().to_string()),
      None => bail!("the final result event carries no text"),
    }
  }
}

/// The backend's one-shot text: its CLI-native one-shot when the backend has one, else the tool-capable
/// [`agent_one_shot_text`].
pub async fn one_shot_text(
  mut built: BuiltBackend,
  base_env: &charliebot_registry::backend::Env,
  prompt: &str,
  system_prompt: &str,
  timeout: Option<Duration>,
) -> anyhow::Result<String> {
  if let Some(native) = built.backend.one_shot_text(base_env, prompt, system_prompt, timeout) {
    return native.await;
  }
  agent_one_shot_text(built, base_env, prompt, system_prompt, timeout).await
}

pub fn frame_system_prompt(system_prompt: &str, prompt: &str) -> String {
  format!("<system-instructions>\n{system_prompt}\n</system-instructions>\n\n{prompt}")
}

pub fn assistant_text(event: &Value) -> String {
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
pub(crate) mod tests {
  use super::*;
  use std::fs;
  use std::path::{Path, PathBuf};

  use charliebot_registry::Registry;
  use charliebot_registry::backend::{AgentBackend, Env, LaunchArgs, Translate, TransportKind};
  use serde_json::json;

  pub(crate) struct ScriptedBackend {
    pub(crate) script: String,
    pub(crate) native_answer: Option<String>,
    /// The transport directory the runner handed to `prepare_transport`.
    pub(crate) seen_log_dir: Arc<Mutex<Option<PathBuf>>>,
  }

  impl AgentBackend for ScriptedBackend {
    fn instructions_target(&self) -> Option<(&'static str, &'static str)> {
      None
    }

    fn prepare_transport(&mut self, log_dir: &Path) -> anyhow::Result<()> {
      *self.seen_log_dir.lock().expect("seen log dir lock is not poisoned") = Some(log_dir.to_path_buf());
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
      _base_env: &'a Env,
      _prompt: &'a str,
      _system: &'a str,
      _timeout: Option<Duration>,
    ) -> Option<BoxFuture<'a, anyhow::Result<String>>> {
      let answer = self.native_answer.clone()?;
      Some(Box::pin(async move { Ok(answer) }))
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

  /// The answer is the first result event's text, trimmed. Assistant text is never the answer. A run with no result
  /// event, or a result event with no text field, fails by name; an empty text is the answer's own.
  #[tokio::test]
  async fn final_result_is_the_first_result_text_and_names_a_run_without_one() {
    let final_result = FinalResult::default();
    let on_event = final_result.on_event();
    let no_result = final_result.text().unwrap_err().to_string();
    assert!(no_result.contains("without a final result event"), "{no_result}");

    on_event(json!({"type":"assistant","message":{"content":[{"type":"text","text":"intermediate"}]}})).await.unwrap();
    assert!(final_result.text().unwrap_err().to_string().contains("without a final result event"));

    on_event(json!({"type":"result","result":"  final answer \n"})).await.unwrap();
    on_event(json!({"type":"result","result":"second result"})).await.unwrap();
    assert_eq!(final_result.text().unwrap(), "final answer");

    let empty = FinalResult::default();
    empty.on_event()(json!({"type":"result","result":"   "})).await.unwrap();
    assert_eq!(empty.text().unwrap(), "");

    let textless = FinalResult::default();
    textless.on_event()(json!({"type":"result","usage":{}})).await.unwrap();
    let message = textless.text().unwrap_err().to_string();
    assert!(message.contains("carries no text"), "{message}");
  }

  #[tokio::test]
  async fn agent_one_shot_text_answers_with_the_result_text() {
    let root = tempfile::tempdir().unwrap();
    let instructions_file = root.path().join("instructions.md");
    let script = concat!(
      "cat <<'EVENTS'\n",
      "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"intermediate\"}]}}\n",
      "{\"type\":\"result\",\"result\":\"answer\",\"usage\":{}}\n",
      "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"ignored\"}]}}\n",
      "EVENTS\n"
    )
    .to_string();
    // The script records its working directory so the test can check the one-shot cwd removal.
    let record = root.path().join("cwd.txt");
    let script = format!("pwd > {};\n{}", record.display(), script);
    let seen_log_dir = Arc::new(Mutex::new(None));
    let built = BuiltBackend {
      backend: Box::new(ScriptedBackend { script, native_answer: None, seen_log_dir: Arc::clone(&seen_log_dir) }),
      launch: LaunchArgs {
        instructions_content: Some("must not be written".to_string()),
        instructions_file: Some(instructions_file.clone()),
        extra_flags: Vec::new(),
        buffer_limit: None,
        resume_session_id: None,
        log_dir: None,
        cgroup_session_id: None,
        remote_launch: None,
        backend_kwargs: serde_json::Map::new(),
        manager_turn: false,
      },
      backend_type: "scripted".to_string(),
      registry: Arc::new(Registry::new()),
    };

    let agent_path = "/synthetic/agent-entry-points:/synthetic/system-bin:/bin:/usr/bin";
    let mut base_env = crate::gh_wrapper::stub_environment(root.path(), agent_path);
    base_env.insert("PYDANTIC_DISABLE_PLUGINS".to_string(), "__all__".to_string());
    let answer = agent_one_shot_text(built, &base_env, "prompt", "system", None).await.unwrap();

    assert_eq!(answer, "answer");
    assert!(!instructions_file.exists());

    // The run's throwaway transport dir (`charliebot-run-*`) and the
    // one-shot cwd (`charliebot-oneshot-*`) are both gone once the call returns.
    let log_dir = seen_log_dir
      .lock()
      .expect("seen log dir lock is not poisoned")
      .clone()
      .expect("prepare_transport did not observe the run's log dir");
    assert!(
      log_dir.file_name().and_then(|name| name.to_str()).unwrap_or("").starts_with("charliebot-run-"),
      "unexpected run log dir name: {}",
      log_dir.display()
    );
    assert!(!log_dir.exists(), "run log dir survived the run: {}", log_dir.display());
    let cwd = fs::read_to_string(&record).unwrap();
    let cwd = cwd.trim();
    assert!(cwd.rsplit('/').next().unwrap().starts_with("charliebot-oneshot-"), "unexpected one-shot cwd: {cwd}");
    assert!(!Path::new(cwd).exists(), "one-shot cwd survived the call: {cwd}");
  }

  #[tokio::test]
  async fn dispatcher_takes_the_native_one_shot_and_falls_back_to_the_tool_capable_one() {
    let root = tempfile::tempdir().unwrap();
    let base_env = crate::gh_wrapper::stub_environment(root.path(), "/bin:/usr/bin");
    let native = BuiltBackend {
      backend: Box::new(ScriptedBackend {
        script: "exit 1".to_string(),
        native_answer: Some("native".to_string()),
        seen_log_dir: Arc::new(Mutex::new(None)),
      }),
      launch: LaunchArgs::default(),
      backend_type: "scripted".to_string(),
      registry: Arc::new(Registry::new()),
    };
    assert_eq!(one_shot_text(native, &base_env, "prompt", "system", None).await.unwrap(), "native");

    let script = "printf '%s\\n' '{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"tool-capable\"}]}}' '{\"type\":\"result\",\"result\":\"tool-capable\"}'";
    let tool_capable = BuiltBackend {
      backend: Box::new(ScriptedBackend {
        script: script.to_string(),
        native_answer: None,
        seen_log_dir: Arc::new(Mutex::new(None)),
      }),
      launch: LaunchArgs::default(),
      backend_type: "scripted".to_string(),
      registry: Arc::new(Registry::new()),
    };
    assert_eq!(one_shot_text(tool_capable, &base_env, "prompt", "system", None).await.unwrap(), "tool-capable");
  }
}
