# charliebot-core

This crate owns server inputs and shared types used by the Rust port.

## Public API

| Rust item | Purpose |
| --- | --- |
| `venv::VenvBin`, `VenvBin::new`, and `VenvBin::path` | Validate and hold the `CHARLIEBOT_VENV_BIN` directory loaded by the server before its home writer fence. |
