---
name: file-server
description: Must invoke when presenting a file to the user. The CharlieBot server has a built-in file browser that serves any file on the host filesystem.
version: 1.1.0
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
where it is written. This is the sole canonical prefix: the legacy `/files` and `/file` spellings
are unmounted and answer 404, so never write or repair a link onto them. The file server sits
behind the access key — an unauthenticated browser navigation gets the unlock form, not the file.

Examples:

| Filesystem path | URL |
|---|---|
| `/path/to/trace.json` | `<base_url>/absolute_filepath/path/to/trace.json` |
| `/path/to/results/` | `<base_url>/absolute_filepath/path/to/results/` |

## Publish Lane

Links that reach readers beyond the operator come from `charliebot publish <artifact-path>`: the
command copies the file to `<publish.dir>/<token>/<basename>` and prints the published URL,
`<publish.public_base_url>/<token>/<basename>`. The token is 22 random URL-safe characters, fresh
per publish, so publishing the same file twice gives two links and overwrites nothing. The host
serves the publish directory publicly, so anyone holding a link opens it, and a link cannot be
guessed from the page name. Publishing refuses unless `<publish.dir>/index.html` exists: without
it the host's static server would list the directory, and that listing would expose every link.

A host can publish through an external program in place of the publish directory. `publish.command` holds the program's argv. `charliebot publish <artifact-path> [-- <lane options>]` runs that argv with the artifact's absolute path and the lane options appended. The program prints the reader URL as its one line of stdout and exits 0. `charliebot publish` prints that URL. When the program exits non-zero, `charliebot publish` exits 1 and shows the program's stderr. The host skill that installs the program documents its lane options. A host configures one lane: `publish.command`, or the pair `publish.dir` and `publish.public_base_url`.

Reader links come only from `charliebot publish`: the Slack and Discord reply paths post the
reply text as written — they never rewrite links — and a reply that still contains a file-server
URL is refused with a 422 naming the link and the publish command that fixes it. The server-port
links above serve the operator's own review in the browser and the chat embeds.

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

1. Before you paste a file link, run `charliebot files probe '<link>'` on that exact link.
   Paste the link only after the command prints `200`.
   The local 200 certifies the path that the reader opens, because the link's public base URL names the same server.
   The command authenticates inside its own process and prints only the status and the probed path.
   The session thus reads no credential file and handles no key.
2. Present the link in markdown format: `[descriptive text](url)`
3. **If the file is a Perfetto/Chrome trace** (`.json` trace from training/profiling, or a directory
   of rank traces), ALWAYS also include a Perfetto viewer link alongside the file link. Read the
   `perfetto` skill for how to construct the viewer URL.

   Trace indicators: filename matches `trace_rank*.json` / `*trace*.json`, lives under a `trace/` or
   `profile/` dir, or the user called it a "trace"/"profile"/"perf capture".
4. Write a raw filesystem path as plain text, `path:line`, since the CharlieBot UI renders a markdown
   link around a raw local path as a dead link. The form to wrap in `[descriptive text](url)` is a
   file-server URL.
5. An artifact shared beyond the operator (a Slack post, a group report) ships comment-disabled: the
   comment tray belongs to the operator's own review flow.

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

Pages of every genre pass the cold-read gate.
The gate has two parts: the genre's mechanical assertions and one cold read.

A waiting page (`prompts/web_reply.md`, Form) has the genre `waiting`.

- When you want a page's failures before the status set, run `charliebot artifact check <page-file> --genre <genre> --assertions-only`.
  A broken page then stops before the user opens it.
- The status set that links a page runs the fork assertions of its genre and refuses a broken page.
- The cold read of `charliebot web status set` reads the item together with its page.
- Count that cold read as the page's cold read, and start no second cold read for the page.
  Each Waiting on you item then takes one cold read.

Before you share a page or register a plan version, run the assertions:

    charliebot artifact check <page-file> --genre <genre> --assertions-only

When an assertion fails, fix the page and run the assertions again.
Plan registration runs the same assertions.
The page-height line reports the measured height against the 2000 px target.
The page-height assertion passes at any height.
When a gloss puts the page over the target, keep the gloss.

The cold read is one model pass without context. The model reads the page file alone.
It answers eight questions about these items:

1. the problem;
2. the conclusion and its epistemic state;
3. the action that the page asks of the reader;
4. the section where the problem first became clear;
5. at most five re-read points;
6. whether the page answers the trigger;
7. the project terms and page conventions whose meaning the page never states, for the reader of
   the Vocabulary rule (prompts/master.md);
8. what the run changed and what it held fixed, each with a value.

The trigger is the chat message that asked for the page.
For a plan, the trigger is the goal sentence of its understanding page, verbatim.
When no understanding exists, the trigger for a plan is the originating request.

The cold read runs in a cold-read child session after the page leaves the author.
Start the cold read with this command:

    charliebot artifact check <page-file> --genre <genre> --trigger "<trigger>"

- After the Improve round, and after each later `charliebot plan amend`, start the cold read.
- When verify runs on the plan version, start the cold read in the same turn as the verify delegation.
- For a page of any other genre: after you share the page, start the cold read.

In a manager session, the command delegates the cold read to a cold-read child of your session.
After the command prints its result, end the turn.
The child's report wakes your session.
The report states the page path and the sha256 of the page bytes that the cold read read.
Then the report lists each failed model attempt, the answering model, and the eight answers verbatim.
In a worker session, the command runs the cold read in the foreground and prints the same lines.

When the cold read fails, state in the reply that the cold read failed.
When the page changed after the cold read started, check each answer against the current page.
Each item below is a signal to revise the page:

- Answers 1 to 3 differ from your intent for the page.
- Answer 2 gives an epistemic state that differs from the labels on the page.
- Answer 4 names a section other than the first content section.
- Answer 6 is no for one part of the trigger.
- Answer 7 lists one or more terms.
- Answer 8 is NOT STATED, in whole or in part, on a page that reports a run.
- On a plan or understanding page, answer 3 omits a decision that the page asks for.
- Answer 3 contains FOLDED, or omits an option that a visible option line shows.
- A re-read point in answer 5 names a decision fork (a div.fork block) in section 1, or a jump between a fork and another section.
- A re-read point in answer 5 names the run, its variables, or the difference between the compared legs.

Revise each genuine clarity gap that these signals surface, and skip a false-positive flag on an established
project or domain term.
For each real gap, revise the page in place.
For a plan, register the revision with `charliebot plan amend`.
When a verify round runs, fold the cold-read revisions and the verify findings into one amend.
Leave term-count tallies out of the chat reply.
A revision from cold-read findings closes the gate. That revision gets no cold read.

The cold-reader instructions and the eight questions live in `prompts/cold_read.md`.

### Source Check

A page that states a cause, a mechanism, or a derived number carries facts
that the user acts on. A derived number is a sum, a ratio, or a difference
that the page computes. The cold read judges readability, and the source
check judges the facts.

- The page author or the worker that built the page verifies every cited `path:line` and derived number before
  sharing, and corrects the page or labels the claim Open.
- A plan page takes the verify round of the plan-approval skill in place of this check.
- A page whose every statement reads a current value back from a system (a count, a time, a status) keeps its genre
  checks only.
- When a separate fact-check worker is wanted, `source_check_spec.md` in this skill's directory is an optional
  reference template; delegating it never blocks sharing the page.
