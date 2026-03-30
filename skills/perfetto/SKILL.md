---
name: perfetto
description: Use when the user wants to view, inspect, or share Perfetto/Chrome trace files -- triggered by words like "trace", "perf", "perfetto", "profiling", "trace viewer", or when dealing with trace JSON files from training runs.
version: 1.0.0
---

# Perfetto Trace Viewer

Generate links to the CharlieBot built-in Perfetto trace viewer, which can load and merge multiple rank traces into a single view.

## Endpoints

All URLs use `https://<hostname>:<port>` where `<hostname>` is the CharlieBot server hostname and `<port>` is `server_port` from `~/.charliebot/config.yaml`. Check HOST MEMORY for the actual values on the current host.

### View traces from a directory (most common)

```
https://<hostname>:<port>/perfetto?dir=<absolute-path>
```

Auto-discovers all `*.json` trace files in the directory, merges them client-side (labeling by rank), and loads into an embedded Perfetto UI.

Optional params:
- `&pattern=<glob>` -- file pattern to match (default: `*.json`)
- `&title=<text>` -- custom page title

### View specific trace files

```
https://<hostname>:<port>/perfetto?trace=/files/<path1>&trace=/files/<path2>
```

Pass one or more `trace=` params with `/files/`-prefixed absolute paths.

### Single trace

```
https://<hostname>:<port>/perfetto?trace=/files/<absolute-path>
```

## Examples

| Scenario | URL |
|---|---|
| All rank traces in a dir | `https://<hostname>:<port>/perfetto?dir=/data/checkpoints/foo/trace` |
| Only rank 0 | `https://<hostname>:<port>/perfetto?trace=/files/data/checkpoints/foo/trace/trace_rank000_step100.json` |
| Custom pattern | `https://<hostname>:<port>/perfetto?dir=/data/checkpoints/foo/trace&pattern=trace_rank*.json` |

## Rules

1. Always verify the trace directory/files exist before sharing the link (use `ls` or `Glob`)
2. Resolve hostname and port from HOST MEMORY or `~/.charliebot/config.yaml` — never hardcode them
3. For a directory with multiple rank traces, prefer `?dir=` over listing individual `?trace=` params
4. Present the link in markdown format: `[descriptive text](url)`
5. Note: only JSON traces can be merged; protobuf traces will show only the first file
