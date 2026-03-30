---
name: file-server
description: Use when you need to share a file or directory with the user via a clickable link. The CharlieBot server has a built-in file browser that serves any file on the host filesystem.
version: 1.0.0
---

# File Server

Share files and directories with the user by generating links to the CharlieBot file server. The server serves any file on the host filesystem via the `/files/` endpoint.

## URL Format

```
https://<hostname>:<port>/files/<absolute-path>
```

Where `<hostname>` is the CharlieBot server hostname and `<port>` is `server_port` from `~/.charliebot/config.yaml`. Check HOST MEMORY for the actual values on the current host.

The path after `/files/` is the absolute filesystem path (without leading `/`). Examples:

| Filesystem path | URL |
|---|---|
| `/data/checkpoints/foo/trace.json` | `https://<hostname>:<port>/files/data/checkpoints/foo/trace.json` |
| `/home/user/results/` | `https://<hostname>:<port>/files/home/user/results/` |

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
2. Resolve hostname and port from HOST MEMORY or `~/.charliebot/config.yaml` — never hardcode them
3. Present the link in markdown format: `[descriptive text](url)`
4. For Perfetto traces (`.json` files from training), remind the user to open them at https://ui.perfetto.dev
