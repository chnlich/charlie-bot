---
name: file-server
description: Use when you need to share a file or directory with the user via a clickable link. The CharlieBot server has a built-in file browser that serves any file on the host filesystem.
version: 1.0.0
---

# File Server

Share files and directories with the user by generating links to the CharlieBot file server. The server serves any file on the host filesystem via the `/files/` endpoint.

## URL Format

```
<base_url>/files/<absolute-path>
```

Where `<base_url>` is the CharlieBot URL resolved from HOST MEMORY (look for the **CharlieBot URL** entry).

The path after `/files/` is the absolute filesystem path (without leading `/`). Examples:

| Filesystem path | URL |
|---|---|
| `/data/checkpoints/foo/trace.json` | `<base_url>/files/data/checkpoints/foo/trace.json` |
| `/home/user/results/` | `<base_url>/files/home/user/results/` |

## Behavior

- **File path**: returns the file for download (auto-detected MIME type)
- **Directory path**: returns an HTML directory listing with clickable entries

## When to Use

Whenever you need to present a file to the user (logs, traces, checkpoints, images, configs, etc.), generate the URL instead of dumping file contents into chat. This is especially useful for:

- Large files (traces, logs, pickles)
- Binary files (images, model checkpoints)
- Directories the user may want to browse

## Rules

1. Always verify the file/directory exists before sharing the link (use `ls` or `Glob`)
2. Resolve base URL from HOST MEMORY — never hardcode hostnames or ports
3. Present the link in markdown format: `[descriptive text](url)`
4. **If the file is a Perfetto/Chrome trace** (`.json` trace from training/profiling, or a directory of rank traces), ALWAYS also include a Perfetto viewer link alongside the file link. Read the `perfetto` skill for how to construct the viewer URL.

   Trace indicators: filename matches `trace_rank*.json` / `*trace*.json`, lives under a `trace/` or `profile/` dir, or the user called it a "trace"/"profile"/"perf capture".
