#!/usr/bin/env bash
# Runs codex-skill-roots.sh against a stub codex on PATH. The stub renders the skills section of `codex debug
# prompt-input` from the roots that its CODEX_HOME and HOME name: the pinned skills, the bundled skills and the user
# skills. It honors the codex home's config.toml the way the round's codex does: a bundled root that the config turns
# off is not listed or installed, and a skill whose path the config turns off is not listed. STUB_CODEX_IGNORES_CONFIG
# makes the stub ignore the config, which is a codex whose discovery still lists a host skill.
#
#   dev/improve/codex-skill-roots.test.sh
set -euo pipefail

# The runner's helper that the module calls. It matches the runner's message prefix and its exit codes.
fail() {
  local code=$1
  shift
  printf 'improve runner: %s\n' "$*" >&2
  exit "$code"
}

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
source "$SCRIPT_DIR/codex-skill-roots.sh"

TMP_ROOT=$(mktemp -d)
trap 'rm -rf -- "$TMP_ROOT"' EXIT

mkdir -p -- "$TMP_ROOT/bin"
cat >"$TMP_ROOT/bin/codex" <<'STUB'
#!/usr/bin/env bash
# Stub codex: `debug prompt-input` renders the skills section of the round's input, and no other command is supported.
set -euo pipefail
if [[ ${1-} != debug || ${2-} != prompt-input ]]; then
  printf 'stub codex: unsupported arguments: %s\n' "$*" >&2
  exit 2
fi
config=$CODEX_HOME/config.toml
bundled_on=1
off=()
if [[ -z ${STUB_CODEX_IGNORES_CONFIG-} && -r $config ]]; then
  if awk '/^\[skills\.bundled\]$/ { in_bundled = 1; next }
    in_bundled && /^enabled = false$/ { found = 1; exit }
    { in_bundled = 0 }
    END { exit !found }' "$config"; then
    bundled_on=0
  fi
  while IFS= read -r path; do
    off+=("$path")
  done < <(sed -n 's/^path = "\(.*\)"$/\1/p' "$config")
fi
# A bundled install writes through the codex home's link into the pinned directory, as codex does when its bundled
# root is on.
if (( bundled_on )); then
  mkdir -p -- "$CODEX_HOME/skills/.system/imagegen"
  printf -- '---\nname: imagegen\ndescription: stub bundled skill\n---\n' \
    >"$CODEX_HOME/skills/.system/imagegen/SKILL.md"
fi

is_off() {
  local candidate
  for candidate in "${off[@]+"${off[@]}"}"; do
    [[ $candidate == "$1" ]] && return 0
  done
  return 1
}

roots_text=
entries_text=
index=0
add_root() {
  local dir alias="r$index" skill name path
  index=$((index + 1))
  [[ -d $1 ]] || return 0
  dir=$(readlink -f -- "$1")
  roots_text+=$'\n'"- \`$alias\` = \`$dir\`"
  for skill in "$dir"/*/SKILL.md; do
    [[ -f $skill ]] || continue
    name=${skill#"$dir"/}
    name=${name%/SKILL.md}
    path="$dir/$name/SKILL.md"
    is_off "$path" && continue
    entries_text+=$'\n'"- $name: stub skill (file: $alias/$name/SKILL.md)"
  done
}
add_root "$CODEX_HOME/skills"
add_root "$HOME/.agents/skills"
if (( bundled_on )); then
  add_root "$CODEX_HOME/skills/.system"
fi

text="<skills_instructions>
## Skills
### Skill roots${roots_text}
### Available skills${entries_text}
</skills_instructions>"
jq -n --arg text "$text" '[{role: "developer", content: [{type: "input_text", text: $text}]}]'
STUB
chmod +x -- "$TMP_ROOT/bin/codex"

home=$TMP_ROOT/home
pinned=$TMP_ROOT/pinned
mkdir -p -- "$home/.agents/skills/host-skill" "$pinned/pinned-skill"
printf -- '---\nname: host-skill\ndescription: host skill\n---\n' >"$home/.agents/skills/host-skill/SKILL.md"
printf -- '---\nname: pinned-skill\ndescription: pinned skill\n---\n' >"$pinned/pinned-skill/SKILL.md"

export PATH="$TMP_ROOT/bin:$PATH" HOME="$home" IMPROVE_SKILLS_DIR="$pinned"

# A clean host: the round's codex sees only the pinned skill, the bundled root never writes into the pinned directory,
# and the host skill is turned off by its path in the codex home's config.
codex_home=$TMP_ROOT/codex-home-clean
mkdir -p -- "$codex_home"
prepare_codex_skill_roots "$codex_home"
[[ $(readlink -- "$codex_home/skills") == "$pinned" ]] \
  || fail 1 "the codex home's skills do not link the pinned directory"
[[ ! -e $pinned/.system ]] || fail 1 "the bundled skills were written into the pinned directory"
grep -qxF "path = \"$home/.agents/skills/host-skill/SKILL.md\"" "$codex_home/config.toml" \
  || fail 1 "the config does not turn off the host skill by its path"
codex_skill_listing "$codex_home"
[[ $skill_listing == "$pinned/pinned-skill/SKILL.md" ]] \
  || fail 1 "the discovery lists other than the pinned skill: $skill_listing"
printf 'ok: a clean host lists only the pinned skills\n'

# A codex whose discovery still lists the host skill: the round refuses to start, exit 7, and names the path.
codex_home=$TMP_ROOT/codex-home-refused
mkdir -p -- "$codex_home"
status=0
(STUB_CODEX_IGNORES_CONFIG=1 prepare_codex_skill_roots "$codex_home") 2>"$TMP_ROOT/refused.err" || status=$?
[[ $status == 7 ]] || fail 1 "the start check exited $status, not 7: $(cat -- "$TMP_ROOT/refused.err")"
grep -qF "would list a skill outside $pinned: $home/.agents/skills/host-skill/SKILL.md" "$TMP_ROOT/refused.err" \
  || fail 1 "the start check does not name the host skill path: $(cat -- "$TMP_ROOT/refused.err")"
printf 'ok: a discovery that lists a host skill refuses the round with exit 7\n'
