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
- **Waiting releases the process**: a command that does work runs in the foreground and returns its result.
  A command that sleeps or repeats a check is a wait.
  A held process delays every message to its session, so each wait follows one of these cases.
  - When your command does work that can finish within 15 minutes, run it in the foreground in one call.
  - When a background task of your tool is the last result you need, collect it with one `tail --pid` call.
  - After an action such as a push, wait up to 60 seconds for its readback.
    Use one command with a 60-second timeout that returns once the expected state shows.
  Every other wait ends your turn, a CI run or a server restart included.
  The wake starts your next turn in the same conversation.
  A registered trigger persists through a server restart, so a restart readback also goes to a trigger.
  - Give every registration below `--max-wait <seconds>` and a short `--message <label>`.
  - To wait on a command that can run past 15 minutes, start it with `charliebot schedule-trigger --cmd`.
    The wake carries the command's exit code and log path.
  - To wait on several commands, start one `--cmd` that runs them all and ends with `wait`.
  - To wait on a process that already runs, run `charliebot schedule-trigger --watch <pid>`.
    For a process on a remote host, the watch target is `<host>:<pid>`.
  - To wait on a SLURM job, run `charliebot schedule-trigger --watch slurm:<job id>`.
    For a job on a remote cluster, the watch target is `<host>:slurm:<job id>`.
  - To wait on a condition, start a command that exits once the condition holds, with `--cmd`.
  - When no command can detect the condition, run `charliebot schedule-trigger --max-wait <seconds>`.
  - After the registration prints its result, end the turn.

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

