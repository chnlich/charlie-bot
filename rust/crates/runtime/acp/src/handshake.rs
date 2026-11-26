//! The ACP handshake of one agent process: `initialize`, then the session, then the model. Each request is written after
//! the previous response has appeared in the raw log, so the session id is known before the first prompt is written.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use charliebot_registry::midturn_delivery::ExitedProbe;
use serde_json::{Value, json};
use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::codec::{INITIALIZE_ID, MODEL_ID, SESSION_ID, SharedSession, line_bytes, read_output_before};

/// How often the handshake reads the raw log for its response.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Which session the process takes: the one a turn resumes, or a new one.
pub enum SessionStart {
  Load(String),
  New,
}

/// The handshake's parameters: the session, the working directory the session runs in, and the model it selects.
pub struct HandshakeSpec {
  pub start: SessionStart,
  pub cwd: PathBuf,
  pub model: String,
}

/// Runs the handshake on `stdin`, reading the responses from `raw_path`. The session id the agent answers with is set
/// into `session`. A refused request or a process that exits before its answer ends the run with an error.
pub async fn run(
  stdin: &mut (dyn AsyncWrite + Send + Unpin),
  raw_path: &Path,
  exited: &ExitedProbe,
  session: &SharedSession,
  spec: &HandshakeSpec,
) -> anyhow::Result<()> {
  let initialize = json!({
    "protocolVersion": 1,
    "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false},
  });
  write_request(stdin, INITIALIZE_ID, "initialize", initialize).await?;
  await_result(raw_path, exited, INITIALIZE_ID).await?;

  let cwd = spec.cwd.to_string_lossy().into_owned();
  let (method, params) = match &spec.start {
    SessionStart::Load(id) => ("session/load", json!({"sessionId": id, "cwd": cwd, "mcpServers": []})),
    SessionStart::New => ("session/new", json!({"cwd": cwd, "mcpServers": []})),
  };
  write_request(stdin, SESSION_ID, method, params).await?;
  let result = await_result(raw_path, exited, SESSION_ID).await?;
  let session_id = result
    .get("sessionId")
    .and_then(Value::as_str)
    .with_context(|| format!("ACP agent answered {method} without a sessionId"))?
    .to_string();
  *session.lock().expect("ACP session mutex is not poisoned") = Some(session_id.clone());

  let model = json!({"sessionId": session_id, "modelId": spec.model});
  write_request(stdin, MODEL_ID, "session/set_model", model).await?;
  await_result(raw_path, exited, MODEL_ID).await?;
  Ok(())
}

async fn write_request(
  stdin: &mut (dyn AsyncWrite + Send + Unpin),
  id: u64,
  method: &str,
  params: Value,
) -> anyhow::Result<()> {
  let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
  stdin.write_all(&line_bytes(&request)).await.with_context(|| format!("writing {method} to ACP agent"))?;
  stdin.flush().await.with_context(|| format!("flushing {method} to ACP agent"))?;
  Ok(())
}

/// Waits until the raw log holds the response to request `id`, and returns its result.
async fn await_result(raw_path: &Path, exited: &ExitedProbe, id: u64) -> anyhow::Result<Value> {
  loop {
    let response = read_output_before(raw_path, u64::MAX)?
      .into_iter()
      .find(|line| line.get("id").and_then(Value::as_u64) == Some(id) && line.get("method").is_none());
    if let Some(response) = response {
      if let Some(error) = response.get("error") {
        bail!("ACP agent refused request {id}: {error}");
      }
      return response
        .get("result")
        .cloned()
        .with_context(|| format!("ACP agent answered request {id} without a result"));
    }
    if exited() {
      bail!("ACP agent exited before it answered request {id}");
    }
    tokio::time::sleep(POLL_INTERVAL).await;
  }
}
