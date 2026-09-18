<!-- section: coding_principles -->
## Coding Principles
The codebase has a single user. Apply these principles:
- **Fail fast**: surface errors immediately. Do NOT add fallbacks, defaults, or silent recovery.
- **No swallowed exceptions**: always log or re-raise. Never use bare `except: pass`.
- **No defensive programming**: do not add guards for scenarios that cannot happen.
- **Recipes are executable**: submit, deploy, and recovery flows go through the repo's entry
  point; a command sequence worth running twice becomes a script, and its preflight asserts
  the mechanisms the task depends on (launcher, credentials, environment).
- **含反斜杠内容经文件写入工具或带引号 heredoc 落盘**: LaTeX、正则表达式、Windows 路径这类内容
  用写入工具或 `cat > file <<'EOF'` 写入;页面产物直接交给组装入口点
  (`charliebot artifact wrap`)。嵌进 `python3 -c` 的字符串字面量会让解释器在写入前静默
  改写 \t、\n、\\ 序列。
- **Long commands finish in the foreground**: a command expected within five minutes runs in the foreground and
  the tool waits for it; state the wait budget when the call takes one, and when the tool hands the command back
  still running, the next call waits on it again instead of probing it. A job expected longer starts detached (a
  SLURM job, `setsid nohup`) and is waited for in bounded chunks, one call each, a chunk ending when the job ends
  or its bound expires (`timeout <bound> tail --pid=<pid> -f <log>` locally, a `timeout`-bounded `sacct` loop
  for a SLURM job); the report records the job's identifier. A task spec may hand the watch to the master instead.

<!-- section: skills_discovery -->
## Skills Discovery
- **Before starting any task**, check for skills relevant to the target repo or task domain.
  - Look in **`~/.charliebot/skills/`** (canonical source — always available regardless of CLI backend).
  - Alternatively: `~/.claude/skills/` (Claude Code) or `~/.agents/skills/` (Codex/Gemini).
- **Read matching skills first** to avoid wasting time on environment setup, tooling issues, or reinventing existing workflows.
- **Mandatory for tasks in any domain that has a matching skill**: you MUST read that skill BEFORE writing any code, running any command, or submitting any job. This includes profiling, metrics analysis, data processing — not just training. Starting work without reading the relevant skill is forbidden.
- A local run killed by the session memory cap is a routing error: re-run that step through the host's declared remote-compute entry instead of retrying locally.

<!-- section: remote_scratch -->
## Remote Scratch
Your worktree lives on this host and the remote side mounts nothing from it, so work on a remote host needs a place of
its own there. That place is one directory per run, named for the task by content:

    ~/scripts/<YYYYMMDD>_<slug>/

Every file you create on that host belongs in it, whatever produced it.

A working directory carries only within one ssh one-shot, so each command creates that directory if absent and enters it
before anything else.

