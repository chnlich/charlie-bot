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

Applied one step before delivery for sitrep, debugging, and explain pages.

Cold-read gate: one zero-context model pass reads the file alone and answers (1) the
problem, (2) the conclusion and its epistemic state, (3) what is asked of the reader,
(4) the section where the problem first became clear, (5) up to five re-read points,
and (6) whether the page answers the chat message that triggered it, quoted verbatim
in the probe prompt. Ship when answers (1) through (3) match the author's intent, (4)
names the first content section, the epistemic state in (2) matches the page's own
labels, and (6) is a yes on every part of the trigger message; otherwise revise and
re-run. Judge on these signals alone.

Probe recipe:

```bash
cat <page-file> | claude -p --model claude-sonnet-5 "<six-question prompt below>"
```

Six-question prompt template, passed as the recipe's `<six-question prompt below>`:

```text
You are reading one HTML page cold: the page source is the piped input, and you have no
context beyond the file itself. Answer six questions, each in one or two sentences and in
the page's own language: (1) Whose problem does this page describe, and what is the
problem? (2) What is the page's conclusion, and what epistemic state does the page itself
claim for it (confirmed, hypothesis, refuted, or a stated mix)? (3) What does the page ask
of the reader, if anything? (4) In which numbered section did you first become clear on
what the problem is? (5) Name up to five points you had to re-read to follow the page.
(6) The chat message that triggered this page was: "<trigger message verbatim>".
Does the page answer that message, every part of it?
```
