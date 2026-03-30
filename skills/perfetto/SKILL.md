---
name: perfetto
description: Use when the user wants to view, inspect, or share Perfetto/Chrome trace files -- triggered by words like "trace", "perf", "perfetto", "profiling", "trace viewer", or when dealing with trace JSON files from training runs.
version: 1.0.0
---

# Perfetto Trace Viewer

Generate links to the CharlieBot built-in Perfetto trace viewer, which can load and merge multiple rank traces into a single view.

## Endpoints

All URLs use `<base_url>` where `<base_url>` is the CharlieBot URL resolved from HOST MEMORY (look for the **CharlieBot URL** entry).

### View traces from a directory (most common)

```
<base_url>/perfetto?dir=<absolute-path>
```

Auto-discovers all `*.json` trace files in the directory, merges them client-side (labeling by rank), and loads into an embedded Perfetto UI.

Optional params:
- `&pattern=<glob>` -- file pattern to match (default: `*.json`)
- `&title=<text>` -- custom page title

### View specific trace files

```
<base_url>/perfetto?trace=/files/<path1>&trace=/files/<path2>
```

Pass one or more `trace=` params with `/files/`-prefixed absolute paths.

### Single trace

```
<base_url>/perfetto?trace=/files/<absolute-path>
```

## Examples

| Scenario | URL |
|---|---|
| All rank traces in a dir | `<base_url>/perfetto?dir=/data/checkpoints/foo/trace` |
| Only rank 0 | `<base_url>/perfetto?trace=/files/data/checkpoints/foo/trace/trace_rank000_step100.json` |
| Custom pattern | `<base_url>/perfetto?dir=/data/checkpoints/foo/trace&pattern=trace_rank*.json` |

## Rules

1. Always verify the trace directory/files exist before sharing the link (use `ls` or `Glob`)
2. Resolve base URL from HOST MEMORY — never hardcode hostnames or ports
3. For a directory with multiple rank traces, prefer `?dir=` over listing individual `?trace=` params
4. Present the link in markdown format: `[descriptive text](url)`
5. Note: only JSON traces can be merged; protobuf traces will show only the first file
