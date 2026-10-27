//! The feature-owned subcommands `charliebot plan` and `charliebot artifact` (`src/features/artifacts/plan_cli.py`,
//! `src/features/artifacts/cli.py`). Deleting this package removes both subcommands with two dispatcher lines.

pub mod artifact;
pub mod plan;

mod config;
mod light_probe;
