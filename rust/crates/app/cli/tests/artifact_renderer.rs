use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use charliebot_shell_harness as harness;
use futures_util::{SinkExt, StreamExt};
use nix::errno::Errno;
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use serde_json::{Value, json};
use tokio_tungstenite::{accept_async, tungstenite::Message};
use url::Url;
use wait_timeout::ChildExt;

const PROCESS_TIMEOUT: Duration = Duration::from_secs(20);
const CDP_ACCEPT_TIMEOUT: Duration = Duration::from_secs(10);

#[test]
fn artifact_check_closes_the_fake_chrome_process_group_before_exit() {
  let page = fs::read_to_string(harness::repo_root().join("prompts/plan_template.html")).unwrap();
  let run = run_check(&page, false);

  assert_eq!(run.code, 0, "stderr: {}", run.stderr);
  assert_eq!(run.measurements, 1);
  assert!(!run.browser_group_has_members, "artifact check left fake Chrome process group running");
}

#[test]
fn artifact_check_closes_the_fake_chrome_process_group_after_assertion_failures() {
  let run = run_check("<html><body></body></html>", false);

  assert_eq!(run.code, 1, "stdout: {}\nstderr: {}", run.stdout, run.stderr);
  assert_eq!(run.measurements, 1);
  assert!(!run.browser_group_has_members, "artifact check left fake Chrome process group running");
}

#[test]
fn artifact_check_closes_the_fake_chrome_process_group_after_assertion_errors() {
  let page = fs::read_to_string(harness::repo_root().join("prompts/plan_template.html")).unwrap();
  let run = run_check(&page, true);
  let error: Value = serde_json::from_str(&run.stderr).unwrap();

  assert_eq!(run.code, 1);
  assert!(error["error"].as_str().is_some());
  assert_eq!(run.measurements, 1);
  assert!(!run.browser_group_has_members, "artifact check left fake Chrome process group running");
}

struct CheckRun {
  code: i32,
  stdout: String,
  stderr: String,
  measurements: usize,
  browser_group_has_members: bool,
}

fn run_check(page_source: &str, remove_probe_on_measurement: bool) -> CheckRun {
  let temporary = tempfile::tempdir().unwrap();
  let home = temporary.path().join("home");
  fs::create_dir(&home).unwrap();
  let fake_chrome = temporary.path().join("fake-chrome");
  let browser_pid_path = temporary.path().join("browser.pid");
  let profile_path_path = temporary.path().join("profile.path");
  let page = temporary.path().join("page.html");
  fs::write(&page, page_source).unwrap();
  let listener = TcpListener::bind("127.0.0.1:0").unwrap();
  listener.set_nonblocking(true).unwrap();
  let port = listener.local_addr().unwrap().port();
  let (server, measurements) = start_cdp_server(listener, remove_probe_on_measurement);
  write_fake_chrome(&fake_chrome, &browser_pid_path, &profile_path_path, port);
  let config = format!("headless_chrome_bin: {}\n", serde_json::to_string(&fake_chrome.display().to_string()).unwrap());
  fs::write(home.join("config.yaml"), config).unwrap();

  let stdout_path = temporary.path().join("stdout.txt");
  let stderr_path = temporary.path().join("stderr.txt");
  let mut child = Command::new(env!("CARGO_BIN_EXE_charliebot"))
    .args(["artifact", "check"])
    .arg(&page)
    .args(["--genre", "plan", "--assertions-only"])
    .current_dir(temporary.path())
    .env_clear()
    .env("CHARLIEBOT_HOME", &home)
    .env("CHARLIEBOT_REPO_ROOT", harness::repo_root())
    .env("HOME", temporary.path().join("user-home"))
    .stdin(Stdio::null())
    .stdout(Stdio::from(fs::File::create(&stdout_path).unwrap()))
    .stderr(Stdio::from(fs::File::create(&stderr_path).unwrap()))
    .spawn()
    .unwrap();

  let status = match child.wait_timeout(PROCESS_TIMEOUT).unwrap() {
    Some(status) => status,
    None => {
      child.kill().unwrap();
      child.wait().unwrap();
      if let Ok(pid) = read_browser_pid(&browser_pid_path) {
        stop_process_group(pid);
      }
      panic!("artifact check did not finish within {PROCESS_TIMEOUT:?}");
    }
  };
  server.join().unwrap();

  let browser_pid = read_browser_pid(&browser_pid_path).expect("fake Chrome records its process id");
  let group_has_members = process_group_has_members(browser_pid);
  stop_process_group(browser_pid);
  let profile_path = fs::read_to_string(profile_path_path).unwrap();
  let profile_path = Path::new(profile_path.trim());
  if profile_path.exists() {
    fs::remove_dir_all(profile_path).unwrap();
  }
  CheckRun {
    code: status.code().expect("the CLI exits by code"),
    stdout: fs::read_to_string(stdout_path).unwrap(),
    stderr: fs::read_to_string(stderr_path).unwrap(),
    measurements: measurements.load(Ordering::SeqCst),
    browser_group_has_members: group_has_members,
  }
}

fn write_fake_chrome(path: &Path, pid_path: &Path, profile_path_path: &Path, port: u16) {
  let script = format!(
    r#"#!/bin/sh
profile=
for arg in "$@"; do
  case "$arg" in
    --user-data-dir=*) profile=${{arg#*=}} ;;
  esac
done
printf '%s\n' "$$" > {pid_path}
printf '%s\n' "$profile" > {profile_path_path}
printf '%s\n%s\n' '{port}' '/devtools/browser/stub' > "$profile/DevToolsActivePort"
exec /bin/sleep 300
"#,
    pid_path = shell_quote(&pid_path.display().to_string()),
    profile_path_path = shell_quote(&profile_path_path.display().to_string()),
  );
  fs::write(path, script).unwrap();
  let mut permissions = fs::metadata(path).unwrap().permissions();
  permissions.set_mode(0o755);
  fs::set_permissions(path, permissions).unwrap();
}

fn shell_quote(value: &str) -> String {
  format!("'{}'", value.replace('\'', "'\\''"))
}

fn read_browser_pid(path: &Path) -> Result<i32, String> {
  fs::read_to_string(path)
    .map_err(|error| format!("read fake Chrome pid {}: {error}", path.display()))?
    .trim()
    .parse::<i32>()
    .map_err(|error| format!("parse fake Chrome pid {}: {error}", path.display()))
}

fn process_group_has_members(process_group: i32) -> bool {
  match signal::killpg(Pid::from_raw(process_group), None) {
    Ok(()) => true,
    Err(Errno::ESRCH) => false,
    Err(error) => panic!("check fake Chrome process group {process_group}: {error}"),
  }
}

fn stop_process_group(process_group: i32) {
  match signal::killpg(Pid::from_raw(process_group), Some(Signal::SIGKILL)) {
    Ok(()) | Err(Errno::ESRCH) => {}
    Err(error) => panic!("kill fake Chrome process group {process_group}: {error}"),
  }
  let deadline = Instant::now() + Duration::from_secs(5);
  while process_group_has_members(process_group) && Instant::now() < deadline {
    thread::sleep(Duration::from_millis(10));
  }
}

fn start_cdp_server(listener: TcpListener, remove_probe_on_measurement: bool) -> (JoinHandle<()>, Arc<AtomicUsize>) {
  let measurements = Arc::new(AtomicUsize::new(0));
  let server_measurements = Arc::clone(&measurements);
  let handle = thread::spawn(move || {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    runtime.block_on(async move {
      let listener = tokio::net::TcpListener::from_std(listener).unwrap();
      let (stream, _) = tokio::time::timeout(CDP_ACCEPT_TIMEOUT, listener.accept()).await.unwrap().unwrap();
      let mut socket = accept_async(stream).await.unwrap();
      let mut probe_url = None;
      loop {
        let message = socket.next().await.unwrap().unwrap();
        let Message::Text(text) = message else { continue };
        let request: Value = serde_json::from_str(text.as_ref()).unwrap();
        let method = request["method"].as_str().unwrap();
        let response = match method {
          "Target.createTarget" => json!({"targetId": "target"}),
          "Target.attachToTarget" => json!({"sessionId": "session"}),
          "Runtime.evaluate" => json!({"result": {"type": "string", "value": "2600"}}),
          _ => json!({}),
        };
        let mut reply = json!({"id": request["id"], "result": response});
        if let Some(session) = request.get("sessionId") {
          reply["sessionId"] = session.clone();
        }
        socket.send(Message::Text(reply.to_string().into())).await.unwrap();
        if method == "Page.navigate" {
          probe_url = Some(request["params"]["url"].as_str().unwrap().to_owned());
          let event = json!({"method": "Page.loadEventFired", "sessionId": "session"});
          socket.send(Message::Text(event.to_string().into())).await.unwrap();
        }
        if method == "Runtime.evaluate" {
          if remove_probe_on_measurement {
            let probe = Url::parse(probe_url.as_deref().unwrap()).unwrap().to_file_path().unwrap();
            fs::remove_file(probe).unwrap();
          }
          server_measurements.fetch_add(1, Ordering::SeqCst);
          return;
        }
      }
    });
  });
  (handle, measurements)
}
