# charliebot-core

This crate owns server inputs and shared types used by the Rust port.

## Public API

| Rust item | Purpose |
| --- | --- |
| `repo_root::RepoRoot`, `RepoRoot::new`, and `RepoRoot::path` | Validate and hold the checkout that `REPO_ROOT_ENV` names. `main.rs` loads it once, and the startup state shares it. |
| `git::run`, `git::head_sha`, and the git timeouts | The stripped stdout of a git command in the checkout, with a timeout. `buildinfo.py` and `templating.py` read the HEAD sha and commit date this way. |
