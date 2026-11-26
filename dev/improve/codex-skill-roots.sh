# The skill roots of a codex round's CODEX_HOME. remote-runner.sh sources this file in the round's scope and calls
#
#   prepare_codex_skill_roots <codex home>
#
# before the round's agent starts, once for the member's codex home and, when the review is codex, once for the
# review's. After the call, the codex home's discovery lists only $IMPROVE_SKILLS_DIR, at its own paths.
#
# Codex lists skills from three roots. `$CODEX_HOME/skills` is the pinned directory: the codex home links it to
# $IMPROVE_SKILLS_DIR, and codex names each of its skills at the link's target. `$CODEX_HOME/skills/.system` holds the
# bundled skills, which `[skills.bundled] enabled = false` turns off before any listing: otherwise codex would write
# them into the pinned directory. `$HOME/.agents/skills` holds the host's user skills. HOME names that root, not
# CODEX_HOME, and a root is not turned off by its directory, so each host skill that the discovery still lists is turned
# off by its SKILL.md path in the codex home's config.toml.
#
# The discovery is codex's own: `codex debug prompt-input` renders the skills section that the agent sees, with no model
# call. The listing runs in the current directory, which is the round's. The discovery runs twice: the first listing
# finds the host's skills, and the second is the start check. The check fails the round (exit 7) when the discovery
# still lists a skill path outside $IMPROVE_SKILLS_DIR, and it names that path.
#
# The file defines functions only. It relies on the runner's `fail` and on $IMPROVE_SKILLS_DIR, which the runner
# exports.

# Set skill_listing to the skill file paths that the codex home's discovery lists, one per line, sorted and unique.
codex_skill_listing() {  # <codex home>
  local home=$1 rendered section
  if ! rendered=$(CODEX_HOME=$home timeout 120 codex debug prompt-input </dev/null); then
    fail 7 "codex discovery failed for the codex home $home: codex debug prompt-input exited with an error" \
      "(its stderr is above)"
  fi
  section=$(jq -r '.. | strings | select(contains("<skills_instructions>"))' <<<"$rendered") \
    || fail 7 "codex discovery printed no JSON for the codex home $home"
  skill_listing=$(skill_entry_paths <<<"$section" | sort -u)
}

# The file paths of a rendered skills section, read from standard input. Each entry names its file as
# `(file: <alias>/<rest>)`, and the section's roots table maps the alias to a root: "- `r0` = `/path`". An entry without
# an alias names an absolute path.
skill_entry_paths() {
  awk '
    /^- `r[0-9]+` = `/ {
      alias = $2
      gsub(/`/, "", alias)
      root = $0
      sub(/^- `r[0-9]+` = `/, "", root)
      sub(/`$/, "", root)
      roots[alias] = root
      next
    }
    /\(file: [^)]*\)$/ {
      file = $0
      sub(/^.*\(file: /, "", file)
      sub(/\)$/, "", file)
      alias = file
      sub(/\/.*$/, "", alias)
      if (alias in roots) {
        print roots[alias] substr(file, length(alias) + 1)
      } else {
        print file
      }
    }'
}

# Link the codex home's skills to the pinned directory, write its config, and check the discovery it leaves.
prepare_codex_skill_roots() {  # <codex home>
  local home=$1 path
  rm -rf -- "$home/skills"
  ln -s -- "$IMPROVE_SKILLS_DIR" "$home/skills" || fail 1 "cannot link the round's pinned skills into $home"
  printf '[skills.bundled]\nenabled = false\n' >"$home/config.toml"
  codex_skill_listing "$home"
  {
    printf '[skills.bundled]\nenabled = false\n'
    while IFS= read -r path; do
      if [[ -n $path && $path != "$IMPROVE_SKILLS_DIR"/* ]]; then
        printf '\n[[skills.config]]\npath = %s\nenabled = false\n' "$(jq -Rn --arg path "$path" '$path')"
      fi
    done <<<"$skill_listing"
  } >"$home/config.toml"
  codex_skill_listing "$home"
  while IFS= read -r path; do
    if [[ -n $path && $path != "$IMPROVE_SKILLS_DIR"/* ]]; then
      fail 7 "the codex discovery of the round would list a skill outside $IMPROVE_SKILLS_DIR: $path (codex home $home)"
    fi
  done <<<"$skill_listing"
}
