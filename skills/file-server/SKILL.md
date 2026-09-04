---
name: file-server
description: Must invoke when presenting a file to the user. The CharlieBot server has a built-in file browser that serves any file on the host filesystem.
version: 1.0.0
---

# File Server

Share files and directories with the user by generating links to the CharlieBot file server. The
server serves any file on the host filesystem.

## URL Format

```
<base_url>/absolute_filepath/<absolute-path>
```

Where `<base_url>` is the CharlieBot URL resolved from HOST MEMORY (look for the **CharlieBot URL**
entry).

The path after `/absolute_filepath/` is the absolute filesystem path with its leading `/` removed.
The prefix names what has to follow it, so a path that dropped its leading segments reads as wrong
where it is written. Examples:

| Filesystem path | URL |
|---|---|
| `/path/to/trace.json` | `<base_url>/absolute_filepath/path/to/trace.json` |
| `/path/to/results/` | `<base_url>/absolute_filepath/path/to/results/` |

The server answers on the alias `/files/` as well, which is the prefix links already sent carry and
the prefix the web UI builds its own URLs with.

## Publish Lane

Links that reach readers beyond the operator come from `charliebot publish <artifact-path>`: the
command copies the file into the publish directory and prints the published URL, and the Slack
reply path rewrites the file-server URLs of an outbound reply to published ones on its own. The
server-port links above serve the operator's own review in the browser and the chat embeds.

## Behavior

- **File path**: returns the file for download (auto-detected MIME type)
- **Directory path**: returns an HTML directory listing with clickable entries

## When to Use

Whenever you need to present a file to the user (logs, traces, checkpoints, images, configs, and so
on), generate the URL instead of dumping file contents into chat. This is especially useful for:

- Large files (traces, logs, pickles)
- Binary files (images, model checkpoints)
- Directories the user may want to browse

## Rules

1. Take the path in a link from the current turn's command output: run `ls` on the exact full path
   about to be pasted, rather than reconstructing it from memory.
2. Present the link in markdown format: `[descriptive text](url)`
3. **If the file is a Perfetto/Chrome trace** (`.json` trace from training/profiling, or a directory
   of rank traces), ALWAYS also include a Perfetto viewer link alongside the file link. Read the
   `perfetto` skill for how to construct the viewer URL.

   Trace indicators: filename matches `trace_rank*.json` / `*trace*.json`, lives under a `trace/` or
   `profile/` dir, or the user called it a "trace"/"profile"/"perf capture".
4. Write a raw filesystem path as plain text, `path:line`, since the CharlieBot UI renders a markdown
   link around a raw local path as a dead link. The form to wrap in `[descriptive text](url)` is a
   file-server URL.
5. An artifact shared beyond Chao (a Slack post, a group report) ships comment-disabled: the
   comment tray belongs to Chao's own review flow.

## HTML Artifacts

Generated HTML artifacts (reports, plans, dashboards) must satisfy:

- Full document with doctype, html, body tags.
- Self-contained: inline CSS/JS. External resources from `cdn.jsdelivr.net` or
  `unpkg.com` only.
- Sandboxing: chat embeds render via srcdoc + sandbox attribute (no access to parent
  window, cookies, or storage). The plan panel viewer runs same-origin without sandbox
  because its in-frame comment tray requires same-origin; plan artifact content is
  trusted master-authored output.
- Aim for well-organized, visually polished pages that present information more densely
  than markdown allows. Multiple artifacts per response are supported.

### Cold-Read Gate

The gate applies to every genre one step before the page leaves the author: before each
`charliebot plan present` or `amend` for a plan page, and before sharing for
understanding, sitrep, debug, and explain pages.

Cold-read gate: the page first passes its genre's mechanical DOM assertions, then one
zero-context model pass reads the file alone and answers seven questions: (1) the
problem, (2) the conclusion and its epistemic state, (3) what is asked of the reader,
(4) the section where the problem first became clear, (5) up to five re-read points,
(6) whether the page answers the trigger quoted in the prompt, and (7) the terms,
abbreviations, or names the page uses without explaining. Ship when answers (1)
through (3) match the author's intent, (4) names the first content section, the
epistemic state in (2) matches the page's own labels, (6) is a yes on every part of
the trigger message, and (7) is none; for plan and understanding pages, (3) names the
decisions the page asks for (Trade-offs or divergences). A re-read point in (5) naming
section 1's forks, or a jump between a fork and another section, also means revise
and re-run. Judge on these signals alone.

The trigger quoted in question 6 is the chat message that asked for the page; a plan
quotes the confirmed understanding's goal sentence verbatim and falls back to the
originating request when no understanding exists.

Invocation (the genre's assertions run first; the probe only fires once every one of
them passed):

```bash
charliebot artifact check <page-file> --genre <plan|understanding|sitrep|debug|explain> --trigger "<trigger message verbatim>"
```

`--assertions-only` runs the assertions alone and mirrors the plan registration gate.

The seven-question prompt, the backend order, and the timeout live in
`src/core/artifact_check.py`; the command prints each tried backend's failure, then the
answering backend's id and the seven answers verbatim. Exit 0 means every assertion
passed; judging the answers stays with the reader of this gate.
