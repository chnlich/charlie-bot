//! The backend router and its CLI wrapper.
//!
//! `charliebot router` answers one routing question per process: the caller writes one JSON object on stdin and reads
//! one back on stdout.
//!
//!   {"role": "implement", "text": "<the work>", "candidates": [{"id": "claude-opus", "cooling_until": null}],
//!    "session_backend": "claude-opus"}
//!   {"routed": true, "backends": ["claude-opus"], "reason": "context p=0.81"}
//!
//! `<charliebot home>/router.yaml` (or the file `--config` names) lists the classes of work and the roles that route.
//! A role lists its classes; when it lists more than one, Jev chooses one of them, and the chosen class names the
//! backend the work runs on: `session` (the delegating session's backend id itself) or one `backends.options` id or one
//! `backends.pools` id. A role the file does not list is not routed, and its caller keeps its own default. The Jev key
//! comes from the `credentials.yaml` section that `jev.credential` names, even when `--config` names the file. The rules
//! file is read on every call, so an edit takes effect at the next choice.
//!
//! The caller bounds one routing call at [`timeouts::ROUTER_TIMEOUT_SECONDS`], which is why one attempt is bounded at 5 s
//! and the call has one retry: a timed-out attempt takes it at once, and an HTTP 429 takes it after a 10 s wait. Two
//! attempts plus that wait fit inside the budget. A second timeout or HTTP 429 fails the call, and the answer's reason
//! says when a timeout retry was needed.
//!
//! The answer is the only thing printed on stdout. Every failure — a broken rules file, a missing Jev key, malformed
//! stdin, a Jev error, a second timeout, a second HTTP 429 — writes `router: <message>` on stderr and exits 1; the
//! command never prints a fallback answer.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::exit::source_chain;
use charliebot_cli_common::json_out;
use charliebot_core::config;
use charliebot_core::credentials;
use charliebot_core::home;
use charliebot_core::paths;
use charliebot_core::text_repr;
use charliebot_core::timeouts;
use clap::{CommandFactory, FromArgMatches, Parser};
use serde_json::{Map, Value, json};
use serde_yaml::{Mapping, Value as YamlValue};

const ROUTER_FILENAME: &str = "router.yaml";

/// Jev reads at most this many characters of one request: a delegation's first characters and a thread's last ones. A
/// thread grows without bound, and Jev's input cap does not.
const TEXT_LIMIT: usize = 20_000;

/// The role whose text keeps its last characters. Every other role is a delegation, labelled with its task type.
const THREAD_ROLE: &str = "thread";

/// Keys of the rules-file form that this router no longer reads. A file that still has one is refused.
const OLD_FORM_FIELDS: &[&str] = &["routine", "design", "strong"];
const TOP_LEVEL_FIELDS: &[&str] = &["jev", "instructions", "classes", "roles"];
const JEV_FIELDS: &[&str] = &["url", "model", config::CREDENTIAL_KEY];
const CLASS_FIELDS: &[&str] = &["when", "run_on"];

/// The `run_on` keyword that sends a class to the delegating session's backend. Any other `run_on` value is one
/// `backends.options` id or one `backends.pools` id.
const RUN_ON_SESSION: &str = "session";

/// The name of the one question each choice asks Jev.
const CHOICE_QUESTION: &str = "kind";

/// The bounds that the caller's [`timeouts::ROUTER_TIMEOUT_SECONDS`] budget shapes: the bound on the first attempt, the
/// bound on the retry, and the pause before the retry after an HTTP 429. A timeout takes its retry at once. Production
/// gives both attempts the one configured bound. Tests shorten the first attempt's bound, so an attempt they make time
/// out costs little wall time, and keep the retry's bound generous, so the retry's answer never loses to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limits {
  pub request_timeout: Duration,
  pub retry_request_timeout: Duration,
  pub rate_limit_wait: Duration,
}

#[derive(Parser)]
#[command(name = "router", about = "Choose backends for one CharlieBot task: ask Jev which class of work it is")]
struct RouterCli {
  #[arg(
    long,
    value_name = "PATH",
    help = "Read the rules from PATH instead of <charliebot home>/router.yaml; the credentials still come from the home"
  )]
  config: Option<PathBuf>,
}

/// Parse the arguments and run one routing call. Parser errors print and exit on their own: help
/// exits 0, a usage error exits 2.
pub fn main(args: &[OsString]) {
  let command = with_infer_long_args(RouterCli::command());
  let argv = std::iter::once(OsString::from("charliebot router")).chain(args.iter().cloned());
  let matches = match command.try_get_matches_from(argv) {
    Ok(matches) => matches,
    Err(error) => error.exit(),
  };
  let cli = RouterCli::from_arg_matches(&matches).expect("try_get_matches_from validated the arguments");
  let mut raw = String::new();
  if let Err(error) = std::io::stdin().read_to_string(&mut raw) {
    eprintln!("router: {error}");
    std::process::exit(1);
  }
  let limits = Limits {
    request_timeout: Duration::from_secs(timeouts::ROUTER_JEV_REQUEST_TIMEOUT),
    retry_request_timeout: Duration::from_secs(timeouts::ROUTER_JEV_REQUEST_TIMEOUT),
    rate_limit_wait: Duration::from_secs(timeouts::ROUTER_JEV_RATE_LIMIT_WAIT),
  };
  match route(&raw, cli.config.as_deref(), &limits, &mut |pause| std::thread::sleep(pause)) {
    Ok(answer) => print!("{}", json_out::render(&answer, None)),
    Err(message) => {
      eprintln!("router: {message}");
      std::process::exit(1);
    }
  }
}

/// Config first, then the stdin request, so a broken rules file wins over a broken request.
fn route(
  raw: &str,
  rules_path: Option<&Path>,
  limits: &Limits,
  sleep: &mut dyn FnMut(Duration),
) -> Result<Value, String> {
  let home_dir = home::charliebot_home_dir().map_err(|error| error.to_string())?;
  let rules_path = rules_path.map_or_else(|| config_path(&home_dir), Path::to_path_buf);
  route_in_home(&home_dir, &rules_path, raw, limits, sleep)
}

fn route_in_home(
  home: &Path,
  rules_path: &Path,
  raw: &str,
  limits: &Limits,
  sleep: &mut dyn FnMut(Duration),
) -> Result<Value, String> {
  let rules = load_home_rules(home, rules_path)?;
  let request = read_request(raw)?;
  let answer = answer(home, &rules, &request, limits, sleep)?;
  Ok(json!({ "routed": answer.routed, "backends": answer.backends, "reason": answer.reason }))
}

/// `<charliebot home>/router.yaml`, the rules file `--config` replaces.
fn config_path(home: &Path) -> PathBuf {
  home.join(ROUTER_FILENAME)
}

/// The `backends.options` and `backends.pools` ids of the home's `config.yaml`, loaded by the same loader the server
/// uses. A `run_on` value is one of them.
fn live_backend_ids(home: &Path) -> Result<Vec<String>, String> {
  let registry = charliebot_packages::registry().map_err(|error| error.to_string())?;
  let cfg = registry.load_config(home).map_err(|error| error.to_string())?;
  Ok(
    cfg
      .backends
      .options
      .iter()
      .map(|option| option.id.clone())
      .chain(cfg.backends.pools.iter().map(|pool| pool.id.clone()))
      .collect(),
  )
}

fn load_home_rules(home: &Path, rules_path: &Path) -> Result<RouterRules, String> {
  let backend_ids = live_backend_ids(home)?;
  load_rules(rules_path, &backend_ids)
}

/// One validated rules file: where Jev lives, the question it answers, the classes, and the classes each role lists.
struct RouterRules {
  url: String,
  model: String,
  credential: String,
  instructions: String,
  classes: BTreeMap<String, ClassRule>,
  roles: BTreeMap<String, Vec<String>>,
}

/// One class of work: the criterion Jev reads for it and the backends it runs on.
struct ClassRule {
  when: String,
  run_on: RunOn,
}

enum RunOn {
  /// One backend id: a `backends.options` id runs as written, and a `backends.pools` id runs on the member the server
  /// selects.
  Backend(String),
  Session,
}

/// One decoded stdin object. `state` is the text Jev reads, already labelled and cut.
struct RouteRequest {
  role: String,
  state: String,
  candidate_ids: Vec<String>,
  session_backend: Option<String>,
}

/// The router's answer for one request.
struct Answer {
  routed: bool,
  backends: Vec<String>,
  reason: String,
}

fn yaml_key(name: &str) -> YamlValue {
  YamlValue::String(name.to_string())
}

fn field_error(path_text: &str, field: &str, expectation: &str) -> String {
  format!("{path_text}: {field} must be {expectation}")
}

fn required_text(mapping: &Mapping, field: &str, path_text: &str, prefix: &str) -> Result<String, String> {
  match mapping.get(yaml_key(field)) {
    Some(YamlValue::String(text)) if !text.trim().is_empty() => Ok(text.trim().to_string()),
    _ => Err(field_error(path_text, &format!("{prefix}{field}"), "a non-empty string")),
  }
}

/// Refuse a key the mapping does not define, so a misspelled or retired field cannot pass silently. `prefix` is the
/// dotted path of the mapping with its trailing dot, or empty at the top level.
fn reject_unknown_fields(mapping: &Mapping, known: &[&str], path_text: &str, prefix: &str) -> Result<(), String> {
  for key in mapping.keys() {
    match key.as_str() {
      Some(name) if known.contains(&name) => {}
      Some(name) => {
        return Err(format!("{path_text}: {prefix}{name} is not a field here; the fields are {}", known.join(", ")));
      }
      None => return Err(format!("{path_text}: {prefix}keys must be text")),
    }
  }
  Ok(())
}

/// Read and validate one rules file. A missing file, an old-form file, an unknown field, an unknown class in a role and
/// an unknown backend id each fail with a message that names the file and the field.
fn load_rules(path: &Path, backend_ids: &[String]) -> Result<RouterRules, String> {
  let path_text = paths::display(path);
  let text = match std::fs::read_to_string(path) {
    Ok(text) => text,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
      return Err(format!("{path_text}: missing; the router reads its Jev address, classes and roles from it"));
    }
    Err(error) => return Err(format!("{path_text}: {error}")),
  };
  let document: YamlValue = serde_yaml::from_str(&text).map_err(|error| format!("{path_text}: {error}"))?;
  let expectation = "a mapping with jev, instructions, classes and roles";
  let Some(document) = document.as_mapping() else {
    return Err(field_error(&path_text, "the document", expectation));
  };
  if let Some(field) = OLD_FORM_FIELDS.iter().copied().find(|field| document.contains_key(yaml_key(field))) {
    return Err(format!(
      "{path_text}: {field} is the old router form; write classes and roles instead (see configs/router.example.yaml)"
    ));
  }
  reject_unknown_fields(document, TOP_LEVEL_FIELDS, &path_text, "")?;
  let Some(YamlValue::Mapping(jev)) = document.get(yaml_key("jev")) else {
    return Err(field_error(&path_text, "jev", "a mapping"));
  };
  reject_unknown_fields(jev, JEV_FIELDS, &path_text, "jev.")?;
  let url = required_text(jev, "url", &path_text, "jev.")?;
  let model = required_text(jev, "model", &path_text, "jev.")?;
  let credential = required_text(jev, config::CREDENTIAL_KEY, &path_text, "jev.")?;
  let instructions = required_text(document, "instructions", &path_text, "")?;
  let classes = classes_of(document, backend_ids, &path_text)?;
  let roles = roles_of(document, &classes, &path_text)?;
  Ok(RouterRules { url, model, credential, instructions, classes, roles })
}

fn classes_of(
  document: &Mapping,
  backend_ids: &[String],
  path_text: &str,
) -> Result<BTreeMap<String, ClassRule>, String> {
  let expectation = "a non-empty mapping of class names to rules";
  let Some(YamlValue::Mapping(classes)) = document.get(yaml_key("classes")) else {
    return Err(field_error(path_text, "classes", expectation));
  };
  if classes.is_empty() {
    return Err(field_error(path_text, "classes", expectation));
  }
  let mut rules = BTreeMap::new();
  for (name, rule) in classes {
    let Some(name) = name.as_str() else {
      return Err(field_error(path_text, "classes", "class names that are text"));
    };
    let YamlValue::Mapping(rule) = rule else {
      return Err(field_error(path_text, &format!("classes.{name}"), "a mapping with when and run_on"));
    };
    let prefix = format!("classes.{name}.");
    reject_unknown_fields(rule, CLASS_FIELDS, path_text, &prefix)?;
    rules.insert(
      name.to_string(),
      ClassRule {
        when: required_text(rule, "when", path_text, &prefix)?,
        run_on: run_on_of(rule, name, backend_ids, path_text)?,
      },
    );
  }
  Ok(rules)
}

fn run_on_of(rule: &Mapping, class: &str, backend_ids: &[String], path_text: &str) -> Result<RunOn, String> {
  let field = format!("classes.{class}.run_on");
  let expectation = "session, or one backends.options or backends.pools id";
  match rule.get(yaml_key("run_on")) {
    Some(YamlValue::String(target)) if target == RUN_ON_SESSION => Ok(RunOn::Session),
    Some(YamlValue::String(target)) if backend_ids.contains(target) => Ok(RunOn::Backend(target.clone())),
    Some(YamlValue::String(target)) => Err(format!(
      "{path_text}: {field}: '{target}' is not a backends.options or backends.pools id; write session, one model id or \
       one pool id"
    )),
    Some(YamlValue::Sequence(_)) => {
      Err(format!("{path_text}: {field}: a list of backend ids is retired; write session, one model id or one pool id"))
    }
    _ => Err(field_error(path_text, &field, expectation)),
  }
}

fn roles_of(
  document: &Mapping,
  classes: &BTreeMap<String, ClassRule>,
  path_text: &str,
) -> Result<BTreeMap<String, Vec<String>>, String> {
  let Some(YamlValue::Mapping(roles)) = document.get(yaml_key("roles")) else {
    return Err(field_error(path_text, "roles", "a mapping of role names to lists of class names"));
  };
  let mut table = BTreeMap::new();
  for (role, names) in roles {
    let Some(role) = role.as_str() else {
      return Err(field_error(path_text, "roles", "role names that are text"));
    };
    let field = format!("roles.{role}");
    let YamlValue::Sequence(names) = names else {
      return Err(field_error(path_text, &field, "a non-empty list of class names"));
    };
    if names.is_empty() {
      return Err(field_error(path_text, &field, "a non-empty list of class names"));
    }
    let mut listed = Vec::new();
    for (index, name) in names.iter().enumerate() {
      let Some(name) = name.as_str() else {
        return Err(field_error(path_text, &format!("{field}[{index}]"), "a class name"));
      };
      if !classes.contains_key(name) {
        return Err(format!("{path_text}: {field}[{index}]: '{name}' is not a class in classes"));
      }
      listed.push(name.to_string());
    }
    table.insert(role.to_string(), listed);
  }
  Ok(table)
}

/// Decode one stdin object. The text Jev reads is built and cut here, so the server and a calibration replay that send
/// the same role and spec get the same Jev input.
fn read_request(raw: &str) -> Result<RouteRequest, String> {
  let document: Value = serde_json::from_str(raw).map_err(|error| format!("stdin: not one JSON object: {error}"))?;
  let Some(document) = document.as_object() else {
    return Err("stdin: the request must be one JSON object".to_string());
  };
  let role = match document.get("role") {
    Some(Value::String(role)) if !role.trim().is_empty() => role.clone(),
    _ => return Err("stdin.role: must be a non-empty string".to_string()),
  };
  let text = match document.get("text") {
    Some(Value::String(text)) => text,
    _ => return Err("stdin.text: must be a string".to_string()),
  };
  let Some(Value::Array(candidates)) = document.get("candidates") else {
    return Err("stdin.candidates: must be a list of {id, cooling_until} objects".to_string());
  };
  let mut candidate_ids = Vec::new();
  for (index, candidate) in candidates.iter().enumerate() {
    let identifier = candidate.as_object().and_then(|candidate| candidate.get("id"));
    match identifier {
      Some(Value::String(identifier)) if !identifier.trim().is_empty() => candidate_ids.push(identifier.clone()),
      _ => return Err(format!("stdin.candidates[{index}].id: must be a non-empty string")),
    }
  }
  let session_backend = match document.get("session_backend") {
    Some(Value::Null) => None,
    Some(Value::String(backend)) if !backend.trim().is_empty() => Some(backend.clone()),
    _ => return Err("stdin.session_backend: must be a backend id or null".to_string()),
  };
  Ok(RouteRequest { state: jev_state(&role, text), role, candidate_ids, session_backend })
}

/// The text Jev reads for one request: a delegation's task-type line and spec, cut to its first [`TEXT_LIMIT`]
/// characters, or a thread's text cut to its last [`TEXT_LIMIT`] characters.
fn jev_state(role: &str, text: &str) -> String {
  if role == THREAD_ROLE {
    return last_chars(text, TEXT_LIMIT);
  }
  first_chars(&format!("Task type: {role}\n\n{text}"), TEXT_LIMIT)
}

/// `text[:limit]` in characters, not bytes.
fn first_chars(text: &str, limit: usize) -> String {
  text.chars().take(limit).collect()
}

/// `text[-limit:]` in characters, not bytes.
fn last_chars(text: &str, limit: usize) -> String {
  let count = text.chars().count();
  if count <= limit {
    return text.to_string();
  }
  text.chars().skip(count - limit).collect()
}

/// The answer for one request. A role the rules do not list answers not routed without a Jev call; a role with one class
/// answers that class without one; otherwise Jev chooses among the role's classes.
fn answer(
  home: &Path,
  rules: &RouterRules,
  request: &RouteRequest,
  limits: &Limits,
  sleep: &mut dyn FnMut(Duration),
) -> Result<Answer, String> {
  let Some(classes) = rules.roles.get(&request.role) else {
    return Ok(Answer { routed: false, backends: Vec::new(), reason: format!("{} not routed", request.role) });
  };
  let (chosen, probability, retried_timeout) = if let [only] = classes.as_slice() {
    (only.as_str(), 1.0, false)
  } else {
    choose_class(home, rules, classes, &request.state, limits, sleep)?
  };
  let mut reason = format!("{chosen} p={}", text_repr::format_f(probability, 2));
  if retried_timeout {
    reason.push_str("; retried after a timeout");
  }
  let backend = match &rules.classes[chosen].run_on {
    RunOn::Backend(backend_id) if request.candidate_ids.contains(backend_id) => backend_id.clone(),
    RunOn::Backend(backend_id) => {
      let reason = format!("{reason}; backend {backend_id} is not a candidate here; ran on the default path");
      return Ok(Answer { routed: false, backends: Vec::new(), reason });
    }
    RunOn::Session => match &request.session_backend {
      Some(session_backend) => session_backend.clone(),
      None => {
        return Err(format!(
          "classes.{chosen}.run_on: session runs on the delegating session's backend, but the request names no session_backend"
        ));
      }
    },
  };
  Ok(Answer { routed: true, backends: vec![backend], reason })
}

/// Ask Jev, with at most one retry after a timeout or an HTTP 429, which of one role's classes fits `state`, and the
/// probability Jev gave that class. Probabilities that tie at the top go to the class the role lists first; otherwise
/// the choice is Jev's `choice`. The flag reports whether the retry absorbed a timeout.
fn choose_class<'a>(
  home: &Path,
  rules: &RouterRules,
  classes: &'a [String],
  state: &str,
  limits: &Limits,
  sleep: &mut dyn FnMut(Duration),
) -> Result<(&'a str, f64, bool), String> {
  let loaded = credentials::load_credentials(home).map_err(|error| error.to_string())?;
  let api_key = loaded.require(&rules.credential, "api_key").map_err(|error| error.to_string())?;
  let reply = post(&rules.url, &jev_body(rules, classes, state), &api_key, limits, sleep)?;
  let payload = &reply.payload;
  let probabilities =
    classes.iter().map(|class| class_probability(&rules.url, payload, class)).collect::<Result<Vec<f64>, String>>()?;
  let top = probabilities.iter().copied().fold(f64::NEG_INFINITY, f64::max);
  let tied: Vec<usize> = (0..classes.len()).filter(|&index| probabilities[index] == top).collect();
  if let [first, _, ..] = tied.as_slice() {
    return Ok((classes[*first].as_str(), top, reply.retried_timeout));
  }
  let choice = choice_of(&rules.url, payload)?;
  let Some(index) = classes.iter().position(|class| class == choice) else {
    return Err(format!("{}: the answer's choice '{choice}' is not one of the role's classes", rules.url));
  };
  Ok((classes[index].as_str(), probabilities[index], reply.retried_timeout))
}

/// Jev's choice request: one question with the role's classes as its criteria.
fn jev_body(rules: &RouterRules, classes: &[String], state: &str) -> Value {
  let criteria: Map<String, Value> =
    classes.iter().map(|class| (class.clone(), Value::String(rules.classes[class].when.clone()))).collect();
  json!({
    "model": rules.model,
    "state": state,
    "questions": {
      CHOICE_QUESTION: {
        "type": "choice",
        "instructions": rules.instructions,
        "criteria": criteria,
      },
    },
  })
}

/// One Jev reply and whether its call needed the one timeout retry.
struct JevReply {
  payload: Value,
  retried_timeout: bool,
}

/// What one Jev attempt ended in. Only an HTTP 429 and a timeout are retryable; every other failure is final.
enum Attempt {
  Answered(Value),
  RateLimited(String),
  TimedOut(String),
  Failed(String),
}

/// POST one Jev request. The call has one retry: a timed-out attempt takes it at once, and an HTTP 429 takes it after
/// `rate_limit_wait`. A failure of the retry attempt — a timeout, an HTTP 429, or anything else — ends the call with an
/// error that names the retry; a first attempt that fails another way ends it at once.
fn post(
  url: &str,
  body: &Value,
  api_key: &str,
  limits: &Limits,
  sleep: &mut dyn FnMut(Duration),
) -> Result<JevReply, String> {
  charliebot_cli_common::http::ensure_crypto_provider();
  let runtime = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
    .map_err(|error| format!("{url}: request failed: {error}"))?;
  runtime.block_on(async {
    // Redirects are not followed: an unexpected one surfaces as its status.
    let client = reqwest::Client::builder()
      .redirect(reqwest::redirect::Policy::none())
      .build()
      .map_err(|error| format!("{url}: request failed: {}", source_chain(&error)))?;
    match one_attempt(&client, url, body, api_key, limits.request_timeout).await {
      Attempt::Answered(payload) => Ok(JevReply { payload, retried_timeout: false }),
      Attempt::Failed(cause) => Err(format!("{url}: {cause}")),
      Attempt::RateLimited(_) => {
        sleep(limits.rate_limit_wait);
        retried(&client, url, body, api_key, false, limits.retry_request_timeout).await
      }
      Attempt::TimedOut(_) => retried(&client, url, body, api_key, true, limits.retry_request_timeout).await,
    }
  })
}

/// Take the call's single retry and finish it. An answer records whether the retry absorbed a timeout; any failure of
/// the retry attempt ends the call with an error that names the retry.
async fn retried(
  client: &reqwest::Client,
  url: &str,
  body: &Value,
  api_key: &str,
  absorbed_timeout: bool,
  request_timeout: Duration,
) -> Result<JevReply, String> {
  let cause = match one_attempt(client, url, body, api_key, request_timeout).await {
    Attempt::Answered(payload) => return Ok(JevReply { payload, retried_timeout: absorbed_timeout }),
    Attempt::Failed(cause) | Attempt::RateLimited(cause) | Attempt::TimedOut(cause) => cause,
  };
  Err(format!("{url}: the retry attempt failed: {cause}"))
}

/// One Jev request, bounded by `request_timeout`: send it, then read the body. A 429 and a timeout are reported apart
/// from every other failure, so the caller can retry only those two.
async fn one_attempt(
  client: &reqwest::Client,
  url: &str,
  body: &Value,
  api_key: &str,
  request_timeout: Duration,
) -> Attempt {
  let request = client.post(url).bearer_auth(api_key).json(body).timeout(request_timeout);
  let response = match request.send().await {
    Ok(response) => response,
    Err(error) => return failed_attempt(&error),
  };
  let status = response.status();
  let body_text = match response.text().await {
    Ok(body_text) => body_text,
    Err(error) => return failed_attempt(&error),
  };
  if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
    return Attempt::RateLimited(format!("HTTP {}: {body_text}", status.as_u16()));
  }
  if status != reqwest::StatusCode::OK {
    return Attempt::Failed(format!("HTTP {}: {body_text}", status.as_u16()));
  }
  match serde_json::from_str::<Value>(&body_text) {
    Ok(payload) if payload.is_object() => Attempt::Answered(payload),
    Ok(_) => Attempt::Failed(format!("the response is not one JSON object: {body_text}")),
    Err(_) => Attempt::Failed(format!("the response body is not JSON: {body_text}")),
  }
}

/// One failed attempt: a timeout when the reqwest error says so anywhere in its source chain, and another request
/// failure otherwise.
fn failed_attempt(error: &reqwest::Error) -> Attempt {
  let cause = format!("request failed: {}", source_chain(error));
  if timeout_in_chain(error) { Attempt::TimedOut(cause) } else { Attempt::Failed(cause) }
}

/// Whether a reqwest failure is the request timeout. The timeout fires while sending or while reading the body, and its
/// marker may sit anywhere in the source chain.
fn timeout_in_chain(error: &reqwest::Error) -> bool {
  let mut current: &(dyn std::error::Error + 'static) = error;
  loop {
    if let Some(reqwest_error) = current.downcast_ref::<reqwest::Error>()
      && reqwest_error.is_timeout()
    {
      return true;
    }
    match current.source() {
      Some(source) => current = source,
      None => return false,
    }
  }
}

/// Compact JSON: escaped non-ASCII, `", "` between items.
fn dumps_payload(payload: &Value) -> String {
  text_repr::json_dumps(payload, &text_repr::DEFAULT_JSON)
}

/// The `answers.kind.probabilities.<class>` number of one Jev response. A JSON boolean is not a number.
fn class_probability(url: &str, payload: &Value, class: &str) -> Result<f64, String> {
  payload
    .get("answers")
    .and_then(|answer| answer.get(CHOICE_QUESTION))
    .and_then(|question| question.get("probabilities"))
    .and_then(|probabilities| probabilities.get(class))
    .and_then(Value::as_f64)
    .ok_or_else(|| {
      format!(
        "{url}: the answer has no answers.{CHOICE_QUESTION}.probabilities.{class} number: {}",
        dumps_payload(payload)
      )
    })
}

/// The `answers.kind.choice` text of one Jev response.
fn choice_of<'a>(url: &str, payload: &'a Value) -> Result<&'a str, String> {
  payload
    .get("answers")
    .and_then(|answer| answer.get(CHOICE_QUESTION))
    .and_then(|question| question.get("choice"))
    .and_then(Value::as_str)
    .ok_or_else(|| {
      format!("{url}: the answer has no answers.{CHOICE_QUESTION}.choice text: {}", dumps_payload(payload))
    })
}

#[cfg(test)]
mod tests {
  use std::collections::VecDeque;
  use std::io::Write;
  use std::net::{TcpListener, TcpStream};
  use std::sync::atomic::{AtomicBool, Ordering};
  use std::sync::{Arc, Condvar, Mutex};

  use super::*;

  const INSTRUCTIONS: &str = "Which class of work is this?";
  const ROUTINE_RULE: &str = "Routine work: fixes, tests and short actions.";
  const CONTEXT_RULE: &str = "Context class: a first read, an understanding or a design.";
  const DESIGN_RULE: &str = "Design work: a new mechanism across modules.";
  const STRONG: [&str; 2] = ["strong-alpha", "strong-beta"];
  const POOL: &str = "pool-gamma";
  const DELTA: &str = "pool-delta";
  const SESSION: &str = "strong-alpha";
  const API_KEY: &str = "test-key";
  const DEFAULT_ROLES: &str =
    "roles:\n  implement: [routine, context]\n  quick-edit: [routine, context]\n  script-run: [routine, context]\n";

  /// The bounds of a call whose attempts answer: the production bound, which no answer reaches even under load, and a
  /// wait no test sleeps out for real.
  fn test_limits() -> Limits {
    Limits {
      request_timeout: Duration::from_secs(timeouts::ROUTER_JEV_REQUEST_TIMEOUT),
      retry_request_timeout: Duration::from_secs(timeouts::ROUTER_JEV_REQUEST_TIMEOUT),
      rate_limit_wait: Duration::from_millis(20),
    }
  }

  /// A reply delay far past every bound the timeout tests inject, so the attempt that asks for it times out at its
  /// bound under any load. The stub sleeps on its own thread; the test does not wait for it.
  fn past_the_timeout() -> Duration {
    Duration::from_secs(1)
  }

  /// Each test runs behind this ceiling so a hang fails the test instead of the suite.
  fn with_timeout<T: Send + 'static>(body: impl FnOnce() -> T + Send + 'static) -> T {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let handle = std::thread::spawn(body);
    loop {
      if handle.is_finished() {
        return handle.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic));
      }
      if std::time::Instant::now() >= deadline {
        panic!("the test exceeded its 10 s timeout");
      }
      std::thread::sleep(Duration::from_millis(2));
    }
  }

  /// A Jev endpoint that records each request body and answers from a scripted reply list.
  struct FakeJev {
    url: String,
    port: u16,
    requests: Arc<(Mutex<Vec<Value>>, Condvar)>,
    authorizations: Arc<Mutex<Vec<String>>>,
    replies: Arc<Mutex<VecDeque<(u16, Value)>>>,
    delays: Arc<Mutex<VecDeque<Duration>>>,
    stop: Arc<AtomicBool>,
    listener: TcpListener,
  }

  #[derive(Clone)]
  struct Shared {
    requests: Arc<(Mutex<Vec<Value>>, Condvar)>,
    authorizations: Arc<Mutex<Vec<String>>>,
    replies: Arc<Mutex<VecDeque<(u16, Value)>>>,
    delays: Arc<Mutex<VecDeque<Duration>>>,
  }

  impl FakeJev {
    fn start(replies: Vec<(u16, Value)>) -> FakeJev {
      let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
      let port = listener.local_addr().unwrap().port();
      let shared = Shared {
        requests: Arc::new((Mutex::new(Vec::new()), Condvar::new())),
        authorizations: Arc::new(Mutex::new(Vec::new())),
        replies: Arc::new(Mutex::new(replies.into())),
        delays: Arc::new(Mutex::new(VecDeque::new())),
      };
      let accept = listener.try_clone().unwrap();
      let stop = Arc::new(AtomicBool::new(false));
      let thread_shared = shared.clone();
      let thread_stop = stop.clone();
      std::thread::spawn(move || {
        for stream in accept.incoming() {
          if thread_stop.load(Ordering::Relaxed) {
            break;
          }
          let Ok(stream) = stream else { break };
          // The script is taken in accept order, the order the router connected in, so a timed-out attempt keeps its
          // own reply and delay even when its serve thread reads the request late. One thread per connection, so a
          // delayed reply does not hold back the retried attempt.
          let scripted = thread_shared.replies.lock().unwrap().pop_front();
          let delay = thread_shared.delays.lock().unwrap().pop_front().unwrap_or_default();
          let shared = thread_shared.clone();
          std::thread::spawn(move || serve(stream, &shared, scripted, delay));
        }
      });
      FakeJev {
        url: format!("http://127.0.0.1:{port}/v1/systemone"),
        port,
        requests: shared.requests,
        authorizations: shared.authorizations,
        replies: shared.replies,
        delays: shared.delays,
        stop,
        listener,
      }
    }

    fn requests(&self) -> Vec<Value> {
      self.requests.0.lock().unwrap().clone()
    }

    /// The requests recorded so far, after waiting up to 1 s for `count`. A timed-out attempt's serve thread records
    /// its request after the call returns, so a test that counts requests waits instead of reading at once.
    fn wait_for_requests(&self, count: usize) -> Vec<Value> {
      let requests = self.requests.0.lock().unwrap();
      let (requests, _) =
        self.requests.1.wait_timeout_while(requests, Duration::from_secs(1), |r| r.len() < count).unwrap();
      requests.clone()
    }

    fn authorizations(&self) -> Vec<String> {
      self.authorizations.lock().unwrap().clone()
    }

    /// The scripted replies not yet served: a test that expects a failure consumes none.
    fn unserved(&self) -> usize {
      self.replies.lock().unwrap().len()
    }

    /// Make the next scripted reply arrive after `delay`, so the attempt that asks for it hits the request timeout.
    fn delay_next(&self, delay: Duration) {
      self.delays.lock().unwrap().push_back(delay);
    }
  }

  impl Drop for FakeJev {
    fn drop(&mut self) {
      // The accept loop holds its own listener clone, so closing alone would not end it: raise the
      // stop flag and open one throwaway connection to wake the blocked accept.
      self.stop.store(true, Ordering::Relaxed);
      let _ = TcpStream::connect(("127.0.0.1", self.port));
      drop(self.listener.try_clone().unwrap());
    }
  }

  /// One request = one connection: the stub answers with `Connection: close`.
  fn serve(mut stream: TcpStream, shared: &Shared, scripted: Option<(u16, Value)>, delay: Duration) {
    let mut buffered = Vec::new();
    let header_end = loop {
      let mut chunk = [0u8; 4096];
      let Ok(read) = stream.read(&mut chunk) else { return };
      if read == 0 {
        return;
      }
      buffered.extend_from_slice(&chunk[..read]);
      if let Some(position) = buffered.windows(4).position(|window| window == b"\r\n\r\n") {
        break position + 4;
      }
    };
    let head = String::from_utf8_lossy(&buffered[..header_end]).into_owned();
    let content_length = head
      .lines()
      .find_map(|line| {
        let lowered = line.to_ascii_lowercase();
        lowered.strip_prefix("content-length:").and_then(|value| value.trim().parse::<usize>().ok())
      })
      .unwrap_or(0);
    let mut body = buffered[header_end..].to_vec();
    while body.len() < content_length {
      let mut chunk = [0u8; 4096];
      let Ok(read) = stream.read(&mut chunk) else { return };
      if read == 0 {
        break;
      }
      body.extend_from_slice(&chunk[..read]);
    }
    let request: Value = serde_json::from_slice(&body).expect("the router sends one JSON object");
    shared.requests.0.lock().unwrap().push(request);
    shared.requests.1.notify_all();
    let authorization = head
      .lines()
      .find_map(|line| {
        if line.to_ascii_lowercase().starts_with("authorization:") {
          Some(line["authorization:".len()..].trim().to_string())
        } else {
          None
        }
      })
      .unwrap_or_default();
    shared.authorizations.lock().unwrap().push(authorization);
    let (status, reply) = scripted.expect("the test scripted one reply per request");
    std::thread::sleep(delay);
    let reply_body = serde_json::to_vec(&reply).unwrap();
    let response = format!(
      "HTTP/1.1 {status} Answer\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
      reply_body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.write_all(&reply_body);
    let _ = stream.flush();
  }

  /// A Jev answer naming `choice`, with the probability of each of the two default classes.
  fn jev_reply(choice: &str, routine: f64, context: f64) -> (u16, Value) {
    (
      200,
      json!({ "answers": { "kind": { "choice": choice, "probabilities": { "routine": routine, "context": context } } } }),
    )
  }

  struct TestHome {
    home: PathBuf,
    _temp: tempfile::TempDir,
  }

  impl TestHome {
    fn new() -> TestHome {
      let temp = tempfile::tempdir().unwrap();
      let home = TestHome { home: temp.path().to_path_buf(), _temp: temp };
      write_credentials(&home, "jev");
      write_config(&home);
      home
    }

    fn rules_path(&self) -> PathBuf {
      self.home.join(ROUTER_FILENAME)
    }
  }

  fn write_credentials(home: &TestHome, section: &str) {
    std::fs::write(home.home.join("credentials.yaml"), format!("{section}:\n  api_key: {API_KEY}\n")).unwrap();
  }

  /// The live `config.yaml`: the codex options, and the rotate pools the rules may name over them.
  fn write_config(home: &TestHome) {
    let options: String = STRONG
      .iter()
      .map(|id| format!("    - {{id: {id}, label: {id}, type: codex, model: synthetic-model}}\n"))
      .collect();
    let members = STRONG.join(", ");
    let item = |id: &str| format!("    - {{id: {id}, label: {id}, rule: rotate, members: [{members}]}}\n");
    let pools = format!("  pools:\n{}{}", item(POOL), item(DELTA));
    std::fs::write(home.home.join("config.yaml"), format!("backends:\n  options:\n{options}{pools}")).unwrap();
  }

  fn rules_text(url: &str, classes: &str, roles: &str) -> String {
    format!(
      "jev:\n  url: {url}\n  model: jev-test\n  credential: jev\ninstructions: \"{INSTRUCTIONS}\"\n{classes}{roles}"
    )
  }

  /// The two-class rules of the example: routine runs on the pool, context on the session.
  fn default_classes() -> String {
    let routine = format!("  routine: {{when: \"{ROUTINE_RULE}\", run_on: {POOL}}}\n");
    format!("classes:\n{routine}  context: {{when: \"{CONTEXT_RULE}\", run_on: session}}\n")
  }

  fn write_rules(home: &TestHome, url: &str, classes: &str, roles: &str) {
    std::fs::write(home.rules_path(), rules_text(url, classes, roles)).unwrap();
  }

  fn request(role: &str, text: &str, candidates: &[&str], session_backend: Option<&str>) -> String {
    let entries: Vec<Value> =
      candidates.iter().map(|identifier| json!({ "id": identifier, "cooling_until": null })).collect();
    serde_json::to_string(&json!({
      "role": role,
      "text": text,
      "candidates": entries,
      "session_backend": session_backend,
    }))
    .unwrap()
  }

  /// The first attempt's bound in the timeout tests. It is short because a timed-out attempt pays it in full, and the
  /// two-timeout test pays it twice. The timed-out attempt's reply waits [`past_the_timeout`], so only the bound decides
  /// the timeout.
  const TIMED_OUT_ATTEMPT_BOUND: Duration = Duration::from_millis(20);

  /// Limits for a call whose first attempt times out. The retry keeps the generous bound, so the retry's answer, or
  /// its 429, never loses to the short bound that makes the timeout cheap.
  fn timeout_limits() -> Limits {
    Limits { request_timeout: TIMED_OUT_ATTEMPT_BOUND, ..test_limits() }
  }

  fn run(home: &TestHome, payload: &str, jev: &FakeJev, replies: Vec<(u16, Value)>) -> Result<Value, String> {
    run_with_limits(home, payload, jev, replies, &test_limits())
  }

  fn run_with_limits(
    home: &TestHome,
    payload: &str,
    jev: &FakeJev,
    replies: Vec<(u16, Value)>,
    limits: &Limits,
  ) -> Result<Value, String> {
    jev.replies.lock().unwrap().extend(replies);
    let mut pauses = Vec::new();
    let result = route_in_home(&home.home, &home.rules_path(), payload, limits, &mut |pause| pauses.push(pause));
    if result.is_ok() {
      assert_eq!(pauses, Vec::<Duration>::new(), "a success path sleeps no rate-limit wait");
    }
    result
  }

  #[test]
  fn context_class_answers_the_session_backend_at_the_probability_jev_gave_it() {
    with_timeout(|| {
      let home = TestHome::new();
      let jev = FakeJev::start(Vec::new());
      write_rules(&home, &jev.url, &default_classes(), DEFAULT_ROLES);

      let payload = request("implement", "Understand the caching layer.", &[STRONG[0], POOL], Some(SESSION));
      let answer = run(&home, &payload, &jev, vec![jev_reply("context", 0.19, 0.81)]).unwrap();

      assert_eq!(answer, json!({ "routed": true, "backends": [SESSION], "reason": "context p=0.81" }));
      let requests = jev.requests();
      assert_eq!(requests.len(), 1);
      let body = &requests[0];
      assert_eq!(body["model"], "jev-test");
      assert_eq!(body["state"], "Task type: implement\n\nUnderstand the caching layer.");
      assert_eq!(body["questions"]["kind"]["type"], "choice");
      assert_eq!(body["questions"]["kind"]["instructions"], INSTRUCTIONS);
      assert_eq!(
        body["questions"]["kind"]["criteria"],
        json!({ "context": CONTEXT_RULE, "routine": ROUTINE_RULE }),
        "the role's classes are the criteria"
      );
      assert_eq!(jev.authorizations(), vec![format!("Bearer {API_KEY}")]);
    });
  }

  #[test]
  fn routine_work_answers_its_pool_id_as_the_one_backend() {
    with_timeout(|| {
      let home = TestHome::new();
      let jev = FakeJev::start(Vec::new());
      write_rules(&home, &jev.url, &default_classes(), DEFAULT_ROLES);

      let payload = request("implement", "Add a test for the parser.", &[STRONG[0], POOL], Some(SESSION));
      let answer = run(&home, &payload, &jev, vec![jev_reply("routine", 0.88, 0.12)]).unwrap();

      assert_eq!(answer, json!({ "routed": true, "backends": [POOL], "reason": "routine p=0.88" }));
    });
  }

  /// Acceptance ④: `run_on` takes a `backends.options` id as well as a pool id, and the answer routes there.
  #[test]
  fn a_run_on_naming_a_model_id_loads_and_routes_there() {
    with_timeout(|| {
      let home = TestHome::new();
      let jev = FakeJev::start(Vec::new());
      let classes = format!(
        "classes:\n  routine: {{when: \"{ROUTINE_RULE}\", run_on: {}}}\n  context: {{when: \"{CONTEXT_RULE}\", run_on: session}}\n",
        STRONG[1]
      );
      write_rules(&home, &jev.url, &classes, DEFAULT_ROLES);

      let payload = request("implement", "Add a test for the parser.", &STRONG, Some(SESSION));
      let answer = run(&home, &payload, &jev, vec![jev_reply("routine", 0.88, 0.12)]).unwrap();

      assert_eq!(answer, json!({ "routed": true, "backends": [STRONG[1]], "reason": "routine p=0.88" }));
    });
  }

  #[test]
  fn a_run_on_naming_a_missing_pool_id_is_refused_when_the_rules_load() {
    with_timeout(|| {
      let home = TestHome::new();
      // The config declares the pools {POOL, DELTA} and none named `pool`, so `run_on: pool` names no id.
      let options: String = STRONG
        .iter()
        .map(|id| format!("    - {{id: {id}, label: {id}, type: codex, model: synthetic-model}}\n"))
        .collect();
      std::fs::write(home.home.join("config.yaml"), format!("backends:\n  options:\n{options}")).unwrap();
      let jev = FakeJev::start(Vec::new());
      let classes = format!(
        "classes:\n  routine: {{when: \"{ROUTINE_RULE}\", run_on: pool}}\n  context: {{when: \"{CONTEXT_RULE}\", run_on: session}}\n"
      );
      write_rules(&home, &jev.url, &classes, DEFAULT_ROLES);

      let payload = request("implement", "Add a test for the parser.", &[STRONG[0], "pool"], Some(SESSION));
      let error = run(&home, &payload, &jev, Vec::new()).unwrap_err();

      assert!(error.contains("classes.routine.run_on") && error.contains("'pool'"), "{error}");
      assert!(error.contains("write session, one model id or one pool id"), "{error}");
    });
  }

  #[test]
  fn a_backend_id_list_in_run_on_exits_non_zero_naming_the_pools_form() {
    with_timeout(|| {
      let home = TestHome::new();
      let jev = FakeJev::start(Vec::new());
      let routine = format!("  routine: {{when: \"{ROUTINE_RULE}\", run_on: {POOL}}}\n");
      let classes =
        format!("classes:\n{routine}  design: {{when: \"{DESIGN_RULE}\", run_on: [{}, {}]}}\n", STRONG[1], STRONG[0]);
      write_rules(&home, &jev.url, &classes, "roles:\n  implement: [routine, design]\n");

      let payload = request("implement", "Design a new caching layer.", &[STRONG[0], POOL, STRONG[1]], None);
      let error = run(&home, &payload, &jev, Vec::new()).unwrap_err();

      assert!(error.contains("classes.design.run_on") && error.contains("a list of backend ids is retired"), "{error}");
      assert!(error.contains("write session, one model id or one pool id"), "{error}");
    });
  }

  #[test]
  fn a_tie_for_the_top_probability_takes_the_class_the_role_lists_first() {
    with_timeout(|| {
      let home = TestHome::new();
      let jev = FakeJev::start(Vec::new());
      write_rules(&home, &jev.url, &default_classes(), "roles:\n  implement: [context, routine]\n");

      let payload = request("implement", "Read the caching layer.", &[STRONG[0]], Some(SESSION));
      // Jev names routine, but context is listed first and ties at the top.
      let answer = run(&home, &payload, &jev, vec![jev_reply("routine", 0.5, 0.5)]).unwrap();

      assert_eq!(answer, json!({ "routed": true, "backends": [SESSION], "reason": "context p=0.50" }));
    });
  }

  #[test]
  fn a_session_target_without_a_session_backend_exits_non_zero_naming_the_field() {
    with_timeout(|| {
      let home = TestHome::new();
      let jev = FakeJev::start(Vec::new());
      write_rules(&home, &jev.url, &default_classes(), DEFAULT_ROLES);

      let payload = request("implement", "Understand the caching layer.", &[STRONG[0]], None);
      let error = run(&home, &payload, &jev, vec![jev_reply("context", 0.2, 0.8)]).unwrap_err();

      assert!(error.contains("classes.context.run_on"), "{error}");
      assert!(error.contains("session_backend"), "{error}");
    });
  }

  #[test]
  fn a_one_class_role_answers_its_class_without_a_jev_call() {
    with_timeout(|| {
      let home = TestHome::new();
      let jev = FakeJev::start(Vec::new());
      write_rules(&home, &jev.url, &default_classes(), "roles:\n  quick-edit: [context]\n  script-run: [routine]\n");

      let session = request("quick-edit", "Read the page.", &[STRONG[0], POOL], Some(SESSION));
      let session_answer = run(&home, &session, &jev, Vec::new()).unwrap();
      let pool = request("script-run", "Run the script.", &[STRONG[0], POOL], Some(SESSION));
      let pool_answer = run(&home, &pool, &jev, Vec::new()).unwrap();

      assert_eq!(session_answer, json!({ "routed": true, "backends": [SESSION], "reason": "context p=1.00" }));
      assert_eq!(pool_answer, json!({ "routed": true, "backends": [POOL], "reason": "routine p=1.00" }));
      assert!(jev.requests().is_empty(), "a one-class role makes no call");
    });
  }

  #[test]
  fn a_role_absent_from_the_rules_answers_not_routed_without_a_jev_call() {
    with_timeout(|| {
      let home = TestHome::new();
      let jev = FakeJev::start(Vec::new());
      write_rules(&home, &jev.url, &default_classes(), DEFAULT_ROLES);

      let payload = request("verify", "Confirm the change.", &[STRONG[0]], Some(SESSION));
      let answer = run(&home, &payload, &jev, Vec::new()).unwrap();

      assert_eq!(answer, json!({ "routed": false, "backends": [], "reason": "verify not routed" }));
      assert!(jev.requests().is_empty(), "an absent role makes no call");
    });
  }

  #[test]
  fn an_old_form_rules_file_exits_non_zero_naming_the_field() {
    with_timeout(|| {
      let home = TestHome::new();
      let jev = FakeJev::start(Vec::new());
      std::fs::write(
        home.rules_path(),
        format!(
          "jev:\n  url: {}\n  model: jev-test\n  credential: jev\ninstructions: \"{INSTRUCTIONS}\"\n\
           routine: \"{ROUTINE_RULE}\"\ndesign: \"{DESIGN_RULE}\"\nstrong:\n  - {}\n",
          jev.url, STRONG[0]
        ),
      )
      .unwrap();

      let payload = request("implement", "Add a test for the parser.", &STRONG, None);
      let error = run(&home, &payload, &jev, Vec::new()).unwrap_err();

      assert!(error.contains(&paths::display(&home.rules_path())), "{error}");
      assert!(error.contains("routine"), "{error}");
    });
  }

  #[test]
  fn an_unknown_class_in_a_role_exits_non_zero_naming_the_role_and_class() {
    with_timeout(|| {
      let home = TestHome::new();
      let jev = FakeJev::start(Vec::new());
      write_rules(&home, &jev.url, &default_classes(), "roles:\n  implement: [routine, ghost]\n");

      let payload = request("implement", "Add a test for the parser.", &STRONG, None);
      let error = run(&home, &payload, &jev, Vec::new()).unwrap_err();

      assert!(error.contains("roles.implement[1]"), "{error}");
      assert!(error.contains("'ghost'"), "{error}");
    });
  }

  #[test]
  fn an_unknown_name_in_run_on_exits_non_zero_naming_the_pools_form() {
    with_timeout(|| {
      let home = TestHome::new();
      let jev = FakeJev::start(Vec::new());
      let routine = format!("  routine: {{when: \"{ROUTINE_RULE}\", run_on: {POOL}}}\n");
      let classes = format!("classes:\n{routine}  context: {{when: \"{CONTEXT_RULE}\", run_on: ghost-pool}}\n");
      write_rules(&home, &jev.url, &classes, DEFAULT_ROLES);

      let payload = request("implement", "Add a test for the parser.", &STRONG, None);
      let error = run(&home, &payload, &jev, Vec::new()).unwrap_err();

      assert!(error.contains("classes.context.run_on") && error.contains("ghost-pool"), "{error}");
      assert!(error.contains("write session, one model id or one pool id"), "{error}");
    });
  }

  #[test]
  fn the_config_flag_reads_the_given_rules_file_and_the_home_credentials() {
    with_timeout(|| {
      let home = TestHome::new();
      let jev = FakeJev::start(Vec::new());
      let elsewhere = tempfile::tempdir().unwrap();
      let rules = elsewhere.path().join("calibration.yaml");
      std::fs::write(&rules, rules_text(&jev.url, &default_classes(), DEFAULT_ROLES)).unwrap();
      assert!(!home.rules_path().exists(), "the home holds no router.yaml");

      let payload = request("implement", "Understand the caching layer.", &STRONG, Some(SESSION));
      jev.replies.lock().unwrap().push_back(jev_reply("context", 0.19, 0.81));
      let answer = route_in_home(&home.home, &rules, &payload, &test_limits(), &mut |_| {});

      assert_eq!(answer.unwrap(), json!({ "routed": true, "backends": [SESSION], "reason": "context p=0.81" }));
      assert_eq!(jev.authorizations(), vec![format!("Bearer {API_KEY}")]);
    });
  }

  #[test]
  fn delegation_text_is_cut_to_its_first_characters() {
    with_timeout(|| {
      let home = TestHome::new();
      let jev = FakeJev::start(Vec::new());
      write_rules(&home, &jev.url, &default_classes(), DEFAULT_ROLES);
      let long_text = "x".repeat(TEXT_LIMIT + 1);
      let label = "Task type: implement\n\n";

      let delegation = request("implement", &long_text, &STRONG, None);
      run(&home, &delegation, &jev, vec![jev_reply("routine", 0.9, 0.1)]).unwrap();

      let requests = jev.requests();
      let request = &requests[0];
      let delegation_state = request["state"].as_str().unwrap();
      assert_eq!(delegation_state.chars().count(), TEXT_LIMIT);
      assert_eq!(delegation_state, format!("{label}{}", "x".repeat(TEXT_LIMIT - label.len())));
    });
  }

  #[test]
  fn thread_text_is_cut_to_its_last_characters() {
    with_timeout(|| {
      let home = TestHome::new();
      let jev = FakeJev::start(Vec::new());
      write_rules(
        &home,
        &jev.url,
        &default_classes(),
        "roles:\n  implement: [routine, context]\n  thread: [routine, context]\n",
      );
      let long_text = format!("{}THREAD-END", "x".repeat(TEXT_LIMIT + 1));
      let thread = request("thread", &long_text, &STRONG, None);
      run(&home, &thread, &jev, vec![jev_reply("routine", 0.9, 0.1)]).unwrap();

      let thread_state = jev.requests()[0]["state"].as_str().unwrap().to_string();
      let expected_tail: String = long_text.chars().skip(long_text.chars().count() - TEXT_LIMIT).collect();
      assert_eq!(thread_state, expected_tail);
      assert_eq!(thread_state.chars().count(), TEXT_LIMIT);
    });
  }

  #[test]
  fn the_server_sends_the_expected_jev_input_for_one_spec() {
    with_timeout(|| {
      let home = TestHome::new();
      let jev = FakeJev::start(Vec::new());
      write_rules(&home, &jev.url, &default_classes(), DEFAULT_ROLES);
      let spec = "Goal: explain the routing contract.\n\n## Requester Words\n- \"keep it simple\"";

      // The server sends the delegation's description with its role and the manager's backend.
      let server_request = request("implement", spec, &STRONG, Some(SESSION));
      let answer = run(&home, &server_request, &jev, vec![jev_reply("context", 0.3, 0.7)]).unwrap();

      assert_eq!(answer, json!({ "routed": true, "backends": [SESSION], "reason": "context p=0.70" }));
      let requests = jev.requests();
      assert_eq!(requests.len(), 1);
      assert_expected_spec_request(&requests[0], spec);
    });
  }

  #[test]
  fn the_replay_sends_the_expected_jev_input_for_one_spec() {
    with_timeout(|| {
      let home = TestHome::new();
      let jev = FakeJev::start(Vec::new());
      write_rules(&home, &jev.url, &default_classes(), DEFAULT_ROLES);
      let spec = "Goal: explain the routing contract.\n\n## Requester Words\n- \"keep it simple\"";
      let request = request("implement", spec, &STRONG, Some(SESSION));

      // A calibration replay reads the same spec and role from another rules file under --config.
      let replay_dir = tempfile::tempdir().unwrap();
      let replay_rules = replay_dir.path().join("router.yaml");
      std::fs::write(&replay_rules, rules_text(&jev.url, &default_classes(), DEFAULT_ROLES)).unwrap();
      jev.replies.lock().unwrap().push_back(jev_reply("context", 0.3, 0.7));
      let answer = route_in_home(&home.home, &replay_rules, &request, &test_limits(), &mut |_| {}).unwrap();

      assert_eq!(answer, json!({ "routed": true, "backends": [SESSION], "reason": "context p=0.70" }));
      let requests = jev.requests();
      assert_eq!(requests.len(), 1);
      assert_expected_spec_request(&requests[0], spec);
    });
  }

  fn assert_expected_spec_request(request: &Value, spec: &str) {
    assert_eq!(
      request,
      &json!({
        "model": "jev-test",
        "state": format!("Task type: implement\n\n{spec}"),
        "questions": {
          "kind": {
            "type": "choice",
            "instructions": INSTRUCTIONS,
            "criteria": {"context": CONTEXT_RULE, "routine": ROUTINE_RULE},
          },
        },
      })
    );
  }

  #[test]
  fn a_request_without_session_backend_exits_non_zero_naming_the_field() {
    with_timeout(|| {
      let home = TestHome::new();
      let jev = FakeJev::start(Vec::new());
      write_rules(&home, &jev.url, &default_classes(), DEFAULT_ROLES);

      let payload = json!({ "role": "implement", "text": "Add a test.", "candidates": [] }).to_string();
      let error = run(&home, &payload, &jev, Vec::new()).unwrap_err();

      assert!(error.contains("stdin.session_backend"), "{error}");
    });
  }

  #[test]
  fn one_rate_limit_waits_then_returns_the_answer() {
    with_timeout(|| {
      let home = TestHome::new();
      let jev = FakeJev::start(Vec::new());
      write_rules(&home, &jev.url, &default_classes(), DEFAULT_ROLES);
      let limits = test_limits();

      let payload = request("implement", "Understand the caching layer.", &STRONG, Some(SESSION));
      jev
        .replies
        .lock()
        .unwrap()
        .extend(vec![(429, json!({ "error": "rate limited" })), jev_reply("context", 0.19, 0.81)]);
      let pauses = Arc::new(Mutex::new(Vec::new()));
      let recorded = pauses.clone();
      let result = route_in_home(&home.home, &home.rules_path(), &payload, &limits, &mut |pause| {
        recorded.lock().unwrap().push(pause)
      });

      assert_eq!(result.unwrap(), json!({ "routed": true, "backends": [SESSION], "reason": "context p=0.81" }));
      assert_eq!(jev.requests().len(), 2);
      assert_eq!(*pauses.lock().unwrap(), vec![limits.rate_limit_wait]);
    });
  }

  #[test]
  fn two_rate_limits_exit_non_zero_with_the_status_and_body() {
    with_timeout(|| {
      let home = TestHome::new();
      let jev = FakeJev::start(Vec::new());
      write_rules(&home, &jev.url, &default_classes(), DEFAULT_ROLES);

      let payload = request("implement", "Add a test for the parser.", &STRONG, None);
      let error = run(
        &home,
        &payload,
        &jev,
        vec![(429, json!({ "error": "rate limited" })), (429, json!({ "error": "still limited" }))],
      )
      .unwrap_err();

      assert!(error.contains("429"), "{error}");
      assert!(error.contains("still limited"), "{error}");
      assert_eq!(jev.unserved(), 0);
    });
  }

  #[test]
  fn a_timed_out_attempt_is_retried_once_and_the_answer_records_it() {
    with_timeout(|| {
      let home = TestHome::new();
      let jev = FakeJev::start(Vec::new());
      write_rules(&home, &jev.url, &default_classes(), DEFAULT_ROLES);

      let payload = request("implement", "Understand the caching layer.", &STRONG, Some(SESSION));
      jev.delay_next(past_the_timeout());
      let answer = run_with_limits(
        &home,
        &payload,
        &jev,
        vec![jev_reply("context", 0.19, 0.81), jev_reply("context", 0.19, 0.81)],
        &timeout_limits(),
      )
      .unwrap();

      assert_eq!(
        answer,
        json!({ "routed": true, "backends": [SESSION], "reason": "context p=0.81; retried after a timeout" })
      );
      assert_eq!(jev.wait_for_requests(2).len(), 2, "the timeout takes the call's one retry");
      assert_eq!(jev.unserved(), 0);
    });
  }

  #[test]
  fn two_timeouts_exit_non_zero_naming_the_timeout_and_the_retry() {
    let home = TestHome::new();
    let jev = FakeJev::start(Vec::new());
    write_rules(&home, &jev.url, &default_classes(), DEFAULT_ROLES);

    let payload = request("implement", "Add a test for the parser.", &STRONG, None);
    jev.delay_next(past_the_timeout());
    jev.delay_next(past_the_timeout());
    // The retry times out as well, so both attempts pay the short bound.
    let both_short = Limits { retry_request_timeout: TIMED_OUT_ATTEMPT_BOUND, ..timeout_limits() };
    let error = run_with_limits(
      &home,
      &payload,
      &jev,
      vec![jev_reply("routine", 0.9, 0.1), jev_reply("routine", 0.9, 0.1)],
      &both_short,
    )
    .unwrap_err();

    assert!(error.contains("timed out"), "{error}");
    assert!(error.contains("retry"), "{error}");
    assert_eq!(jev.wait_for_requests(2).len(), 2, "the router makes no third attempt");
  }

  #[test]
  fn a_timeout_followed_by_a_rate_limit_exits_non_zero_without_a_third_request() {
    with_timeout(|| {
      let home = TestHome::new();
      let jev = FakeJev::start(Vec::new());
      write_rules(&home, &jev.url, &default_classes(), DEFAULT_ROLES);

      let payload = request("implement", "Add a test for the parser.", &STRONG, None);
      jev.delay_next(past_the_timeout());
      let error = run_with_limits(
        &home,
        &payload,
        &jev,
        vec![jev_reply("routine", 0.9, 0.1), (429, json!({ "error": "still limited" }))],
        &timeout_limits(),
      )
      .unwrap_err();

      assert!(error.contains("429"), "{error}");
      assert!(error.contains("still limited"), "{error}");
      assert!(error.contains("retry"), "{error}");
      assert_eq!(jev.wait_for_requests(2).len(), 2, "the 429 after a timeout takes no second retry");
      assert_eq!(jev.unserved(), 0);
    });
  }

  #[test]
  fn another_http_error_exits_non_zero_without_printing_an_answer() {
    with_timeout(|| {
      let home = TestHome::new();
      let jev = FakeJev::start(Vec::new());
      write_rules(&home, &jev.url, &default_classes(), DEFAULT_ROLES);

      let payload = request("implement", "Add a test for the parser.", &STRONG, None);
      let error = run(&home, &payload, &jev, vec![(500, json!({ "error": "gateway down" }))]).unwrap_err();

      assert!(error.contains("500"), "{error}");
      assert!(error.contains("gateway down"), "{error}");
    });
  }

  #[test]
  fn a_transport_failure_reports_the_cause_beyond_the_url_error() {
    with_timeout(|| {
      let home = TestHome::new();
      // A port nothing listens on: take one from the kernel, then release it.
      let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
      let port = listener.local_addr().unwrap().port();
      drop(listener);
      let url = format!("http://127.0.0.1:{port}/v1/systemone");
      write_rules(&home, &url, &default_classes(), DEFAULT_ROLES);

      let payload = request("implement", "Understand the caching layer.", &[STRONG[0]], Some(SESSION));
      let error = route_in_home(&home.home, &home.rules_path(), &payload, &test_limits(), &mut |_| {}).unwrap_err();

      assert!(error.contains("request failed:"), "{error}");
      assert!(error.contains(&format!("error sending request for url ({url})")), "the failure names the URL: {error}");
      assert!(error.contains("Connection refused"), "the failure names the cause, not only the URL: {error}");
    });
  }

  #[test]
  fn an_answer_without_a_class_probability_exits_non_zero() {
    with_timeout(|| {
      let home = TestHome::new();
      let jev = FakeJev::start(Vec::new());
      write_rules(&home, &jev.url, &default_classes(), DEFAULT_ROLES);

      let payload = request("implement", "Add a test for the parser.", &STRONG, Some(SESSION));
      let error = run(&home, &payload, &jev, vec![(200, json!({ "answers": { "other": {} } }))]).unwrap_err();

      assert!(error.contains("the answer has no answers.kind.probabilities.routine number"), "{error}");
    });
  }

  #[test]
  fn load_home_rules_reads_an_edit_on_the_next_call() {
    with_timeout(|| {
      let home = TestHome::new();
      let jev_url = "http://127.0.0.1:9/v1/systemone";
      let routine = format!("  routine: {{when: \"{ROUTINE_RULE}\", run_on: {POOL}}}\n");
      let classes =
        |target: &str| format!("classes:\n{routine}  context: {{when: \"{CONTEXT_RULE}\", run_on: {target}}}\n");
      write_rules(&home, jev_url, &classes(POOL), DEFAULT_ROLES);
      let first = load_home_rules(&home.home, &home.rules_path()).unwrap();
      assert!(matches!(&first.classes["context"].run_on, RunOn::Backend(id) if id == POOL));

      write_rules(&home, jev_url, &classes(DELTA), DEFAULT_ROLES);
      let second = load_home_rules(&home.home, &home.rules_path()).unwrap();
      assert!(matches!(&second.classes["context"].run_on, RunOn::Backend(id) if id == DELTA));
    });
  }

  #[test]
  fn a_malformed_rules_file_exits_non_zero_naming_the_file_and_field() {
    with_timeout(|| {
      let home = TestHome::new();
      let path = home.rules_path();
      std::fs::write(&path, "jev:\n  model: jev-test\n  credential: jev\nclasses: {}\nroles: {}\n").unwrap();

      let payload = request("implement", "Add a test for the parser.", &STRONG, None);
      let error = route_in_home(&home.home, &path, &payload, &test_limits(), &mut |_| {}).unwrap_err();

      assert!(error.contains(&paths::display(&path)), "{error}");
      assert!(error.contains("jev.url"), "{error}");
    });
  }

  #[test]
  fn rules_that_are_not_yaml_exit_non_zero_naming_the_file() {
    with_timeout(|| {
      let home = TestHome::new();
      let path = home.rules_path();
      std::fs::write(&path, "jev: [unclosed\n").unwrap();

      let payload = request("implement", "Add a test for the parser.", &STRONG, None);
      let error = route_in_home(&home.home, &path, &payload, &test_limits(), &mut |_| {}).unwrap_err();

      assert!(error.contains(&paths::display(&path)), "{error}");
    });
  }

  #[test]
  fn a_missing_rules_file_exits_non_zero_naming_the_file() {
    with_timeout(|| {
      let home = TestHome::new();
      let path = home.rules_path();

      let payload = request("implement", "Add a test for the parser.", &STRONG, None);
      let error = route_in_home(&home.home, &path, &payload, &test_limits(), &mut |_| {}).unwrap_err();

      assert!(error.contains(&paths::display(&path)), "{error}");
      assert!(error.contains("missing"), "{error}");
    });
  }

  #[test]
  fn a_missing_jev_key_exits_non_zero_naming_the_credentials_file() {
    with_timeout(|| {
      let home = TestHome::new();
      write_credentials(&home, "another-section");
      let jev = FakeJev::start(Vec::new());
      write_rules(&home, &jev.url, &default_classes(), DEFAULT_ROLES);

      let payload = request("implement", "Add a test for the parser.", &STRONG, Some(SESSION));
      let error = run(&home, &payload, &jev, Vec::new()).unwrap_err();

      assert!(error.contains("credentials.yaml"), "{error}");
      assert!(error.contains("jev.api_key"), "{error}");
      assert!(jev.requests().is_empty(), "no request reaches Jev without the key");
    });
  }

  #[test]
  #[ignore = "integration"]
  fn the_example_rules_file_loads_with_the_loader() {
    let example = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../../configs/router.example.yaml"));
    let rules = load_rules(example, &["cheap".to_string(), "claude-opus-5".to_string()]).unwrap();

    assert_eq!(rules.roles["implement"], ["routine", "context", "operations"]);
    assert_eq!(rules.roles.len(), 3);
    assert!(matches!(rules.classes["context"].run_on, RunOn::Session));
    let RunOn::Backend(operations_backend) = &rules.classes["operations"].run_on else {
      panic!("operations runs on a backend id");
    };
    assert_eq!(operations_backend.as_str(), "claude-opus-5");
  }

  #[test]
  fn the_worst_case_stays_inside_the_callers_twenty_five_seconds() {
    let worst_case = 2 * timeouts::ROUTER_JEV_REQUEST_TIMEOUT + timeouts::ROUTER_JEV_RATE_LIMIT_WAIT;
    assert!(worst_case < timeouts::ROUTER_TIMEOUT_SECONDS);
    assert_eq!(timeouts::ROUTER_JEV_REQUEST_TIMEOUT, 5);
  }
}
