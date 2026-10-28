//! The `config get` command (`src/runtime/cli/config.py`): print one top-level config value on stdout.
//!
//! `config.yaml` holds the whole sectioned mapping and the loader is its only reader, so the command resolves the
//! key through the same load, validation and defaults the server uses rather than opening the file itself:
//!
//! ```text
//! charliebot config get <key>
//! ```
//!
//! Stdout carries exactly the value — scalars as Python's `print` writes them, lists, mappings and models as
//! `json.dumps(value, indent=None, ensure_ascii=False)` — so a caller can substitute
//! `$(charliebot config get <key>)` into a command. Diagnostics go to stderr: an unknown key exits 2, an unset
//! (null) value exits 1.

use std::ffi::OsString;

use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::exit::CliExit;
use charliebot_cli_common::json_out;
use charliebot_core::config::{self, BackendOption, CharlieBotConfig, OptionFieldKind};
use charliebot_core::home;
use charliebot_core::paths;
use charliebot_registry::Registry;
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use serde_json::{Map, Value, json};

#[derive(Parser)]
#[command(name = "config", about = "Read a CharlieBot config key through the loader")]
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

/// A `config get` failure: the stderr line and the exit code. The Python command writes these diagnostics as plain
/// text, not as the CLI's shared JSON failure object.
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

/// One resolved value: Python's `print` text, or a value `json.dumps(indent=None)` writes.
enum Rendered {
  Scalar(String),
  Json(Value),
}

/// Parse the arguments and print the key's value. Parser errors print and exit on their own
/// (argparse-compatible: help 0, usage error 2).
pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  let command = with_infer_long_args(ConfigCli::command());
  let argv = std::iter::once(OsString::from("charliebot config")).chain(args.iter().cloned());
  let matches = match command.try_get_matches_from(argv) {
    Ok(matches) => matches,
    Err(error) => error.exit(),
  };
  let cli = ConfigCli::from_arg_matches(&matches).expect("try_get_matches_from validated the arguments");
  let ConfigVerb::Get { key } = cli.verb;

  let registry = charliebot_packages::registry().map_err(|error| CliExit::error(error.to_string()))?;
  // The key check takes the schema only, as Python's does: an unknown key is a usage error even when
  // `config.yaml` itself would fail to load.
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
const FIELD_KEYS: [&str; 7] =
  ["charliebot_home", "headless_chrome_bin", "server", "paths", "backends", "ui", "telegram"];

/// True for a top-level field or a registered section — the names Python's `config get` accepts.
fn known_key(key: &str, registry: &Registry) -> bool {
  FIELD_KEYS.contains(&key) || registry.config_sections().any(|section| section.key == key)
}

/// One key's stdout text, or the failure that exits.
fn get(key: &str, cfg: &CharlieBotConfig, registry: &Registry) -> Result<String, Failure> {
  render(key, value(key, cfg, registry)?)
}

/// The stdout text for one resolved value, or the failure that exits. `None` is a value Python holds as `None`:
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
    "headless_chrome_bin" => Rendered::Scalar(cfg.headless_chrome_bin.clone()),
    "server" => Rendered::Json(server_json(cfg)),
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
  // `None` (unset) is what Python's command refuses to print, so a null section dump takes the same exit.
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

/// `backends`, with each option rendered as its pydantic model dumps it: the shared fields, then the type's own
/// fields in declaration order, each present whether the entry set it or takes the model's default.
fn backends_json(cfg: &CharlieBotConfig, registry: &Registry) -> Result<Value, String> {
  let backends = &cfg.backends;
  let routing = match &backends.routing {
    None => Value::Null,
    Some(routing) => json!({ "command": routing.command }),
  };
  let options = backends.options.iter().map(|option| option_json(option, registry)).collect::<Result<Vec<_>, _>>()?;
  Ok(json!({ "pool": backends.pool, "routing": routing, "options": options }))
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
    // A `float` field: pydantic dumps an integer entry as a float (`1` prints `1.0`).
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

  /// The recorded Python stdout, with the provenance header lines dropped.
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
  fn backends_prints_the_value_the_python_command_printed() {
    let stdout = run("backends").expect("backends prints");
    let printed: Value = serde_json::from_str(stdout.trim_end()).expect("one JSON value");
    let expected_text = recorded(include_str!("../../tests/fixtures/config_get/backends.python.txt"));
    let expected: Value = serde_json::from_str(expected_text.trim_end()).expect("the recorded Python line is JSON");
    assert_eq!(printed, expected);
    for key in ["pool", "routing", "options"] {
      assert!(printed.get(key).is_some(), "backends output lacks {key}");
    }
  }

  #[test]
  fn a_scalar_prints_as_python_prints_it() {
    let stdout = run("headless_chrome_bin").expect("the scalar prints");
    assert_eq!(stdout, recorded(include_str!("../../tests/fixtures/config_get/headless_chrome_bin.python.txt")));
  }

  #[test]
  fn a_model_keeps_its_null_optional_fields() {
    let stdout = run("telegram").expect("telegram prints");
    let expected_text = recorded(include_str!("../../tests/fixtures/config_get/telegram.python.txt"));
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

  #[test]
  fn an_unset_value_exits_1() {
    // No top-level field or registered section can hold null at f2b57d3f8 (the fields are non-optional and a
    // section defaults to a model), so the unset branch is exercised where the resolved value arrives.
    let failure = render("telegram", None).expect_err("an unset value is refused");
    assert_eq!(failure.code, 1);
    assert_eq!(failure.message, "error: config key telegram is unset");
  }
}
