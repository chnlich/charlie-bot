//! Process-group shutdown follows the Python TERM-then-KILL grace period.

use std::future::Future;
use std::time::Duration;

use nix::sys::signal::{self, Signal};
use nix::unistd::{self, Pid};
use tokio::task::JoinHandle;

use super::spawn::AgentChild;

pub const KILL_ESCALATION_GRACE: Duration = Duration::from_secs(5);
pub const KILL_ESCALATION_POLL: Duration = Duration::from_millis(200);

/// Signal a process group and log failures without treating an exited group as an error.
pub fn kill_process_group(pid: u32, sig: Signal) -> bool {
  let process = Pid::from_raw(i32::try_from(pid).expect("process id fits pid_t"));
  let group = match unistd::getpgid(Some(process)) {
    Ok(group) => group,
    Err(nix::errno::Errno::ESRCH | nix::errno::Errno::EPERM) => {
      tracing::debug!(target: "charliebot_agents", pid, sig = ?sig, "kill_pg_gone");
      return false;
    }
    Err(error) => {
      tracing::debug!(target: "charliebot_agents", pid, sig = ?sig, error = %error, "kill_pg_failed");
      return false;
    }
  };
  match signal::killpg(group, sig) {
    Ok(()) => true,
    Err(nix::errno::Errno::ESRCH | nix::errno::Errno::EPERM) => {
      tracing::debug!(target: "charliebot_agents", pid, sig = ?sig, "kill_pg_gone");
      false
    }
    Err(error) => {
      tracing::debug!(target: "charliebot_agents", pid, sig = ?sig, error = %error, "kill_pg_failed");
      false
    }
  }
}

/// Send SIGTERM and escalate only when the caller's liveness proof still holds after five seconds.
pub async fn kill_group_escalating<F>(pid: u32, mut is_alive: F)
where
  F: FnMut() -> bool,
{
  kill_process_group(pid, Signal::SIGTERM);
  let deadline = tokio::time::Instant::now() + KILL_ESCALATION_GRACE;
  while is_alive() && tokio::time::Instant::now() < deadline {
    tokio::time::sleep(KILL_ESCALATION_POLL).await;
  }
  if is_alive() {
    kill_process_group(pid, Signal::SIGKILL);
  }
}

/// Await a future and cancel its stderr task on every exit; timeout kills the process group first.
pub async fn wait_or_kill_group<F, T, U>(
  future: F,
  timeout: Option<Duration>,
  pid: u32,
  stderr_task: JoinHandle<U>,
) -> Result<T, tokio::time::error::Elapsed>
where
  F: Future<Output = T>,
{
  let result = match timeout {
    Some(limit) => match tokio::time::timeout(limit, future).await {
      Ok(value) => Ok(value),
      Err(error) => {
        kill_process_group(pid, Signal::SIGKILL);
        Err(error)
      }
    },
    None => Ok(future.await),
  };
  stderr_task.abort();
  if let Err(error) = stderr_task.await
    && !error.is_cancelled()
  {
    tracing::warn!(target: "charliebot_agents", pid, error = %error, "backend_stderr_stream_cancel_failed");
  }
  result
}

/// Drop a child handle without signalling it.
pub fn detach(child: AgentChild) {
  drop(child);
}

/// Terminate a child process group and wait for exit before returning.
pub async fn terminate_child(child: &mut AgentChild) -> std::io::Result<()> {
  let Some(pid) = child.id() else {
    return wait_child(child).await;
  };
  let sent = kill_process_group(pid, Signal::SIGTERM);
  if !sent {
    return kill_child_and_reap(child).await;
  }
  match tokio::time::timeout(KILL_ESCALATION_GRACE, child.wait()).await {
    Ok(Ok(_)) => return Ok(()),
    Ok(Err(error)) if is_already_reaped(&error) => return Ok(()),
    Ok(Err(error)) => return Err(error),
    Err(_) => {
      tracing::warn!(target: "charliebot_agents", pid, "backend_terminate_timeout");
      kill_process_group(pid, Signal::SIGKILL);
      kill_child_and_reap(child).await?;
    }
  }
  Ok(())
}

async fn kill_child_and_reap(child: &mut AgentChild) -> std::io::Result<()> {
  match child.try_wait() {
    Ok(Some(_)) => return Ok(()),
    Ok(None) => {}
    Err(error) if is_already_reaped(&error) => return Ok(()),
    Err(error) => return Err(error),
  }
  if let Err(error) = child.start_kill() {
    match child.try_wait() {
      Ok(Some(_)) => return Ok(()),
      Err(wait_error) if is_already_reaped(&wait_error) => return Ok(()),
      Ok(None) => return Err(error),
      Err(wait_error) => return Err(wait_error),
    }
  }
  wait_child(child).await
}

async fn wait_child(child: &mut AgentChild) -> std::io::Result<()> {
  match child.wait().await {
    Ok(_) => Ok(()),
    Err(error) if is_already_reaped(&error) => Ok(()),
    Err(error) => Err(error),
  }
}

fn is_already_reaped(error: &std::io::Error) -> bool {
  error.raw_os_error() == Some(nix::errno::Errno::ECHILD as i32)
}

/// Convert an exit status to Python's integer convention, including negative signal numbers.
pub fn python_exit_code(status: std::process::ExitStatus) -> i32 {
  use std::os::unix::process::ExitStatusExt;

  status.code().unwrap_or_else(|| -status.signal().expect("an exit status has a code or signal"))
}

#[cfg(test)]
mod tests {
  use std::collections::HashMap;
  use std::ffi::OsString;

  use super::*;
  use crate::transport::cgroup::SessionCgroupConfig;
  use crate::transport::spawn::{ChildStdin, SpawnSetup, SpawnSpec, spawn_with_setup};

  async fn shell(args: &[&str]) -> AgentChild {
    let spec = SpawnSpec::new(
      "/bin/sh",
      args.iter().map(OsString::from).collect(),
      None,
      HashMap::new(),
      None,
      SessionCgroupConfig::disabled(),
    );
    spawn_with_setup(&spec, None, ChildStdin::Null, SpawnSetup::None).await.unwrap().child
  }

  #[tokio::test]
  async fn wait_without_a_limit_returns_and_cancels_stderr() {
    let stderr = tokio::spawn(async { tokio::time::sleep(Duration::from_secs(60)).await });
    let result = wait_or_kill_group(async { "done" }, None, 1, stderr).await.unwrap();
    assert_eq!(result, "done");
  }

  #[tokio::test]
  async fn terminate_stops_the_spawned_process_group() {
    let mut child = shell(&["-c", "exec /bin/sleep 30"]).await;
    terminate_child(&mut child).await.unwrap();
    assert!(child.try_wait().unwrap().is_some());
  }

  #[tokio::test]
  async fn terminate_reaps_a_child_that_exited_before_its_group_signal() {
    let mut child = shell(&["-c", "exit 0"]).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    terminate_child(&mut child).await.unwrap();
    match child.try_wait() {
      Ok(Some(_)) => {}
      Err(error) if is_already_reaped(&error) => {}
      status => panic!("the exited child was not reaped: {status:?}"),
    }
  }
}
