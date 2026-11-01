# charliebot-charlie-code

The `charlie-code` backend type: the CharlieCode CLI against an OpenAI-compatible endpoint. CLC's usage lives only
in CharlieBot's own run logs, so its usage source has no log reader.

## Public API

- `register`, `register_with_host` and `USAGE_SOURCE`
- `factory::CharlieCodeFactory`, which builds one backend per launch from the option entry
- `flags`, the CLI option names shared by the backend, the review command and the preflight check
- `options::{CharlieCodeOption, SCHEMA, option_model}`, the type-specific option fields and their bounds
- `API_KEY_ENV` and `TASK_FILE_NAME`; `CharlieCodeBackend` and the event translator stay private to the crate
