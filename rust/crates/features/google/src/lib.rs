//! The `charliebot google` command group, run by `cli::main`: the account table in `credentials.yaml`, the consent
//! link and code exchange that store an account's refresh token, and the authenticated requests that mint an access
//! token for each account. The CLI dispatcher in `app/cli` calls `cli::main`.

pub mod cli;
