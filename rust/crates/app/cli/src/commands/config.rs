//! The `config` verb: `get` prints one top-level config value on stdout.
//!
//! `config.yaml` holds the whole sectioned mapping and the loader is its only reader, so `get`
//! resolves the key through the same load, validation and defaults the server uses rather than
//! opening the file itself:
//!
//! ```text
//! charliebot config get <key>
//! ```
//!
//! Stdout carries exactly the value — a scalar as its plain text, lists, mappings and models as compact JSON — so a
//! caller can substitute `$(charliebot config get <key>)` into a command. Diagnostics go to stderr: an unknown key
//! exits 2, an unset (null) value exits 1. The nested keys the command reads are `server.public_url`, the local work
//! guard's `server.enable_local_work_guard`, and the runner's `remote_run.host` and `remote_run.remote_base`.

use std::ffi::OsString;

use charliebot_cli_common::args as cli_args;
use charliebot_cli_common::exit::CliExit;
use charliebot_cli_common::json_out;
use charliebot_core::config::{
  self, BackendOption, CallClass, CallTarget, CharlieBotConfig, OptionFieldKind, PoolConfig, PoolRule,
};
use charliebot_core::home;
use charliebot_core::paths;
use charliebot_registry::Registry;
use clap::{Parser, Subcommand};
use serde_json::{Map, Value, json};

#[derive(Parser)]
#[command(name = "config", about = "Read CharlieBot config values through the loader")]
struct ConfigCli {
  #[command(subcommand)]
  verb: ConfigVerb,
}

#[derive(Subcommand)]
enum ConfigVerb {
  /// Print a config key's value to stdout (nothing else)
  Get {
    /// Top-level CharlieBotConfig field or registered section name
    key: String,
  },
}

/// A `config get` failure: the stderr line and the exit code. These diagnostics are plain text, not the CLI's
/// shared JSON failure object.
#[derive(Debug)]
struct Failure {
  message: String,
  code: i32,
}

impl Failure {
  fn unknown_key(key: &str) -> Failure {
    Failure {
      message: format!("error: unknown config key: {key} (not a CharlieBotConfig field or registered section)"),
      code: 2,
    }
  }

  fn unset(key: &str) -> Failure {
    Failure { message: format!("error: config key {key} is unset"), code: 1 }
  }

  fn value(message: String) -> Failure {
    Failure { message: format!("error: {message}"), code: 1 }
  }
}

/// One resolved value: plain text, or a JSON value.
enum Rendered {
  Scalar(String),
  Json(Value),
}

/// Parse the arguments and run one verb. Parser errors print and exit on their own
/// (help exits 0, a usage error exits 2).
pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  let cli: ConfigCli = cli_args::parse("charliebot config", args);
  let ConfigVerb::Get { key } = cli.verb;

  let registry = charliebot_packages::registry().map_err(|error| CliExit::error(error.to_string()))?;
  // The key check takes the schema only: an unknown key is a usage error even when `config.yaml`
  // itself would fail to load.
  if !known_key(&key, &registry) {
    let failure = Failure::unknown_key(&key);
    eprintln!("{}", failure.message);
    std::process::exit(failure.code);
  }
  let home = home::charliebot_home_dir().map_err(|error| CliExit::error(error.to_string()))?;
  let cfg = registry.load_config(&home).map_err(|error| CliExit::error(error.to_string()))?;
  match get(&key, &cfg, &registry) {
    Ok(text) => {
      print!("{text}");
      Ok(())
    }
    Err(failure) => {
      eprintln!("{}", failure.message);
      std::process::exit(failure.code);
    }
  }
}

/// The top-level names `CharlieBotConfig` declares as fields.
const FIELD_KEYS: [&str; 8] =
  ["charliebot_home", "headless_chrome_bin", "server", "paths", "backends", "ui", "telegram", "remote_run"];

/// The home fact that is no config field: the state directory `CHARLIEBOT_HOME` selects when it is unset or
/// blank. [`charliebot_core::home`] owns the value; the launcher scripts read it the same way they read
/// `charliebot_home`.
const DEFAULT_HOME_KEY: &str = "default_charliebot_home";

/// The nested keys the command reads: the address a person reaches the server at, the local work guard's switch, and
/// the build-host keys the runner reads. The unset optionals are null.
const PUBLIC_URL_KEY: &str = "server.public_url";
const GUARD_SWITCH_KEY: &str = "server.enable_local_work_guard";
const RUNNER_HOST_KEY: &str = "remote_run.host";
const RUNNER_BASE_KEY: &str = "remote_run.remote_base";

/// True for a top-level field, a registered section, the default-home fact, or one of the nested keys.
fn known_key(key: &str, registry: &Registry) -> bool {
  key == DEFAULT_HOME_KEY
    || key == PUBLIC_URL_KEY
    || key == GUARD_SWITCH_KEY
    || key == RUNNER_HOST_KEY
    || key == RUNNER_BASE_KEY
    || FIELD_KEYS.contains(&key)
    || registry.config_sections().any(|section| section.key == key)
}

/// One key's stdout text, or the failure that exits.
fn get(key: &str, cfg: &CharlieBotConfig, registry: &Registry) -> Result<String, Failure> {
  render(key, value(key, cfg, registry)?)
}

/// The stdout text for one resolved value, or the failure that exits. `None` is a null value:
/// no top-level field or registered section of `CharlieBotConfig` at `f2b57d3f8` can be null (every field is
/// non-optional and every section defaults to a model), so the command's unset branch is the loader's safety net
/// for a future nullable key; the test calls this function directly because no `config.yaml` reaches it.
fn render(key: &str, value: Option<Rendered>) -> Result<String, Failure> {
  match value {
    None => Err(Failure::unset(key)),
    Some(Rendered::Scalar(text)) => Ok(format!("{text}\n")),
    Some(Rendered::Json(value)) => Ok(json_out::render(&value, None)),
  }
}

/// Resolve one key to its value. An unknown key is a usage error; a null value is unset.
fn value(key: &str, cfg: &CharlieBotConfig, registry: &Registry) -> Result<Option<Rendered>, Failure> {
  let rendered = match key {
    "charliebot_home" => Rendered::Scalar(paths::display(&cfg.charliebot_home)),
    "default_charliebot_home" => Rendered::Scalar(paths::display(&home::default_charliebot_home())),
    "headless_chrome_bin" => Rendered::Scalar(cfg.headless_chrome_bin.clone()),
    "server" => Rendered::Json(server_json(cfg)),
    PUBLIC_URL_KEY => match &cfg.server.public_url {
      Some(url) => Rendered::Scalar(url.clone()),
      None => Rendered::Json(Value::Null),
    },
    GUARD_SWITCH_KEY => Rendered::Scalar(cfg.server.enable_local_work_guard.to_string()),
    "remote_run" => Rendered::Json(remote_run_json(cfg)),
    RUNNER_HOST_KEY => optional_text(&cfg.remote_run.host),
    RUNNER_BASE_KEY => Rendered::Scalar(cfg.remote_run.remote_base.clone()),
    "paths" => Rendered::Json(paths_json(cfg)),
    "backends" => Rendered::Json(backends_json(cfg, registry).map_err(Failure::value)?),
    "ui" => Rendered::Json(ui_json(cfg)),
    "telegram" => Rendered::Json(telegram_json(cfg)),
    _ => {
      let Some(section) = registry.config_sections().find(|section| section.key == key) else {
        return Err(Failure::unknown_key(key));
      };
      let raw =
        cfg.section_value(key).ok_or_else(|| Failure::value(format!("config section {key:?} was not parsed")))?;
      Rendered::Json((section.dump)(raw).map_err(Failure::value)?)
    }
  };
  // `None` (unset) is what the command refuses to print, so a null section dump takes the same exit.
  Ok(match rendered {
    Rendered::Json(value) if value.is_null() => None,
    other => Some(other),
  })
}

fn server_json(cfg: &CharlieBotConfig) -> Value {
  let server = &cfg.server;
  json!({
    "host": server.host,
    "port": server.port,
    "subprocess_buffer_limit_mb": server.subprocess_buffer_limit_mb,
    "session_memory_max_mb": server.session_memory_max_mb,
    "session_swap_max_mb": server.session_swap_max_mb,
    "session_tasks_max": server.session_tasks_max,
    "min_free_disk_gib": server.min_free_disk_gib,
    "public_url": server.public_url,
  })
}

fn paths_json(cfg: &CharlieBotConfig) -> Value {
  let paths = &cfg.paths;
  json!({
    "workspace_dirs": paths.workspace_dirs,
    "worktree_dir": paths.worktree_dir,
    "scratch_dir": paths.scratch_dir,
  })
}

fn remote_run_json(cfg: &CharlieBotConfig) -> Value {
  json!({ "host": cfg.remote_run.host, "remote_base": cfg.remote_run.remote_base })
}

/// A set option as its text, an unset one as null: the caller reads the null as "unset".
fn optional_text(value: &Option<String>) -> Rendered {
  match value {
    Some(text) => Rendered::Scalar(text.clone()),
    None => Rendered::Json(Value::Null),
  }
}

fn ui_json(cfg: &CharlieBotConfig) -> Value {
  let services: Vec<Value> = cfg
    .ui
    .home_services
    .iter()
    .map(|service| json!({ "name": service.name, "description": service.description, "url": service.url }))
    .collect();
  json!({ "home_services": services })
}

fn telegram_json(cfg: &CharlieBotConfig) -> Value {
  json!({ "chat_id": cfg.telegram.chat_id })
}

/// `backends`, with each option rendered from its registered schema: the shared fields, then the type's own
/// fields in declaration order, each present whether the entry set it or takes the field's default.
fn backends_json(cfg: &CharlieBotConfig, registry: &Registry) -> Result<Value, String> {
  let backends = &cfg.backends;
  let routing = match &backends.routing {
    None => Value::Null,
    Some(routing) => json!({ "command": routing.command }),
  };
  let options = backends.options.iter().map(|option| option_json(option, registry)).collect::<Result<Vec<_>, _>>()?;
  let pools = backends.pools.iter().map(pool_json).collect::<Vec<_>>();
  let calls = CallClass::ALL
    .iter()
    .map(|class| {
      let target = match backends.calls.get(*class) {
        None => Value::Null,
        Some(CallTarget::Session) => json!(config::SESSION_CALL_TARGET),
        Some(CallTarget::Backend(id)) => json!(id),
      };
      (class.key().to_string(), target)
    })
    .collect::<Map<_, _>>();
  Ok(json!({ "routing": routing, "options": options, "pools": pools, "calls": calls }))
}

fn pool_json(pool: &PoolConfig) -> Value {
  let rule = match pool.rule {
    PoolRule::Priority => "priority",
    PoolRule::Rotate => "rotate",
  };
  let members = pool.members.iter().map(|member| member.backend.as_str()).collect::<Vec<_>>();
  json!({ "id": pool.id, "label": pool.label, "rule": rule, "members": members })
}

fn option_json(option: &BackendOption, registry: &Registry) -> Result<Value, String> {
  let schema = registry
    .registered_option_schema(&option.backend_type)
    .ok_or_else(|| format!("backend type {:?} registers no option schema", option.backend_type))?;
  let mut object = Map::new();
  object.insert("id".to_string(), Value::String(option.id.clone()));
  object.insert("label".to_string(), Value::String(option.label.clone()));
  object.insert("model".to_string(), Value::String(option.model.clone()));
  object.insert("prompt_overlay".to_string(), option.prompt_overlay.clone().map_or(Value::Null, Value::String));
  object.insert("billing".to_string(), option.billing.map_or(Value::Null, |billing| json!(billing.key())));
  object.insert("type".to_string(), Value::String(option.backend_type.clone()));
  for field in schema {
    let value = match option.type_fields.get(field.name) {
      Some(raw) => option_field_json(raw, field.kind)?,
      None => field.default.json().ok_or_else(|| {
        format!("backend {:?} omits the required {:?} option field {:?}", option.id, option.backend_type, field.name)
      })?,
    };
    object.insert(field.name.to_string(), value);
  }
  Ok(Value::Object(object))
}

fn option_field_json(raw: &serde_yaml::Value, kind: OptionFieldKind) -> Result<Value, String> {
  if raw.is_null() {
    return Ok(Value::Null);
  }
  match kind {
    OptionFieldKind::AsIs => config::yaml_to_json(raw),
    // A `float` field: an integer entry prints as a float (`1` prints `1.0`).
    OptionFieldKind::Float => {
      let number = raw.as_f64().ok_or_else(|| format!("expected a number, got {}", config::yaml_value_kind(raw)))?;
      serde_json::Number::from_f64(number)
        .map(Value::Number)
        .ok_or_else(|| "a non-finite number has no JSON form".to_string())
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A profile home holding the committed synthetic `config.yaml`.
  fn fixture_home() -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("a temp dir");
    std::fs::write(
      home.path().join(config::CONFIG_FILENAME),
      include_str!("../../tests/fixtures/config_get/config.yaml"),
    )
    .expect("write the fixture config");
    home
  }

  /// The recorded stdout, with the provenance header lines dropped.
  fn recorded(text: &str) -> String {
    let body: Vec<&str> = text.lines().filter(|line| !line.starts_with('#')).collect();
    format!("{}\n", body.join("\n"))
  }

  fn run(key: &str) -> Result<String, Failure> {
    let home = fixture_home();
    let registry = charliebot_packages::registry().expect("the packages register");
    let cfg = registry.load_config(home.path()).expect("the synthetic config loads");
    get(key, &cfg, &registry)
  }

  #[test]
  fn backends_prints_the_recorded_value() {
    let stdout = run("backends").expect("backends prints");
    let printed: Value = serde_json::from_str(stdout.trim_end()).expect("one JSON value");
    let expected_text = recorded(include_str!("../../tests/fixtures/config_get/backends_golden.txt"));
    let expected: Value = serde_json::from_str(expected_text.trim_end()).expect("the recorded Python line is JSON");
    assert_eq!(printed, expected);
    for key in ["pools", "calls", "routing", "options"] {
      assert!(printed.get(key).is_some(), "backends output lacks {key}");
    }
  }

  #[test]
  fn a_scalar_prints_as_str_prints_it() {
    let stdout = run("headless_chrome_bin").expect("the scalar prints");
    assert_eq!(stdout, recorded(include_str!("../../tests/fixtures/config_get/headless_chrome_bin_golden.txt")));
  }

  #[test]
  fn a_model_keeps_its_null_optional_fields() {
    let stdout = run("telegram").expect("telegram prints");
    let expected_text = recorded(include_str!("../../tests/fixtures/config_get/telegram_golden.txt"));
    assert_eq!(stdout, expected_text);
    assert_eq!(serde_json::from_str::<Value>(stdout.trim_end()).unwrap(), json!({ "chat_id": null }));
  }

  #[test]
  fn an_unknown_key_exits_2_naming_the_rule() {
    let failure = run("server.port").expect_err("a nested key is not a top-level key");
    assert_eq!(failure.code, 2);
    assert_eq!(
      failure.message,
      "error: unknown config key: server.port (not a CharlieBotConfig field or registered section)"
    );
  }

  /// The stdout of `key` against a profile home whose `config.yaml` is `yaml`.
  fn run_yaml(yaml: &str, key: &str) -> Result<String, Failure> {
    let (_home, registry, cfg) = load_yaml(yaml);
    get(key, &cfg, &registry)
  }

  fn load_yaml(yaml: &str) -> (tempfile::TempDir, Registry, CharlieBotConfig) {
    let home = tempfile::tempdir().expect("a temp dir");
    std::fs::write(home.path().join(config::CONFIG_FILENAME), yaml).expect("write the config");
    let registry = charliebot_packages::registry().expect("the packages register");
    let cfg = registry.load_config(home.path()).expect("the synthetic config loads");
    (home, registry, cfg)
  }

  #[test]
  fn a_public_url_prints_the_configured_address() {
    let yaml = "server:\n  public_url: https://synthetic.example.test:18498\n";
    let stdout = run_yaml(yaml, "server.public_url").expect("the address prints");
    assert_eq!(stdout, "https://synthetic.example.test:18498\n");
  }

  #[test]
  fn an_unset_public_url_exits_1_naming_the_key() {
    // The fixture sets no address, so the key is null and the command refuses to print it.
    let failure = run("server.public_url").expect_err("an unset address is refused");
    assert_eq!(failure.code, 1);
    assert_eq!(failure.message, "error: config key server.public_url is unset");
  }

  #[test]
  fn the_guard_switch_prints_its_boolean_and_the_runner_keys_print_their_values() {
    let yaml = "server:\n  enable_local_work_guard: false\nremote_run:\n  host: build-host\n";
    let (_home, registry, cfg) = load_yaml(yaml);
    assert_eq!(get("server.enable_local_work_guard", &cfg, &registry).expect("the switch prints"), "false\n");
    assert_eq!(get("remote_run.host", &cfg, &registry).expect("the host prints"), "build-host\n");
    assert_eq!(
      get("remote_run.remote_base", &cfg, &registry).expect("the default base prints"),
      "scripts/remote-run\n",
    );
    let printed: Value =
      serde_json::from_str(get("remote_run", &cfg, &registry).expect("the section prints").trim_end()).unwrap();
    assert_eq!(printed, json!({ "host": "build-host", "remote_base": "scripts/remote-run" }));
    let unset = "server:\n  enable_local_work_guard: false\n";
    let (_unset_home, unset_registry, unset_cfg) = load_yaml(unset);
    let failure = get("remote_run.host", &unset_cfg, &unset_registry).expect_err("an unset host is null");
    assert_eq!(failure.code, 1);
    assert_eq!(failure.message, "error: config key remote_run.host is unset");
  }

  #[test]
  fn an_unset_value_exits_1() {
    // No top-level field or registered section can hold null at f2b57d3f8 (the fields are non-optional and a
    // section defaults to a model), so the unset branch is exercised where the resolved value arrives.
    let failure = render("telegram", None).expect_err("an unset value is refused");
    assert_eq!(failure.code, 1);
    assert_eq!(failure.message, "error: config key telegram is unset");
  }
}
