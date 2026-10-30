//! The golden-case harness: replay the recorded Python CLI behavior against the built Rust binary.
//!
//! Every case under `tests/golden/<subcommand>/<case>.json` runs once per test binary invocation and
//! replays against the built `charliebot` binary; every `expect` field is checked. `README.md`
//! documents the replay command.
//!
//! Each CLI process gets a cleared environment, a temp `CHARLIEBOT_HOME`, and a 20 s timeout; the
//! stub HTTP server binds `127.0.0.1` port 0, so no case can reach the live server or home.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::{Value, json};

/// How long one CLI process may run before the test kills it and fails the case.
const PROCESS_TIMEOUT: Duration = Duration::from_secs(20);

/// The checkout root, for `{repo}` and `{file:<checkout-relative path>}` in a case: the crate sits at
/// `rust/crates/app/cli`.
const REPO_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../..");

const CASES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden");

#[test]
fn golden_cases() {
  let mut failures = Vec::new();
  let mut cases = 0;
  for path in case_files() {
    cases += 1;
    if let Err(message) = run_case(&path) {
      failures.push(format!("{}: {message}", path.display()));
    }
  }
  println!("replayed {cases} golden case(s)");
  assert!(failures.is_empty(), "{} golden case(s) failed:\n{}", failures.len(), failures.join("\n"));
}

/// Python test names backed by golden cases, each pinned by a case file whose name starts with it.
/// `test_connect_never_established_retries_with_backoff_then_exhausts` and
/// `test_post_sends_json_body_content_type_and_auth_header_over_the_real_client` are Rust tests in
/// `charliebot-cli-common`, and `test_port_cache_hit_serves_the_cached_port_in_a_fresh_process` has
/// no Rust counterpart — no port cache, so `changed_server_port_takes_effect_on_the_next_call`
/// replaces it.
const PORTED_PYTHON_TESTS: &[&str] = &[
  "test_main_routes_by_session_env_from_another_session_dir",
  "test_main_rejects_explicit_session_against_session_env",
  "test_main_prints_only_session_and_run_id_from_server_result",
  "test_main_prints_readback_ids_when_the_response_is_lost",
  "test_main_posts_task_spec_file_to_delegate_endpoint",
  "test_main_verify_posts_repoless_payload",
  "test_main_repo_less_delegation_posts_no_repo_fields",
  "test_main_uses_error_detail_from_response",
  "test_session_id_reaches_payload_from_cwd_or_flag",
  "test_two_same_spec_siblings_readback_binds_to_request_identity",
  "test_readback_returns_none_without_the_bound_child",
  "test_readback_run_id_is_the_operation_work_run",
  "test_resolve_session_id_sources_without_env",
  "test_resolve_session_id_rejects_mismatches_without_env",
  "test_resolve_session_id_env_outranks_cwd",
  "test_post_internal_api_bearer_header",
  "test_print_json_keeps_non_ascii_characters",
  "test_print_json_writes_utf8_under_c_locale",
  "test_session_help_lists_archive_without_complete",
  "test_session_archive_rejects_an_explicit_empty_result_file",
  "test_session_archive_readback_resolves_to_own_archived_fact_on_sent_but_lost",
  "test_session_cancel_readback_resolves_to_own_task_closed_fact_on_sent_but_lost",
  "test_cli_session_send_relays_message",
  "test_cli_session_send_maps_a_422_refusal_to_exit_2",
  "test_dialog_formats_blocks_and_skips_empty_messages",
  "test_dialog_joins_pages_ascending",
  "test_dialog_ordinary_session_prints_history_without_events_call",
  "test_cancel_with_unobserved_run_prints_pid_and_exits_nonzero",
  "test_dialog_archived_session_starts_from_bootstrap_cursor",
  "test_dialog_turns_prints_last_complete_turns_and_the_turn_in_progress",
  "test_dialog_turns_prints_whole_history_when_it_holds_fewer_turns",
  "test_dialog_turns_rejects_values_below_one",
  "test_dialog_turns_archived_session_header_counts_events",
  "test_plan_present_posts_to_present_endpoint",
  "test_plan_registration_prints_its_verbs_reminder",
  "test_plan_approve_posts_plan_id",
  "test_plan_close_posts",
  "test_plan_server_rejection_exits_nonzero_with_detail_on_stderr",
  "test_plan_session_auto_derived_from_cwd",
  "test_plan_diff_prints_five_keys_computed_locally",
  "test_main_posts_to_improve_endpoint",
  "test_main_posts_plan_file_when_provided",
  "test_main_exits_on_request_error",
  "test_improve_stop_cli_posts_to_stop_endpoint_and_prints_stopped",
  "test_improve_stop_cli_exits_1_without_a_running_loop",
  "test_cli_unknown_genre_is_usage_error",
  "test_background_flag_is_a_usage_error",
  "test_cli_assertions_only_fails_when_the_height_cannot_be_measured",
  "test_cli_probe_runs_after_assertions_pass_and_prints_backend_and_answers",
  "test_cli_in_a_manager_session_delegates_the_cold_read",
  "test_cli_in_a_manager_session_exits_1_when_the_server_refuses",
  "test_cli_in_a_worker_session_runs_the_cold_read_in_the_foreground",
  "test_cli_with_no_session_identity_runs_the_cold_read_in_the_foreground",
  "test_cli_foreground_prints_every_model_failed_and_exits_1",
  "test_missing_artifact_exits_non_zero_naming_the_path",
  "test_arguments_after_double_dash_reach_the_program_unchanged",
  "test_command_lane_failure_exits_one_with_the_programs_stderr_and_no_url",
  "test_home_without_a_publish_section_exits_one_naming_publish_dir",
  "test_missing_artifact_raises_naming_the_path",
  "test_publish_without_index_html_refuses_and_writes_nothing",
  "test_directory_lane_preflight_messages_stay_word_for_word",
  "test_command_lane_returns_the_url_the_program_prints",
  "test_command_lane_runs_the_command_on_the_absolute_artifact_path_then_the_lane_options",
  "test_command_lane_nonzero_exit_raises_with_the_exit_code_and_stderr",
  "test_command_lane_refuses_stdout_that_is_not_exactly_one_url",
  "test_command_lane_names_a_program_that_cannot_start",
  "test_command_lane_checks_the_artifact_before_running_the_program",
  "test_lane_options_on_the_directory_lane_are_refused_before_any_write",
  "test_usage_errors",
  // The compaction hook and the trash purge run through golden cases too; the Python tests that
  // call private functions instead are Rust tests in the command modules:
  // `test_reads_bash_read_skill_and_split_path_inputs_in_latest_read_order`,
  // `test_parts_obey_character_limit_and_split_only_at_line_ends`,
  // `test_overflow_skills_are_listed_and_not_partially_attached`,
  // `test_pattern_location_is_no_skill_read` and
  // `test_plain_skill_name_with_dot_underscore_or_digit_is_read` in
  // `commands/compaction_context.rs`, `test_gc_trash_yes_hard_deletes` in `commands/gc_trash.rs`.
  "test_empty_slot_prints_nothing",
  "test_missing_session_log_exits_nonzero_with_stderr_reason",
  "test_deleted_skill_read_is_unresolved_while_other_skills_print",
  "test_gc_trash_dry_run_lists_but_deletes_nothing",
  "test_gc_trash_yes_hard_deletes",
  "test_gc_trash_empty_reports_and_returns",
  "test_cli_add_creates_one_staging_file",
  "test_cli_add_on_a_fresh_home_creates_the_scaffold_and_the_capture",
  "test_cli_query_index_on_a_fresh_home_runs",
  "test_cli_query_audience_filter_is_membership",
  "test_cli_lint_dir_reads_given_root",
  "test_cli_lint_warns_on_an_overwide_body_line_without_failing",
  "test_cli_query_dir_reads_given_root",
  "test_active_run_token_fixes_the_audience",
  "test_unlaunched_run_token_refuses",
  "test_ended_run_token_refuses",
  "test_the_cli_posts_the_reply_text_and_prints_the_readback_as_one_json_line",
  "test_the_cli_exits_non_zero_with_a_json_error_on_a_refusal",
  "test_the_cli_sends_the_keep_language_reason_with_the_reply",
  "test_the_cli_dialog_prints_the_for_you_layer_in_log_order",
  "test_the_cli_status_set_posts_the_file_and_prints_the_stored_status_as_one_json_line",
  "test_the_cli_status_set_does_not_start_a_cold_read_after_a_link_rejection",
  "test_the_cli_status_set_stops_on_missing_context_before_calling_a_light_backend",
  "test_the_cli_status_set_reads_and_records_each_unread_item_then_exits_three_before_storing",
  "test_the_cli_status_set_prints_each_backend_error_and_records_no_failed_item",
  "test_the_cli_status_confirm_posts_the_session_and_prints_the_stored_status",
  "test_the_cli_status_show_prints_the_status_and_one_block_per_child_with_waiting_items",
  "test_the_cli_status_show_prints_the_now_of_each_open_child_in_one_block_and_no_block_without_children",
  "test_reply_posts_the_file_text_for_the_cwd_session_and_prints_the_readback",
  "test_reply_reads_stdin_when_the_file_is_a_dash",
  "test_server_refusal_exits_non_zero_with_the_detail_on_stderr",
  "test_stale_thread_refusal_exits_non_zero_with_the_detail_on_stderr",
  "test_read_defaults_to_the_session_thread_with_url_null_and_limit_50",
  "test_read_passes_the_url_and_limit_through",
  "test_limit_out_of_range_is_a_usage_error",
  "test_check_posts_an_empty_body_prints_the_readback_and_exits_zero_when_ok",
  "test_check_exits_one_after_printing_when_ok_is_false",
  "test_help_lists_discord",
  "test_relative_profile_home_is_rejected",
];

#[test]
fn every_ported_python_test_has_a_golden_case() {
  let stems: Vec<String> = case_files()
    .iter()
    .map(|path| path.file_stem().expect("a case file has a stem").to_string_lossy().into_owned())
    .collect();
  for name in PORTED_PYTHON_TESTS {
    assert!(stems.iter().any(|stem| stem.starts_with(name)), "{name} has no golden case file");
  }
}

fn case_files() -> Vec<PathBuf> {
  let mut files = Vec::new();
  let directories = std::fs::read_dir(CASES_DIR).expect("tests/golden exists");
  for directory in directories {
    let directory = directory.expect("readable").path();
    if !directory.is_dir() {
      continue;
    }
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&directory)
      .expect("readable")
      .map(|entry| entry.expect("readable").path())
      .filter(|path| path.extension().is_some_and(|extension| extension == "json"))
      .collect();
    paths.sort();
    files.extend(paths);
  }
  files.sort();
  files
}

// ---------------------------------------------------------------------------
// The stub server
// ---------------------------------------------------------------------------

/// One request the stub received, in the shape `expect.requests` records.
#[derive(Debug, Clone)]
struct RecordedRequest {
  method: String,
  path: String,
  query: String,
  authorization: Option<String>,
  caller_session: Option<String>,
  content_type: Option<String>,
  body: Value,
}

impl RecordedRequest {
  fn to_value(&self) -> Value {
    json!({
      "method": self.method,
      "path": self.path,
      "query": self.query,
      "headers": {
        "Authorization": self.authorization,
        "X-CharlieBot-Caller-Session": self.caller_session,
        "Content-Type": self.content_type,
      },
      "body": self.body,
    })
  }
}

enum ScriptedResponse {
  Answer { status: u16, body: Value },
  DropAfterRequest,
}

struct StubServer {
  port: u16,
  requests: Arc<Mutex<Vec<RecordedRequest>>>,
  stop: Arc<AtomicBool>,
}

impl StubServer {
  /// The port with no listener: `refuse_connect` cases reserve one and release it.
  fn refused_port() -> StubServer {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("a free port binds");
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    StubServer { port, requests: Arc::new(Mutex::new(Vec::new())), stop: Arc::new(AtomicBool::new(false)) }
  }

  fn stop(&self) {
    self.stop.store(true, Ordering::Relaxed);
  }
}

fn start_stub(responses: &[Value]) -> StubServer {
  let refuse_connect = responses.iter().any(|response| response.get("refuse_connect").is_some());
  if refuse_connect {
    return StubServer::refused_port();
  }
  let listener = TcpListener::bind(("127.0.0.1", 0)).expect("a free port binds");
  let port = listener.local_addr().unwrap().port();
  let mut scripted: VecDeque<ScriptedResponse> = responses
    .iter()
    .map(|response| {
      if response.get("drop_after_request").is_some() {
        ScriptedResponse::DropAfterRequest
      } else {
        ScriptedResponse::Answer {
          status: response["status"].as_u64().unwrap_or(200) as u16,
          body: response["body"].clone(),
        }
      }
    })
    .collect();
  let requests = Arc::new(Mutex::new(Vec::new()));
  let stop = Arc::new(AtomicBool::new(false));
  let thread_requests = requests.clone();
  let thread_stop = stop.clone();
  std::thread::spawn(move || {
    let _ = listener.set_nonblocking(true);
    loop {
      match listener.accept() {
        Ok((mut stream, _)) => {
          if let Some(request) = read_request(&mut stream) {
            thread_requests.lock().unwrap().push(request);
            match scripted.pop_front() {
              Some(ScriptedResponse::Answer { status, body }) => write_response(&mut stream, status, &body),
              Some(ScriptedResponse::DropAfterRequest) => {}
              None => write_response(
                &mut stream,
                500,
                &json!({ "detail": "the stub has no scripted response for this request" }),
              ),
            }
          }
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
          if thread_stop.load(Ordering::Relaxed) {
            break;
          }
          std::thread::sleep(Duration::from_millis(5));
        }
        Err(_) => break,
      }
    }
  });
  StubServer { port, requests, stop }
}

fn read_request(stream: &mut TcpStream) -> Option<RecordedRequest> {
  let mut buffer = Vec::new();
  let mut chunk = [0u8; 4096];
  let header_end = loop {
    if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
      break position;
    }
    let read = stream.read(&mut chunk).ok()?;
    if read == 0 {
      return None;
    }
    buffer.extend_from_slice(&chunk[..read]);
  };
  let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
  let mut lines = head.split("\r\n");
  let mut request_line = lines.next()?.split(' ');
  let method = request_line.next()?.to_string();
  let target = request_line.next()?.to_string();
  let (path, query) = match target.split_once('?') {
    Some((path, query)) => (path.to_string(), query.to_string()),
    None => (target, String::new()),
  };
  let mut headers = Vec::new();
  for line in lines {
    if let Some((name, value)) = line.split_once(':') {
      headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }
  }
  let header_value = |name: &str| headers.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone());
  let content_length = header_value("content-length").and_then(|value| value.parse::<usize>().ok()).unwrap_or(0);
  let body_start = header_end + 4;
  while buffer.len() < body_start + content_length {
    let read = stream.read(&mut chunk).ok()?;
    if read == 0 {
      break;
    }
    buffer.extend_from_slice(&chunk[..read]);
  }
  let body_end = (body_start + content_length).min(buffer.len());
  let body = if body_end > body_start {
    serde_json::from_slice(&buffer[body_start..body_end]).unwrap_or(Value::Null)
  } else {
    Value::Null
  };
  Some(RecordedRequest {
    method,
    path,
    query,
    authorization: header_value("authorization"),
    caller_session: header_value("x-charliebot-caller-session"),
    content_type: header_value("content-type"),
    body,
  })
}

fn write_response(stream: &mut TcpStream, status: u16, body: &Value) {
  let body_bytes = serde_json::to_vec(body).expect("the scripted body serializes");
  let response = format!(
    "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
    status,
    reason_phrase(status),
    body_bytes.len()
  );
  let _ = stream.write_all(response.as_bytes());
  let _ = stream.write_all(&body_bytes);
  let _ = stream.flush();
}

fn reason_phrase(status: u16) -> &'static str {
  match status {
    200 => "OK",
    400 => "Bad Request",
    401 => "Unauthorized",
    403 => "Forbidden",
    404 => "Not Found",
    409 => "Conflict",
    422 => "Unprocessable Entity",
    500 => "Internal Server Error",
    _ => "Status",
  }
}

// ---------------------------------------------------------------------------
// The case runner
// ---------------------------------------------------------------------------

fn run_case(case_path: &Path) -> Result<(), String> {
  let case_text = std::fs::read_to_string(case_path).map_err(|error| error.to_string())?;
  let case: Value = serde_json::from_str(&case_text).map_err(|error| format!("invalid case JSON: {error}"))?;
  let temp = case_tempdir().map_err(|error| error.to_string())?;
  let root = temp.path().to_str().ok_or("the temp root is not Unicode")?.to_string();

  // A scripted body may name the run's temp root (a host-served link points into it), so `{root}` substitutes there too.
  let responses: Vec<Value> = case
    .get("responses")
    .and_then(Value::as_array)
    .map(|responses| {
      responses
        .iter()
        .map(|response| {
          let text = serde_json::to_string(response).expect("a scripted response serializes").replace("{root}", &root);
          serde_json::from_str(&text).expect("a substituted response is valid JSON")
        })
        .collect()
    })
    .unwrap_or_default();
  let stub = start_stub(&responses);
  let repo =
    std::fs::canonicalize(REPO_ROOT).map_err(|error| format!("{REPO_ROOT}: {error}"))?.to_string_lossy().into_owned();
  let substitute =
    |text: &str| text.replace("{root}", &root).replace("{port}", &stub.port.to_string()).replace("{repo}", &repo);

  if let Some(files) = case.get("files").and_then(Value::as_object) {
    for (relative, content) in files {
      let path = temp.path().join(substitute(relative));
      if relative.ends_with('/') {
        std::fs::create_dir_all(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        continue;
      }
      let text = checkout_file(&substitute(content.as_str().ok_or("file contents must be strings")?))?;
      if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
      }
      std::fs::write(&path, text).map_err(|error| format!("{}: {error}", path.display()))?;
      if relative.starts_with("bin/") {
        // A stub agent CLI under `bin/` runs as a program on the case's PATH.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
          .map_err(|error| format!("{}: {error}", path.display()))?;
      }
    }
  }

  // A case's executable files: the stub renderer and stub agent CLIs a check launches. The bytes are a script, and
  // the mode makes it runnable; `files` alone cannot, and the tests must start no real browser or agent CLI.
  if let Some(executables) = case.get("executables").and_then(Value::as_object) {
    for (relative, content) in executables {
      let path = temp.path().join(substitute(relative));
      let text = checkout_file(&substitute(content.as_str().ok_or("executable contents must be strings")?))?;
      if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
      }
      std::fs::write(&path, text).map_err(|error| format!("{}: {error}", path.display()))?;
      let mut permissions = std::fs::metadata(&path).map_err(|error| error.to_string())?.permissions();
      permissions.set_mode(0o755);
      std::fs::set_permissions(&path, permissions).map_err(|error| format!("{}: {error}", path.display()))?;
    }
  }

  let cwd = temp.path().join(substitute(case.get("cwd").and_then(Value::as_str).unwrap_or(".")));
  std::fs::create_dir_all(&cwd).map_err(|error| format!("{}: {error}", cwd.display()))?;
  let argv: Vec<String> = case
    .get("argv")
    .and_then(Value::as_array)
    .cloned()
    .unwrap_or_default()
    .iter()
    .map(|item| item.as_str().ok_or("argv entries must be strings"))
    .collect::<Result<Vec<&str>, _>>()
    .map_err(|message| message.to_string())?
    .iter()
    .map(|item| substitute(item))
    .collect();

  let mut command = Command::new(env!("CARGO_BIN_EXE_charliebot"));
  let stdin = case.get("stdin").and_then(Value::as_str).map(substitute);
  command
    .args(&argv)
    .current_dir(&cwd)
    .env_clear()
    .env("CHARLIEBOT_HOME", temp.path().join("home"))
    .env("HOME", temp.path().join("user-home"))
    .env("CHARLIEBOT_REPO_ROOT", temp.path().join("repo"))
    .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
  if let Some(env) = case.get("env").and_then(Value::as_object) {
    for (key, value) in env {
      command.env(key, substitute(value.as_str().ok_or("env values must be strings")?));
    }
  }
  let mut child = command.spawn().map_err(|error| format!("spawn failed: {error}"))?;
  if let Some(stdin) = stdin {
    child
      .stdin
      .take()
      .expect("stdin is piped for a case with stdin")
      .write_all(stdin.as_bytes())
      .map_err(|error| format!("write CLI stdin: {error}"))?;
  }

  let Some(status) = wait_bounded(&mut child)? else {
    stub.stop();
    return Err(format!("the CLI process exceeded {} s and was killed", PROCESS_TIMEOUT.as_secs()));
  };
  let mut stdout_bytes = Vec::new();
  let _ = child.stdout.take().expect("stdout piped").read_to_end(&mut stdout_bytes);
  let mut stderr_bytes = Vec::new();
  let _ = child.stderr.take().expect("stderr piped").read_to_end(&mut stderr_bytes);
  stub.stop();

  let requests: Vec<Value> = stub
    .requests
    .lock()
    .expect("the stub lock")
    .iter()
    .map(RecordedRequest::to_value)
    .map(|request| normalize_value(&request, &root, stub.port))
    .collect();
  let scripted =
    if responses.iter().any(|response| response.get("refuse_connect").is_some()) { 0 } else { responses.len() };
  if requests.len() > scripted {
    return Err(format!("the run made {} request(s) but the case scripts {}", requests.len(), scripted));
  }

  let stdout = normalize(&String::from_utf8_lossy(&stdout_bytes), &root);
  let stderr = normalize(&String::from_utf8_lossy(&stderr_bytes), &root);
  let exit = status.code().ok_or("the CLI process exited from a signal")?;

  replay_case(temp.path(), &case, &stdout, &stderr, exit, &requests)
}

/// A case's file content: `{file:<checkout-relative path>}` reads the checkout's own file, so a case can use the
/// shipped prompt or genre template without embedding a copy that drifts.
fn checkout_file(content: &str) -> Result<String, String> {
  let Some(relative) = content.strip_prefix("{file:").and_then(|rest| rest.strip_suffix('}')) else {
    return Ok(content.to_string());
  };
  let path = Path::new(REPO_ROOT).join(relative);
  std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))
}

/// One case's temp root. The base directory is fixed and the case directory name has a fixed
/// length, so a recorded byte that depends on the root's length — a hook part's character count,
/// a split boundary — replays identically on every host, whatever the run's `TMPDIR` is and how
/// long its process id grew. The tempdir still cleans the case directory up on drop.
fn case_tempdir() -> std::io::Result<tempfile::TempDir> {
  let base = std::path::Path::new("/tmp").join("charliebot-golden");
  std::fs::create_dir_all(&base)?;
  tempfile::Builder::new().prefix("case-").rand_bytes(16).tempdir_in(&base)
}

/// Wait for one CLI process. Past `PROCESS_TIMEOUT` it is killed and `None` answers.
fn wait_bounded(child: &mut Child) -> Result<Option<ExitStatus>, String> {
  let started = Instant::now();
  loop {
    match child.try_wait().map_err(|error| error.to_string())? {
      Some(status) => return Ok(Some(status)),
      None if started.elapsed() > PROCESS_TIMEOUT => {
        let _ = child.kill();
        let _ = child.wait();
        return Ok(None);
      }
      None => std::thread::sleep(Duration::from_millis(10)),
    }
  }
}

fn replay_case(
  root: &Path,
  case: &Value,
  stdout: &str,
  stderr: &str,
  exit: i32,
  requests: &[Value],
) -> Result<(), String> {
  let expect = case.get("expect").ok_or("the case has no expect section")?;
  let parser_generated = case.get("parser_generated").and_then(Value::as_bool).unwrap_or(false);
  let expected_exit = expect.get("exit").and_then(Value::as_i64).ok_or("the case has no expect.exit")? as i32;
  if exit != expected_exit {
    return Err(format!("exit {exit}, expected {expected_exit}; stderr: {stderr}"));
  }
  if parser_generated {
    return Ok(());
  }
  for (name, actual, expected) in [("stdout", stdout, expect.get("stdout")), ("stderr", stderr, expect.get("stderr"))] {
    let expected = expected.and_then(Value::as_str).unwrap_or("");
    if !text_matches(expected, actual) {
      return Err(format!("{name} differs:\nexpected: {expected:?}\nactual:   {actual:?}"));
    }
  }
  let expected_requests = expect.get("requests").and_then(Value::as_array).cloned().unwrap_or_default();
  if requests.len() != expected_requests.len() {
    return Err(format!("the stub received {} request(s), expected {}", requests.len(), expected_requests.len()));
  }
  let root_text = root.to_str().ok_or("the temp root is not Unicode")?;
  for (index, (expected, actual)) in expected_requests.iter().zip(requests).enumerate() {
    let actual = root_placed_value(actual, root_text);
    if !value_matches(expected, &actual) {
      return Err(format!(
        "request {index} differs:\nexpected: {}\nactual:   {}",
        charliebot_core::text_repr::json_dumps(expected, &indent_two_style()),
        charliebot_core::text_repr::json_dumps(&actual, &indent_two_style())
      ));
    }
  }
  if let Some(files) = expect.get("files").and_then(Value::as_object) {
    for (relative, expected_content) in files {
      let path = root.join(relative);
      let content = std::fs::read_to_string(path).map_err(|error| format!("expect.files {relative}: {error}"))?;
      if content != expected_content.as_str().unwrap_or_default() {
        return Err(format!("expect.files {relative}: content differs"));
      }
    }
  }
  Ok(())
}

fn indent_two_style() -> charliebot_core::text_repr::JsonStyle {
  charliebot_core::text_repr::JsonStyle {
    ensure_ascii: false,
    sort_keys: false,
    item_separator: ",",
    key_separator: ": ",
    indent: Some(2),
  }
}

/// The run's temp root reads as `{root}` so a case file does not bake one run's path in.
///
/// The Python CLI's structlog console lines carry a wall-clock stamp and a pid, so they can never replay; they are
/// the Python process's own chatter, not the command's answer, and the Rust CLI prints none.
fn normalize(text: &str, root: &str) -> String {
  static LOG_LINE: OnceLock<Regex> = OnceLock::new();
  let log_line = LOG_LINE.get_or_init(|| {
    Regex::new(r"^\d{4}-\d\d-\d\d \d\d:\d\d:\d\d \[(?:debug|info|warning|error|critical)\s*\]")
      .expect("the structlog console prefix is a valid regex")
  });
  text.replace(root, "{root}").split_inclusive('\n').filter(|line| !log_line.is_match(line)).collect()
}

/// Normalize temporary paths and the stub's dynamic port so a recorded case can replay in another run.
fn normalize_value(value: &Value, root: &str, port: u16) -> Value {
  match value {
    Value::String(text) => {
      Value::String(normalize(text, root).replace(&format!("localhost:{port}"), "localhost:{port}"))
    }
    Value::Array(items) => Value::Array(items.iter().map(|item| normalize_value(item, root, port)).collect()),
    Value::Object(members) => {
      Value::Object(members.iter().map(|(key, item)| (key.clone(), normalize_value(item, root, port))).collect())
    }
    other => other.clone(),
  }
}

/// Expected text with `{uuid4}` / `{timestamp}` placeholders against actual text.
///
/// The placeholders become their patterns after `regex::escape`, so the rest of the expected text still compares
/// byte for byte.
fn text_matches(expected: &str, actual: &str) -> bool {
  let pattern = regex::escape(expected)
    .replace(r"\{uuid4\}", r"[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}")
    .replace(r"\{timestamp\}", r"[0-9]{8}T[0-9]{6}Z");
  Regex::new(&format!("^{pattern}$")).is_ok_and(|regex| regex.is_match(actual))
}

/// Expected request values against actual ones; `{uuid4}` and `{timestamp}` match any value of their shape.
fn value_matches(expected: &Value, actual: &Value) -> bool {
  match (expected, actual) {
    (Value::String(expected), Value::String(actual)) => text_matches(expected, actual),
    (Value::Array(expected), Value::Array(actual)) => {
      expected.len() == actual.len()
        && expected.iter().zip(actual).all(|(expected, actual)| value_matches(expected, actual))
    }
    (Value::Object(expected), Value::Object(actual)) => {
      expected.len() == actual.len()
        && expected.iter().all(|(key, value)| actual.get(key).is_some_and(|actual| value_matches(value, actual)))
    }
    (expected, actual) => expected == actual,
  }
}

/// Actual request values with the run's temp root replaced by `{root}`, matching how the case recorded them.
fn root_placed_value(value: &Value, root: &str) -> Value {
  match value {
    Value::String(text) => Value::String(text.replace(root, "{root}")),
    Value::Array(items) => Value::Array(items.iter().map(|item| root_placed_value(item, root)).collect()),
    Value::Object(members) => {
      Value::Object(members.iter().map(|(key, value)| (key.clone(), root_placed_value(value, root))).collect())
    }
    other => other.clone(),
  }
}

/// A changed `server.port` in config.yaml takes effect on the next call: the port is read from the
/// config on every call, and there is no port cache to serve a stale one (Python's
/// `test_port_cache_hit_serves_the_cached_port_in_a_fresh_process` has no Rust counterpart).
#[test]
fn changed_server_port_takes_effect_on_the_next_call() {
  let temp = tempfile::tempdir().unwrap();
  let config_path = temp.path().join("home").join("config.yaml");
  let run = |stub: &StubServer| {
    std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    std::fs::write(&config_path, format!("server:\n  port: {}\n", stub.port)).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_charliebot"))
      .args(["session", "tree"])
      .current_dir(temp.path())
      .env_clear()
      .env("CHARLIEBOT_HOME", temp.path().join("home"))
      .env("HOME", temp.path().join("user-home"))
      .stdin(Stdio::null())
      .stdout(Stdio::null())
      .stderr(Stdio::piped())
      .spawn()
      .unwrap();
    let status = wait_bounded(&mut child).unwrap().expect("the CLI finishes within the process timeout");
    let mut stderr = String::new();
    child.stderr.take().unwrap().read_to_string(&mut stderr).unwrap();
    assert_eq!(status.code(), Some(0), "stderr: {stderr}");
    assert_eq!(stub.requests.lock().unwrap().len(), 1);
  };

  let first = start_stub(&[json!({ "status": 200, "body": { "name": "first" } })]);
  run(&first);
  first.stop();
  drop(first);

  let second = start_stub(&[json!({ "status": 200, "body": { "name": "second" } })]);
  run(&second);
  second.stop();
}

mod authenticated_commands {
  use super::*;
  use std::process::Output;
  use std::sync::mpsc::{self, Receiver};
  use std::thread;

  const ACCESS_KEY: &str = "synthetic-file-access-key";
  const LINEAR_API_KEY: &str = "synthetic-linear-api-key";

  fn home_for_port(port: u16) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("config.yaml"), format!("server:\n  port: {port}\n")).unwrap();
    std::fs::write(home.path().join("credentials.yaml"), format!("charliebot:\n  access_key: {ACCESS_KEY}\n")).unwrap();
    home
  }

  fn home_for_linear() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
      home.path().join("credentials.yaml"),
      format!("charliebot:\n  access_key: {ACCESS_KEY}\nlinear:\n  api_key: {LINEAR_API_KEY}\n"),
    )
    .unwrap();
    std::fs::write(home.path().join("request.json"), r#"{"query":"query { viewer { id } }"}"#).unwrap();
    home
  }

  fn invoke_files(home: &tempfile::TempDir, link: &str, run_token: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_charliebot"));
    command
      .env_clear()
      .env("CHARLIEBOT_HOME", home.path())
      .env("HTTP_PROXY", "http://127.0.0.1:1")
      .args(["files", "probe", link]);
    if let Some(run_token) = run_token {
      command.env("CHARLIEBOT_RUN_TOKEN", run_token);
    }
    command.output().unwrap()
  }

  fn invoke_linear(home: &tempfile::TempDir, proxy_port: u16) -> Output {
    let proxy = format!("http://127.0.0.1:{proxy_port}");
    Command::new(env!("CARGO_BIN_EXE_charliebot"))
      .env_clear()
      .env("CHARLIEBOT_HOME", home.path())
      .env("HTTPS_PROXY", &proxy)
      .env("HTTP_PROXY", &proxy)
      .args(["linear", "graphql", "--file"])
      .arg(home.path().join("request.json"))
      .output()
      .unwrap()
  }

  fn start_stub(
    status: u16,
    response_body: &[u8],
    location: Option<&str>,
  ) -> (u16, Receiver<Vec<u8>>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let body = response_body.to_vec();
    let location = location.map(str::to_string);
    let (sender, receiver) = mpsc::channel();
    let handle = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      let request = read_request_head(&mut stream);
      sender.send(request).unwrap();
      let reason = match status {
        200 => "OK",
        302 => "Found",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Response",
      };
      let mut response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
      );
      if let Some(location) = location {
        response.push_str(&format!("Location: {location}\r\n"));
      }
      response.push_str("\r\n");
      stream.write_all(response.as_bytes()).unwrap();
      stream.write_all(&body).unwrap();
    });
    (port, receiver, handle)
  }

  fn read_request_head(stream: &mut TcpStream) -> Vec<u8> {
    stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
      let read = stream.read(&mut buffer).unwrap();
      if read == 0 {
        return request;
      }
      request.extend_from_slice(&buffer[..read]);
      if request.windows(4).any(|window| window == b"\r\n\r\n") {
        return request;
      }
    }
  }

  fn assert_clean(output: &Output, forbidden: &[&str]) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    for value in forbidden {
      assert!(!stdout.contains(value), "stdout contains a synthetic credential");
      assert!(!stderr.contains(value), "stderr contains a synthetic credential");
    }
  }

  #[test]
  fn files_probe_uses_the_access_key_and_reports_status_without_response_bodies() {
    for status in [200, 401, 404, 302] {
      let (port, request_receiver, server) =
        start_stub(status, ACCESS_KEY.as_bytes(), (status == 302).then_some("/redirect"));
      let home = home_for_port(port);
      let output = invoke_files(&home, "/absolute_filepath/synthetic/file.txt?download=1", None);
      let request = request_receiver.recv_timeout(Duration::from_secs(2)).unwrap();
      server.join().unwrap();
      let request_text = String::from_utf8(request).unwrap();
      let lowered = request_text.to_ascii_lowercase();

      assert!(lowered.contains(&format!("authorization: bearer {ACCESS_KEY}")));
      assert!(request_text.starts_with("GET /absolute_filepath/synthetic/file.txt?download=1 HTTP/1.1\r\n"));
      assert_clean(&output, &[ACCESS_KEY]);
      if status == 200 {
        assert_eq!(output.status.code(), Some(0));
        assert_eq!(String::from_utf8(output.stdout).unwrap(), "200 /absolute_filepath/synthetic/file.txt?download=1\n");
      } else {
        assert_eq!(output.status.code(), Some(1));
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(error["status"], status);
        assert_eq!(error["path"], "/absolute_filepath/synthetic/file.txt?download=1");
        assert!(error.get("error").is_none());
      }
    }
  }

  #[test]
  fn files_probe_prefers_a_run_token_and_encodes_links_like_a_browser() {
    let run_token = charliebot_core::run_token::sign_run_token(
      &charliebot_core::run_token::RunTokenClaims {
        session_id: "synthetic-session".to_string(),
        run_id: "synthetic-run".to_string(),
        agent: "synthetic-agent".to_string(),
      },
      ACCESS_KEY,
    )
    .unwrap();
    let (port, request_receiver, server) = start_stub(200, &[], None);
    let home = home_for_port(port);
    let output = invoke_files(&home, "https://unrelated.example/a b/雪?encoded=%2F#fragment", Some(&run_token));
    let request = request_receiver.recv_timeout(Duration::from_secs(2)).unwrap();
    server.join().unwrap();
    let request_text = String::from_utf8(request).unwrap();
    let authorization = request_text
      .lines()
      .find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("authorization").then_some(value.trim())
      })
      .unwrap();

    assert!(authorization == format!("Bearer {run_token}"), "the run token was not sent as the authorization header");
    assert!(authorization != format!("Bearer {ACCESS_KEY}"), "the access key replaced the run token");
    assert!(request_text.starts_with("GET /a%20b/%E9%9B%AA?encoded=%2F HTTP/1.1\r\n"));
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "200 /a%20b/%E9%9B%AA?encoded=%2F\n");
    assert_clean(&output, &[ACCESS_KEY, &run_token]);
  }

  #[test]
  fn files_probe_handles_each_link_component_and_rejects_relative_paths() {
    let links = [
      ("/bare/path", "/bare/path"),
      ("/path/file?query=value", "/path/file?query=value"),
      ("/path/file#fragment", "/path/file"),
      ("/path/file name", "/path/file%20name"),
      ("/path/雪", "/path/%E9%9B%AA"),
      ("/path/%2f", "/path/%2f"),
    ];
    for (link, expected) in links {
      let (port, request_receiver, server) = start_stub(200, &[], None);
      let home = home_for_port(port);
      let output = invoke_files(&home, link, None);
      let request = request_receiver.recv_timeout(Duration::from_secs(2)).unwrap();
      server.join().unwrap();
      let request_text = String::from_utf8(request).unwrap();

      assert!(request_text.starts_with(&format!("GET {expected} HTTP/1.1\r\n")));
      assert!(request_text.to_ascii_lowercase().contains(&format!("authorization: bearer {ACCESS_KEY}")));
      assert_eq!(output.status.code(), Some(0));
      assert_clean(&output, &[ACCESS_KEY]);
    }

    let home = home_for_port(1);
    let output = invoke_files(&home, "relative/path", None);
    assert_eq!(output.status.code(), Some(2));
    assert_clean(&output, &[ACCESS_KEY]);
  }

  #[test]
  fn files_probe_connection_failures_have_no_status_or_credential() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let home = home_for_port(port);
    let output = invoke_files(&home, "/unreachable", None);

    assert_eq!(output.status.code(), Some(1));
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(error["status"].is_null());
    assert_eq!(error["path"], "/unreachable");
    assert!(error["error"].as_str().is_some_and(|cause| !cause.is_empty()));
    assert!(output.stdout.is_empty());
    assert_clean(&output, &[ACCESS_KEY]);
  }

  #[test]
  fn files_probe_credential_load_failure_names_only_the_file() {
    let home = home_for_port(1);
    std::fs::write(home.path().join("credentials.yaml"), format!("charliebot: [{ACCESS_KEY}]\n")).unwrap();
    let output = invoke_files(&home, "/unreachable", None);

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(error["status"].is_null());
    assert_eq!(error["path"], "/unreachable");
    assert_eq!(error["error"], "$CHARLIEBOT_HOME/credentials.yaml");
    assert_clean(&output, &[ACCESS_KEY]);
  }

  #[test]
  fn linear_graphql_connection_failures_report_a_cause_without_credentials() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let proxy_port = listener.local_addr().unwrap().port();
    drop(listener);
    let home = home_for_linear();
    let output = invoke_linear(&home, proxy_port);

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(error["status"].is_null());
    assert_eq!(error["response"], "request failed");
    assert!(error["error"].as_str().is_some_and(|cause| !cause.is_empty()));
    assert_clean(&output, &[ACCESS_KEY, LINEAR_API_KEY]);
  }

  #[test]
  fn files_probe_returns_after_the_status_line_without_reading_the_body() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sender, receiver) = mpsc::channel();
    let server = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      let request = read_request_head(&mut stream);
      sender.send(request).unwrap();
      stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000000000\r\nConnection: keep-alive\r\n\r\n").unwrap();
      stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
      let mut byte = [0_u8; 1];
      let _ = stream.read(&mut byte);
    });
    let home = home_for_port(port);
    let output = invoke_files(&home, "/large-trace", None);
    let request = receiver.recv_timeout(Duration::from_secs(2)).unwrap();
    server.join().unwrap();

    assert!(
      String::from_utf8_lossy(&request).to_ascii_lowercase().contains(&format!("authorization: bearer {ACCESS_KEY}"))
    );
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "200 /large-trace\n");
    assert_clean(&output, &[ACCESS_KEY]);
  }
}
