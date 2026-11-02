# charliebot-charlie-code

The `charlie-code` backend type: the CharlieCode CLI against an OpenAI-compatible endpoint. CLC's usage lives only
in CharlieBot's own run logs, so its usage source has no log reader.

## Public API

- `register`, `register_with_host` and `USAGE_SOURCE`
- `factory::CharlieCodeFactory`, which builds one backend per launch from the option entry
- `flags`, the CLI option names shared by the backend and the preflight check
- `options::{CharlieCodeOption, SCHEMA, option_model}`, the type-specific option fields and their bounds
- `CharlieCodeBackend`, `API_KEY_ENV`, `TASK_FILE_NAME` and the event translator stay private to the crate
