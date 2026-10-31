//! The codex command, child environment, and CLI-native one-shot text (`src/backends/codex/codex.py:1-180`).

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use charliebot_core::event_types;
use charliebot_core::str_methods;
use charliebot_core::text_repr::value_str;
use charliebot_registry::backend::{
  AGENTS_INSTRUCTIONS_FILE_NAME, AgentBackend, Env, LaunchArgs, Translate, TransportKind,
};
use futures_util::future::BoxFuture;
use nix::sys::signal::Signal;
use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

use super::options::CodexOptions;
use super::text::truthy;
use super::translate::CodexTranslator;
use charliebot_agents::cli_base::prepend_path_dir;
use charliebot_agents::one_shot::{assistant_text, frame_system_prompt};
use charliebot_agents::transport::cgroup::SessionCgroupConfig;
use charliebot_agents::transport::kill::{exit_code, kill_process_group};
use charliebot_agents::transport::spawn::{AgentChild, AgentPipeReader, SpawnSpec, SpawnedChild, spawn_one_shot};

/// The log event that reports writing the instructions file.
pub const INSTRUCTIONS_WRITTEN_EVENT: &str = "codex_wrote_agents_md";

/// The stderr a one-shot keeps for its error message; later bytes are read and dropped.
const MAX_ONE_SHOT_STDERR_BYTES: usize = 4 * 1024;

/// The `codex exec --json` backend of one launch.
pub struct CodexBackend {
  model: String,
  codex_bin: String,
  model_reasoning_effort: String,
  model_auto_compact_token_limit: Option<i64>,
  user_local_bin: String,
  cgroup_session_id: Option<String>,
  cgroup_config: SessionCgroupConfig,
  extra_flags: Vec<String>,
  instructions_content: Option<String>,
  instructions_file: Option<PathBuf>,
  resume_session_id: Option<String>,
}

impl CodexBackend {
  /// The backend of one launch. `codex_bin` is the resolved binary, and `user_local_bin` the directory the child PATH
  /// gains when it lacks it.
  pub fn from_launch(
    codex_bin: PathBuf,
    user_local_bin: &Path,
    model: &str,
    options: &CodexOptions,
    launch: LaunchArgs,
    cgroup_config: SessionCgroupConfig,
  ) -> anyhow::Result<Self> {
    let codex_bin = codex_bin
      .into_os_string()
      .into_string()
      .map_err(|path| anyhow::anyhow!("the codex binary path is not valid Unicode: {}", Path::new(&path).display()))?;
    Ok(Self {
      model: model.to_string(),
      codex_bin,
      model_reasoning_effort: options.model_reasoning_effort.clone().unwrap_or_else(|| "xhigh".to_string()),
      model_auto_compact_token_limit: options.model_auto_compact_token_limit,
      user_local_bin: user_local_bin.to_string_lossy().into_owned(),
      cgroup_session_id: launch.cgroup_session_id,
      cgroup_config,
      extra_flags: launch.extra_flags,
      instructions_content: launch.instructions_content,
      instructions_file: launch.instructions_file,
      resume_session_id: launch.resume_session_id.filter(|id| !id.is_empty()),
    })
  }

  /// The `--config` pair that carries a worker or review run's instructions; none on a manager turn.
  ///
  /// `developer_instructions` takes the instructions text. `project_doc_fallback_filenames` lets Codex read the
  /// repository's CLAUDE.md when its worktree has no AGENTS.md.
  fn instructions_config_args(&self) -> anyhow::Result<Vec<String>> {
    if self.instructions_file.is_none() {
      return Ok(Vec::new());
    }
    let content = self.instructions_content.as_deref().map(toml_string_value).unwrap_or_else(|| "null".to_string());
    Ok(vec![
      "--config".to_string(),
      format!("developer_instructions={content}"),
      "--config".to_string(),
      r#"project_doc_fallback_filenames=["CLAUDE.md"]"#.to_string(),
    ])
  }

  fn model_config_args(&self) -> Vec<String> {
    let mut args = vec!["--config".to_string(), format!("model_reasoning_effort=\"{}\"", self.model_reasoning_effort)];
    if let Some(limit) = self.model_auto_compact_token_limit {
      args.push("--config".to_string());
      args.push(format!("model_auto_compact_token_limit={limit}"));
    }
    args
  }

  /// The argv shared by streaming, resume and one-shot `codex exec` runs.
  fn exec_args(&self) -> Vec<String> {
    let mut args: Vec<String> = [
      "--json",
      "--skip-git-repo-check",
      "--dangerously-bypass-approvals-and-sandbox",
      "--disable",
      "multi_agent",
      "--model",
    ]
    .iter()
    .map(|flag| flag.to_string())
    .collect();
    args.push(self.model.clone());
    args.extend(self.model_config_args());
    args
  }

  /// The `codex exec` head of the streaming and one-shot paths.
  fn exec_command_head(&self) -> Vec<String> {
    let mut command = vec![self.codex_bin.clone(), "exec".to_string()];
    command.extend(self.exec_args());
    command
  }

  /// The streaming command. A resume splices `resume` after `exec` and the session id after the exec flags, which is
  /// the form `codex exec resume <flags> <session-id>` takes.
  pub fn build_command(&self, prompt: &str) -> anyhow::Result<Vec<String>> {
    let mut command = self.exec_command_head();
    command.extend(self.instructions_config_args()?);
    if let Some(session_id) = &self.resume_session_id {
      command.insert(2, "resume".to_string());
      command.push(session_id.clone());
    }
    command.extend(self.extra_flags.iter().cloned());
    command.push("--".to_string());
    command.push(prompt.to_string());
    Ok(command)
  }

  fn prepare_env(&self, mut env: Env) -> Env {
    prepend_path_dir(&mut env, &self.user_local_bin);
    env
  }

  /// `codex exec --json` with the configured model and effort, for one prompt and no tools.
  ///
  /// Codex has no system-prompt flag, so the system prompt is framed into the prompt. The text is the assistant
  /// messages of the agent_message items. A structured error event, a nonzero exit or an empty answer fails the call.
  async fn run_one_shot(
    &self,
    base_env: &Env,
    prompt: &str,
    system: &str,
    timeout: Option<Duration>,
  ) -> anyhow::Result<String> {
    let mut command = self.exec_command_head();
    command.push("--".to_string());
    command.push(frame_system_prompt(system, prompt));
    let env = self.prepare_env(base_env.clone());
    let spec = SpawnSpec::new(
      command[0].clone(),
      command[1..].iter().map(OsString::from).collect(),
      None,
      env.into_iter().map(|(name, value)| (OsString::from(name), OsString::from(value))).collect::<HashMap<_, _>>(),
      self.cgroup_session_id.clone(),
      self.cgroup_config.clone(),
    );
    let SpawnedChild { mut child, cgroup: _cgroup } =
      spawn_one_shot(&spec, None).await.context("spawning the codex one-shot")?;
    let pid = child.id().context("codex one-shot did not expose its process id")?;
    let stdout = child.stdout.take().context("codex one-shot stdout was not piped")?;
    let stderr = child.stderr.take().context("codex one-shot stderr was not piped")?;
    let stderr_task = tokio::spawn(read_bounded_stderr(stderr));
    let mut translator = CodexTranslator::new(&self.model);

    let collected = match timeout {
      Some(limit) => match tokio::time::timeout(limit, collect(&mut child, stdout, &mut translator)).await {
        Ok(result) => result,
        Err(elapsed) => {
          kill_process_group(pid, Signal::SIGKILL);
          let wait_result = child.wait().await.context("reaping the timed-out codex one-shot");
          stderr_task.abort();
          let stderr_result = match stderr_task.await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(error)) => Err(error).context("reading the timed-out codex one-shot stderr"),
            Err(error) if error.is_cancelled() => Ok(()),
            Err(error) => Err(error).context("waiting for the timed-out codex one-shot stderr reader"),
          };
          wait_result?;
          stderr_result?;
          return Err(elapsed.into());
        }
      },
      None => collect(&mut child, stdout, &mut translator).await,
    };
    let (text, structured_error, returncode) = collected?;
    let stderr = stderr_task.await.context("waiting for the codex one-shot stderr reader")??;
    let stderr_text = str_methods::strip(&String::from_utf8_lossy(&stderr)).to_string();
    let detail = if stderr_text.is_empty() { String::new() } else { format!(": {stderr_text}") };
    if let Some(error) = structured_error.filter(|error| !error.is_empty()) {
      bail!("Codex one-shot failed: {error}");
    }
    if returncode != 0 {
      bail!("Codex one-shot failed (exit {returncode}){detail}");
    }
    if text.is_empty() {
      bail!("Codex one-shot returned no assistant text{detail}");
    }
    Ok(text)
  }
}

impl AgentBackend for CodexBackend {
  fn instructions_target(&self) -> Option<(&'static str, &'static str)> {
    Some((AGENTS_INSTRUCTIONS_FILE_NAME, INSTRUCTIONS_WRITTEN_EVENT))
  }

  fn prepare_transport(&mut self, _log_dir: &Path) -> anyhow::Result<()> {
    Ok(())
  }

  fn build_command(&self, prompt: &str) -> anyhow::Result<Vec<String>> {
    CodexBackend::build_command(self, prompt)
  }

  fn prepare_env(&self, env: Env) -> anyhow::Result<Env> {
    Ok(CodexBackend::prepare_env(self, env))
  }

  fn stdin_prompt(&self, _prompt: &str) -> Option<String> {
    None
  }

  fn translator(&self) -> Box<dyn Translate> {
    Box::new(CodexTranslator::new(&self.model))
  }

  fn transport(&self) -> TransportKind {
    TransportKind::RawLog
  }

  fn one_shot_text<'a>(
    &'a mut self,
    base_env: &'a Env,
    prompt: &'a str,
    system: &'a str,
    timeout: Option<Duration>,
  ) -> Option<BoxFuture<'a, anyhow::Result<String>>> {
    Some(Box::pin(async move { self.run_one_shot(base_env, prompt, system, timeout).await }))
  }
}

/// The codex value of a string: `toml`'s serialization of the text as a TOML value. Codex reads the `-c` value as TOML,
/// so the text it reads is the decoded string, whatever quoting the serializer chose.
pub fn toml_string_value(text: &str) -> String {
  let mut value = String::new();
  text.serialize(toml::ser::ValueSerializer::new(&mut value)).expect("a string serializes as a TOML value");
  value
}

/// Reads the one-shot's stdout to its end. Returns the assistant text, the first structured error and the exit code.
async fn collect(
  child: &mut AgentChild,
  stdout: AgentPipeReader,
  translator: &mut CodexTranslator,
) -> anyhow::Result<(String, Option<String>, i32)> {
  let mut parts = Vec::new();
  let mut structured_error: Option<String> = None;
  let mut lines = BufReader::new(stdout).lines();
  while let Some(line) = lines.next_line().await? {
    let Some(event) = parse_line(&line) else {
      continue;
    };
    for translated in translator.translate_event(&event)? {
      match translated.get("type").and_then(Value::as_str) {
        Some(event_types::ERROR) => {
          let message = translated.get("message").filter(|value| truthy(value)).or(translated.get("content"));
          if let Some(message) = message.filter(|value| truthy(value))
            && structured_error.is_none()
          {
            structured_error = Some(str_methods::strip(&value_str(message)).to_string());
          }
        }
        Some(event_types::ASSISTANT)
          if event.get("item").and_then(|item| item.get("type")).and_then(Value::as_str) == Some("agent_message") =>
        {
          parts.push(assistant_text(&translated));
        }
        _ => {}
      }
    }
  }
  let status = child.wait().await?;
  Ok((str_methods::strip(&parts.concat()).to_string(), structured_error, exit_code(status)))
}

/// One NDJSON line parsed, or `None` for a blank line or a line that is not JSON (logged, as the Python reader does).
fn parse_line(line: &str) -> Option<Value> {
  if line.trim().is_empty() {
    return None;
  }
  match serde_json::from_str::<Value>(line) {
    Ok(event) => Some(event),
    Err(error) => {
      tracing::debug!(target: "charliebot_agents", error = %error, "backend_line_not_json");
      None
    }
  }
}

/// Reads the whole stderr so the child never blocks on a full pipe, and keeps its first bytes.
async fn read_bounded_stderr(mut stderr: AgentPipeReader) -> anyhow::Result<Vec<u8>> {
  let mut captured = Vec::new();
  let mut chunk = [0u8; 8192];
  loop {
    let read = stderr.read(&mut chunk).await?;
    if read == 0 {
      return Ok(captured);
    }
    let room = MAX_ONE_SHOT_STDERR_BYTES - captured.len().min(MAX_ONE_SHOT_STDERR_BYTES);
    captured.extend_from_slice(&chunk[..room.min(read)]);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn toml_string_value_decodes_to_the_same_text() {
    for text in [
      "plain",
      "a\"b\n\u{7f}é",
      "tab\tcr\rff\u{c}bs\u{8}ctrl\u{1}",
      "null\0",
      "back\\slash",
      "\nleading\n\nend",
      "'''\"\"\"",
    ] {
      let decoded: toml::Table =
        toml::from_str(&format!("developer_instructions = {}", toml_string_value(text))).unwrap();
      assert_eq!(decoded["developer_instructions"].as_str(), Some(text));
    }
  }

  #[test]
  fn child_path_gains_the_user_bin_once() {
    let mut env = Env::from([("PATH".to_string(), "/usr/bin:/home/a/.local/bin".to_string())]);
    prepend_path_dir(&mut env, "/home/a/.local/bin");
    assert_eq!(env["PATH"], "/usr/bin:/home/a/.local/bin");
    prepend_path_dir(&mut env, "/home/a/bin");
    assert_eq!(env["PATH"], "/home/a/bin:/usr/bin:/home/a/.local/bin");
  }

  #[test]
  fn instructions_file_without_content_serializes_none_as_toml_null() {
    let launch =
      LaunchArgs { instructions_file: Some(PathBuf::from("/synthetic/instructions.md")), ..LaunchArgs::default() };
    let backend = CodexBackend::from_launch(
      PathBuf::from("/synthetic/bin/codex"),
      Path::new("/synthetic/home/.local/bin"),
      "gpt-5.5",
      &CodexOptions::default(),
      launch,
      SessionCgroupConfig::disabled(),
    )
    .unwrap();
    assert!(backend.build_command("prompt").unwrap().contains(&"developer_instructions=null".to_string()));
  }
}
