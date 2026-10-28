# `charliebot-store`

The single writer for session metadata and chat event logs. `run_records` owns the run event line and its decodes.
`trigger_files` owns the trigger record and its file.

## Public API and Python sources

| Rust item | Python source |
| --- | --- |
| `run_records::event_line` | `src/runtime/worker.py:54-56` (`_event_line`) |
| `run_records::parse_event_line` and `run_records::read_event_lines_lossy` | `src/infra/ndjson.py:80-128` (`parse_ndjson_line`, its `errors="replace"` decode) |
| `run_records::decode_event_line`, `run_records::read_event_values` and `run_records::EventReadError` | No Python twin: the strict decode fails a bad line. Python's reader of the same log skips it, `src/runtime/worker_transcript.py:222-230` (`_read_events`) |
| `run_records::visit_events_from_end`, `run_records::NewestFirstEvents` and `run_records::EventScanStats` | `src/infra/ndjson.py:404-547` (`iter_ndjson_events_from_end`, `type_line_filter`) |
| `trigger_files::TriggerStatus` | `src/infra/models.py:84-88` |
| `trigger_files::WatchTarget` | `src/infra/models.py:312-315` |
| `trigger_files::PendingTrigger` | `src/infra/models.py:318-330` |
| `trigger_files::fresh_uuid` | `src/infra/models.py:318` (`PendingTrigger.id` default factory) |
| `trigger_files::trigger_path` | `src/runtime/triggers.py:1116-1117` (`_trigger_path`) |
| `trigger_files::read_trigger_value` and `trigger_files::TriggerReadError` | `src/runtime/triggers.py:1133-1137` (`_load_trigger`, its `json.loads`) |
| `trigger_files::read_trigger` | `src/runtime/triggers.py:1133-1137` (`_load_trigger`) |
| `trigger_files::write_trigger` | `src/runtime/triggers.py:1119-1131` (`_save_trigger`) |
