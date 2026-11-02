# `charliebot-store`

The single writer for session metadata and chat event logs. `run_records` owns the run event line and its decodes.
`trigger_files` owns the trigger record and its file.

## Public API

- `run_records::event_line`
- `run_records::parse_event_line` and `run_records::read_event_lines_lossy`
- `run_records::decode_event_line`, `run_records::read_event_values` and `run_records::EventReadError`. The strict decode fails a bad line.
- `run_records::visit_events_from_end`, `run_records::NewestFirstEvents` and `run_records::EventScanStats`
- `trigger_files::TriggerStatus`
- `trigger_files::WatchTarget`
- `trigger_files::PendingTrigger`
- `trigger_files::fresh_uuid`
- `trigger_files::trigger_path`
- `trigger_files::read_trigger_value` and `trigger_files::TriggerReadError`
- `trigger_files::read_trigger`
- `trigger_files::write_trigger`
