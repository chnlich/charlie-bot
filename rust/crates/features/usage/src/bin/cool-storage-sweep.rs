//! One real run of the cold-storage sweep over a synthetic home, for `tests/cool_storage_differential.py`. The usage sources
//! are the backends' own registrations, read from the process environment as the server reads them. Every path a sweep
//! reads must resolve inside `<tree>`, or the run stops before its first deletion. Prints the summary line the scheduled
//! handler returns.

use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail};
use charliebot_agents::backends::claude_code::ClaudeHost;
use charliebot_agents::backends::codex::CodexHost;
use charliebot_agents::backends::{claude_code, codex, opencode};
use charliebot_core::home;
use charliebot_core::paths::realpath;
use charliebot_registry::Registry;
use charliebot_registry::store_hooks::store_hooks;
use charliebot_store::Store;
use charliebot_store::metadata::UtcDateTime;
use charliebot_usage_page::cool_storage::{format_sweep_line, run_cool_sweep};

const USAGE: &str = "usage: cool-storage-sweep <tree> <now, RFC 3339>";

fn main() -> anyhow::Result<()> {
  let mut args = env::args_os().skip(1);
  let tree = args.next().map(PathBuf::from).ok_or_else(|| anyhow!(USAGE))?;
  let now = args.next().ok_or_else(|| anyhow!(USAGE))?;
  let now = UtcDateTime::parse(now.to_str().ok_or_else(|| anyhow!("now is not UTF-8"))?)?;

  let mut registry = Registry::new();
  registry.register_package("src.backends.claude_code", claude_code::register)?;
  registry.register_package("src.backends.codex", codex::register)?;
  registry.register_package("src.backends.opencode", opencode::register_usage_source)?;
  let cfg = Arc::new(registry.load_config(&home::charliebot_home_dir()?)?);

  // The sandbox guard: every store the sweep reaches resolves inside the tree, checked before the first deletion.
  let claude = ClaudeHost::from_process()?;
  let codex_host = CodexHost::from_process()?;
  let stores =
    [cfg.charliebot_home.clone(), claude.home, claude.claude_config_dir, codex_host.codex_home(), home::opencode_db()?];
  for path in stores {
    assert_inside(&tree, &path)?;
  }

  let registry = Arc::new(registry);
  let store = Store::new((*cfg).clone(), registry.metadata_slots(), store_hooks(Arc::clone(&registry)))?;
  let categories = run_cool_sweep(cfg, &store, &registry, now)?;
  println!("{}", format_sweep_line(&categories));
  Ok(())
}

fn assert_inside(tree: &Path, path: &Path) -> anyhow::Result<()> {
  if realpath(path).starts_with(realpath(tree)) {
    return Ok(());
  }
  bail!("{} resolves outside the synthetic tree {}; nothing was swept", path.display(), tree.display())
}
