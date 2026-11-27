<!-- section: coding_principles -->
## Coding Principles
The codebase has a single user. Apply these principles:
- **Fail fast**: surface errors immediately. Do NOT add fallbacks, defaults, or silent recovery. When an explicit
  user choice cannot be parsed or its delivery path fails, stop with the error. A default applies only when the user
  made no explicit choice.
- **No swallowed exceptions**: always log or re-raise. Never use bare `except: pass`.
- **No defensive programming**: do not add guards for scenarios that cannot happen.
- **Switch names**: name each on/off switch for the feature it turns on, as in `ENABLE_X` or `USE_X`. Give each
  switch a default value. A caller turns the feature off with the value 0 or false.
- **New code in its own module**: when new code can stand as its own module, put it in that module. The key file that
  uses it stays small and reaches the module through short calls. A key file is a large source file;
  `dev/check-key-file-growth.sh` sets its size. CI runs that check: it fails a pull request whose diff adds more lines
  to a key file than the check allows. An optimization is one case of this rule: keep it local to the component it
  targets, independent of the global model structure. Put its switch inside that component. Put each optimization in
  its own module or class, apart from the model logic. The model file keeps its pre-optimization form and reaches the
  optimization through one-line calls. A component-local optimization keeps its speedup and the run working through
  model edits elsewhere.
- **Superseded code**: delete it in the same change. A retry or a regenerated output overwrites its predecessor.
  Numbered artifacts compare distinct experiment legs.
- **Known defects and stale docs**: when a change touches code with a known defect or a stale doc, fix that defect
  or doc in the same change.
- **Before the first release**: ship each change as new, with no compatibility path for the
  old form. The first release then carries one form.
- **One owner per responsibility**: give each responsibility one owning module. When a change meets scattered
  ownership, consolidate it.
- **One general mechanism**: serve every backend, tool, field and session origin through one general mechanism. A new
  case then reuses that path, so the code keeps one path to test and maintain. The backend of each call comes from the
  `backends` config. Code names no model id, option id, pool id, gateway URL or agent CLI as the backend of a call.
  Instruction text names no model id, option id, pool id or gateway URL as one. `charliebot backend check` refuses such
  a name at landing, in CI and at each cron fire. At server start it reports such a name to each enabled cron task's
  bound session and does not stop the server. The execution role (`manager` or `worker`) alone decides a session's core
  capabilities. A session's origin (web, external thread or cron) attaches only its channel or schedule binding.
  `dev/check-session-origin.sh` is the CI check that enforces this boundary against an explicit channel-binding
  allowlist.
- **Minimal schema**: a schema is the structure that a design stores or transmits: its records, fields, states,
  config keys, command flags and API fields. When a design has two or more workable schemas for what it adds or
  changes, recommend the smallest schema that serves most of the user's stated needs. Schema that the design keeps
  unchanged, and the stored records that it must still read, stay outside the comparison. A stated need that an
  existing path in the product already serves counts as served. Compare schemas by schema alone: code size, page
  length, usage counts, and whether a product is in use on this host stay outside the comparison. Name each stated
  feature that the recommended schema drops, its reason, and the schema that would add it back. State each reason as
  the fact about the user's needs or the product that makes the drop correct, not the history of the decision. Carry
  each drop into later versions and later designs only while its reason holds; once it no longer holds, return the
  feature to the comparison:
  - a plan names them in its Schema section;
  - a page names them beside its recommendation;
  - a delivery report names them for each choice that the agent made without asking the user.
  When the user names a dropped feature in a reply, add that feature back with its schema.
  Three cases follow their own rule:
  - a schema that offers the same options as an established external interface that its users already know, one
    field or flag for each option, counts as the minimal form of that interface, also when the design's own format
    carries it;
  - a safety check at a trust boundary stays in the design, and the comparison picks its smallest workable form; an
    explicit confirmation is such a check, and its smallest workable form is one flag or field that the caller sets
    in the call itself, so the boundary refuses each call that lacks it; a confirmation before an agent acts on a
    task outside its own scope is an explicit confirmation;
  - mechanism complexity, such as fallbacks, retries and multi-layer checks, stays outside this rule.
- **Repo environment**: use the environment that the repo defines. When a toolchain is missing, escalate it and name
  the defect in the manifest.
- **Recipes are executable**: submit, deploy, and recovery flows go through the repo's entry
  point; a command sequence worth running twice becomes a script, and its preflight asserts
  the mechanisms the task depends on (launcher, credentials, environment).
- **含反斜杠内容经文件写入工具或带引号 heredoc 落盘**: LaTeX、正则表达式、Windows 路径这类内容
  用写入工具或 `cat > file <<'EOF'` 写入;页面产物直接交给组装入口点
  (`charliebot artifact wrap`)。嵌进 `python3 -c` 的字符串字面量会让解释器在写入前静默
  改写 \t、\n、\\ 序列。
- **Waiting releases the process**: a held process delays every message to its session, so run work in the foreground only when it finishes within 15 minutes (or within a 60-second readback after an action such as a push). When a worker run waits on an action that only the user can take, end the run with a blocker report that names the action; the manager that receives the report carries the wait. Every other wait, a CI run or a server restart included, ends the turn through a registered trigger (see `charliebot session trigger --help`), and the trigger wake starts the next turn in the same conversation.
- **Lookups stay bounded**: a lookup across the session store or the filesystem returns within
  seconds, because a held command delays every message to its session.
  - The `charliebot scan-guard` hook refuses a per-file process over the session store, a search
    rooted at `/` or the home directory that neither limits depth nor prunes the session store, and a
    recursive search of the session store or a parent directory that reaches run logs. Its refusal
    names the bounded form.

### Web UI Screenshots
For any change that alters what a CharlieBot web page shows, use the reviewed `implement` workflow. If a quick-edit task
would alter a page, report that it needs rerouting before editing.
- Capture screenshots of the changed state before you commit.
- Cover each state the change adds or alters, including open trays, menus and dialogs.
- Capture each window size that `charliebot land --help` lists at device pixel ratio 1, filling each view with the
  longest real content that the page shows in use and capturing the whole screen with every related change applied.
- Keep the screenshot files out of the commits.
- For a landing through `charliebot land`, the PR body names the approved demo and each screenshot by file name, never
  by a path. The directory that `--evidence-dir` names holds those files. The View line names the full URL (path and
  query) that opens the changed state with real content.
- For an improve round, follow `dev/improve/landing.md` Steps 5 and 9 for the screenshots' location, the pull request's
  naming of them and their delivery to the user.

When you review such a change, check each screenshot it names before you land it, wherever the landing keeps the file,
judging it against the demo the change names (or for an improve round against the same round's before screenshot and
direction file) and pass it only when it matches that reference and every region outside the change keeps room for its
content.
- Look for overlap with page content, clipping and missing controls in every state the screenshots show.
- Report the landing as blocked instead of landing it when a screenshot is missing or shows one of these defects.

`charliebot land` refuses a landing whose diff changes a file that a CharlieBot page serves, or a page template under
`prompts/`, unless its PR body names the approved demo on a `Demo:` line, a `View:` line, and a screenshot at each
window size `charliebot land --help` lists, each by file name in the `--evidence-dir` directory. A `View:` line names
the full URL, path and query, that opens the changed state with real content on the live server, such as
`/diff?repo=~/workspace/charlie-bot&base=<sha>~1&head=<sha>` or `/?session=<id>`. A `/?session=<id>` view names a
session the server knows. An improve round merges through
`dev/improve/github-post.sh pr-merge`, which refuses a web page change whose evidence directory lacks a screenshot at
each window size, whose PR body lacks its `View:` line, or whose review record shows no read of a screenshot the
body names.

After the release switch, compare each live screenshot that `scripts/switch-server.sh` prints for a commit with the demo
before you report the change delivered. When the switch exits 7 with `not-visible` on a commit whose change only opens
through interaction, confirm that change from the screenshots its landing named, and name those screenshots and the
check's reason in the delivery report; a `broken` verdict or failed check requires a fix instead.

<!-- section: skills_discovery -->
## Skills Discovery
- **Before starting any task**, check for skills relevant to the target repo or task domain.
  - Look in `~/.claude/skills/` or `~/.agents/skills/`: both symlink the two canonical sources, the charlie-bot checkout's `skills/` (general, writing-style included) and `$CHARLIEBOT_HOME/skills/` (host-specific).
- Before you act on a task, read each matching skill to avoid wasting time on environment setup, tooling issues,
  or reinventing existing workflows.
- A local run killed by the session memory cap is a routing error: re-run that step through the host's declared remote-compute entry instead of retrying locally.
- **Purpose over letter**: instructions include rules, task-spec items, and checks.
  When a case falls outside every instruction, derive the action from the purposes of related instructions.
  When an instruction's literal action would defeat its stated purpose, report the conflict before you act.
  A step that defeats its own purpose costs the rework that the instruction exists to prevent.

<!-- section: profile_home -->
## Profile Home
`CHARLIEBOT_HOME` holds the path of this instance's profile home: its config, credentials, sessions, memory store and host skills.
The server sets it in every manager turn and worker run.
Write each path under the profile home as `$CHARLIEBOT_HOME/<path>`.

<!-- section: remote_scratch -->
## Remote Scratch
Your worktree lives on this host and the remote side mounts nothing from it, so work on a remote host needs a place of
its own there. That place is one directory per run, named for the task by content:

    ~/scripts/<YYYYMMDD>_<slug>/

By default, place every file you create on that host in `~/scripts/<YYYYMMDD>_<slug>/`, whatever produced it. When a
host's storage rules assign large temporary artifacts to a data-disk task directory, place large temporary artifacts in
that data-disk task directory and keep scripts and logs in `~/scripts/<YYYYMMDD>_<slug>/`.

A working directory carries only within one ssh one-shot, so each command creates that directory if absent and enters it
before anything else.

<!-- section: text_model_reads -->
## Text a model reads
- **Text a model reads**: before you write text for a model, read the "Text a model reads"
  subsection of `~/workspace/charlie-bot/prompts/master.md`.

