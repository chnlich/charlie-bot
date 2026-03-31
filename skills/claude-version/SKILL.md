# Claude CLI Version Management

Manage Claude Code CLI versions — install, switch, rollback, list.

## Two install methods

| Method | How it works | Version switch |
|--------|-------------|----------------|
| **Official installer** (`curl -fsSL https://claude.ai/install.sh \| sh`) | Standalone binary at `~/.local/share/claude/versions/<ver>`, symlinked from `~/.local/bin/claude` | Re-point the symlink |
| **npm global** (`npm i -g @anthropic-ai/claude-code`) | Lives in the active Node prefix (`$(npm prefix -g)/bin/claude`) | `npm i -g @anthropic-ai/claude-code@<ver>` |

Detect which method is in use:

```bash
which claude        # ~/.local/bin/claude → official installer
                    # ~/.nvm/.../bin/claude → npm global
file $(readlink -f $(which claude))   # ELF = binary install, script/symlink = npm
```

## Official installer method

### Layout

| Path | Purpose |
|------|---------|
| `~/.local/bin/claude` | Symlink → active version |
| `~/.local/share/claude/versions/<ver>` | Binary (≥ ~2.1.80) or wrapper script |
| `~/.local/share/claude/versions/<ver>-node/` | Unpacked npm package (for older Node-based versions) |

### List / check

```bash
ls ~/.local/share/claude/versions/       # installed versions
readlink ~/.local/bin/claude             # active version
claude --version
```

### Switch to an installed version

```bash
ln -sf ~/.local/share/claude/versions/<VERSION> ~/.local/bin/claude
```

### Install a specific older version (< ~2.1.80, Node-based)

Recent versions (≥ ~2.1.80) are standalone ELF binaries. Older versions are Node.js packages and need a wrapper. Requires Node ≥ 18.

```bash
# 1. Download & extract
npm pack @anthropic-ai/claude-code@<VERSION> --pack-destination /tmp/
mkdir -p ~/.local/share/claude/versions/<VERSION>-node
tar xzf /tmp/anthropic-ai-claude-code-<VERSION>.tgz -C /tmp
cp -r /tmp/package/* ~/.local/share/claude/versions/<VERSION>-node/

# 2. Create wrapper script
cat > ~/.local/share/claude/versions/<VERSION> << 'EOF'
#!/bin/bash
exec "$HOME/.nvm/versions/node/$(ls "$HOME/.nvm/versions/node" | sort -V | tail -1)/bin/node" \
     "$HOME/.local/share/claude/versions/<VERSION>-node/cli.js" "$@"
EOF
chmod +x ~/.local/share/claude/versions/<VERSION>

# 3. Activate
ln -sf ~/.local/share/claude/versions/<VERSION> ~/.local/bin/claude
claude --version
```

### Cleanup

Each binary release is ~220 MB. Remove old ones with:

```bash
rm ~/.local/share/claude/versions/<OLD>
rm -rf ~/.local/share/claude/versions/<OLD>-node   # if Node-based
```

## npm global method

### Switch version

```bash
npm i -g @anthropic-ai/claude-code@<VERSION>
claude --version
```

### Rollback

Same command with the older version number.

### List available versions

```bash
npm view @anthropic-ai/claude-code versions --json | tail -20
```

## Notes

- Switching versions does NOT affect the currently running Claude session — only new invocations.
- The two methods can coexist but `$PATH` order determines which `claude` binary wins.
- When using npm method, ensure the active Node version is ≥ 18 (e.g. via nvm).
