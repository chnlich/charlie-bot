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
- **Optimizations stay local**: keep every optimization local to the component it targets, independent of the global
  model structure. Put its switch inside that component. Put each optimization in its own module or class, apart from
  the model logic. The model file keeps its pre-optimization form and reaches the optimization through one-line calls.
  A component-local optimization keeps its speedup and the run working through model edits elsewhere.
- **Superseded code**: delete it in the same change. A retry or a regenerated output overwrites its predecessor.
  Numbered artifacts compare distinct experiment legs.
- **Before the first release**: ship each change as new, with no compatibility path for the
  old form. The first release then carries one form.
- **One owner per responsibility**: give each responsibility one owning module. When a change meets scattered
  ownership, consolidate it.
- **Repo environment**: use the environment that the repo defines. When a toolchain is missing, escalate it and name
  the defect in the manifest.
- **Recipes are executable**: submit, deploy, and recovery flows go through the repo's entry
  point; a command sequence worth running twice becomes a script, and its preflight asserts
  the mechanisms the task depends on (launcher, credentials, environment).
- **含反斜杠内容经文件写入工具或带引号 heredoc 落盘**: LaTeX、正则表达式、Windows 路径这类内容
  用写入工具或 `cat > file <<'EOF'` 写入;页面产物直接交给组装入口点
  (`charliebot artifact wrap`)。嵌进 `python3 -c` 的字符串字面量会让解释器在写入前静默
  改写 \t、\n、\\ 序列。
- **Waiting releases the process**: a held process delays every message to its session, so run work in the foreground only when it finishes within 15 minutes (or within a 60-second readback after an action such as a push). Every other wait, a CI run or a server restart included, ends the turn through a registered trigger (`charliebot schedule-trigger` with `--max-wait <seconds>` and a short `--message <label>`; see `charliebot schedule-trigger --help` for `--cmd` and `--watch` targets), and the trigger wake starts the next turn in the same conversation.

<!-- section: skills_discovery -->
## Skills Discovery
- **Before starting any task**, check for skills relevant to the target repo or task domain.
  - Look in `~/.claude/skills/` or `~/.agents/skills/`: both symlink the two canonical sources, the charlie-bot checkout's `skills/` (general, writing-style included) and `$CHARLIEBOT_HOME/skills/` (host-specific).
- **Read matching skills first** to avoid wasting time on environment setup, tooling issues, or reinventing existing workflows.
- **Mandatory for tasks in any domain that has a matching skill**: you MUST read that skill BEFORE writing any code, running any command, or submitting any job. This includes profiling, metrics analysis, data processing — not just training. Starting work without reading the relevant skill is forbidden.
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

Every file you create on that host belongs in it, whatever produced it.

A working directory carries only within one ssh one-shot, so each command creates that directory if absent and enters it
before anything else.

<!-- section: text_model_reads -->
## Text a model reads
- **Text a model reads**: before you write text for a model, read the "Text a model reads"
  subsection of `~/workspace/charlie-bot/prompts/master.md`. This text includes prompts, skills,
  task specs, memory entries, messages to other agents, and `CLAUDE.md` and `AGENTS.md` files.

