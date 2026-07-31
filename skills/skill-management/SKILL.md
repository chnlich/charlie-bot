---
name: skill-management
description: >
  CharlieBot skill directory layout, sync rules, and CLI tool mappings.
  Use when creating, installing, or syncing skills across Claude Code, Codex, Gemini, OpenCode, and Antigravity.
---

# Skill Management

## Skill Sources (canonical)

| Location | Scope | Description |
|----------|-------|-------------|
| `<charlie-bot-repo>/skills/` | General (shared across hosts) | Integration skills, self-knowledge |
| `~/.charliebot/skills/` | Host-specific (per-machine) | Domain skills tied to local repos/hardware |

## CLI Tool Skill Directories

| CLI Tool | User Skills Path | Shared Standard | Notes |
|----------|-----------------|-----------------|-------|
| **Claude Code** | `~/.claude/skills/<name>/SKILL.md` | Proprietary | Also reads `.claude/skills/` at project level |
| **Codex CLI** | `~/.agents/skills/<name>/SKILL.md` | Agent Skills (open standard) | `.system/` in `~/.codex/skills/` is Codex built-in; user skills go in `~/.agents/skills/` |
| **Gemini CLI** | `~/.gemini/skills/<name>/SKILL.md` OR `~/.agents/skills/<name>/SKILL.md` | Agent Skills (open standard) | `~/.agents/skills/` takes precedence over `~/.gemini/skills/` |
| **OpenCode** | scans `~/.claude/` and `~/.agents/` | reuses both standards | No dedicated target — auto-discovers Claude Code + Agent Skills. Disable via `OPENCODE_DISABLE_CLAUDE_CODE_SKILLS=1` |
| **Antigravity (`agy`)** | `~/.gemini/antigravity-cli/skills/<name>/SKILL.md` | Agent Skills (open standard) | Global customizations root (also scans `~/.gemini/skills/`); does NOT read `~/.claude`/`~/.agents` |

### Deduplication

Three sync targets cover all five CLIs:

1. `~/.claude/skills/` — Claude Code
2. `~/.agents/skills/` — Codex + Gemini (shared open standard)
3. `~/.gemini/antigravity-cli/skills/` — Antigravity (`agy`)

OpenCode needs **no** target: it auto-scans `~/.claude/` and `~/.agents/`, both already populated above.

## Sync Rules

- Each skill entry in target dirs is a **symlink** pointing to the canonical source
- Never overwrite non-symlink entries or dotfiles (e.g. Codex `.system/`)
- Remove stale symlinks whose targets no longer exist
- Sync script: `<charlie-bot-repo>/scripts/sync-skills.sh`
- Workers see skills through the backend CLI's skill dir — not directly from
  `~/.charliebot/skills/`.

## Host-Specific Content Boundary

Repo-level shared files (`<charlie-bot-repo>/skills/**`, `<charlie-bot-repo>/prompts/**`) MUST NOT contain host-specific info — hostnames, local paths, GPU specs, per-host ports/IPs. Anything host-specific belongs in the memory store's `host` topic (`~/.charliebot/memory/entries/host/`) or `~/.charliebot/skills/<name>/`. Rules like "feature X is local-host only" must be flagged in the repo-level skill so that workers running anywhere see them.

Repo-level skills must also stay free of session-specific debug artifacts: dated transcripts (`observed in D4`, `2026-05-04 run`), internal task IDs, specific bug strings (`bytes-vs-str + train_ranges API mismatches`), or any phrasing that reads like a LESSONS.md entry. Workers loading shared skills cannot see `~/.charliebot/LESSONS.md` and have no context for those references. Keep shared skills as static principles; only promote a finding into a shared skill once the debug is closed and the lesson rewrites cleanly as a generic rule.

A local pre-commit hook enforces this boundary. One-time setup per clone:

    git config --local core.hooksPath scripts/git-hooks

Additional host-specific blocklist patterns (real names, internal project names, tenant identifiers, etc.) live at `~/.charliebot/skills_leak_patterns.local.txt` — one regex per line, NEVER committed to this repo. Run `scripts/check-skills-host-leak.sh` manually anytime to scan the whole tree.

## File Name Convention

All CLIs require `SKILL.md` (exact name, case-sensitive) as the entry point file.

## Updating Existing Skills

When new lessons surface and a skill needs an update, follow these rules:

- **Reorganize, don't append.** If related content already lives in the skill, edit the existing section in place rather than tacking on a new paragraph. Skills must stay concise so they remain scannable; growing them by accretion (one new bullet per session) defeats the purpose. Re-read the whole skill first, then propose the smallest diff that captures the new lesson.
- **Ask before modifying any local skill.** Surface the intended edit to the user before writing it — never silently mutate a skill file, even when "just adding a clarifying line".
- **Always present the change as a file diff.** Show the actual before/after (or `Edit` patch) so the user can see surrounding context and judge whether the edit fits. Don't summarize the change as "I'd add a note about X" — the user needs to see the literal lines being added/changed/removed to approve.
