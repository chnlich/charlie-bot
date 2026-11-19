#!/usr/bin/env bash
set -euo pipefail

if [[ $# -eq 0 ]]; then
  printf 'usage: %s <lucide-icon-name>...\n' "$0" >&2
  exit 2
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
icon_dir="$repo_root/web/static/icons/lucide"
license_path="$repo_root/web/static/icons/LICENSE"
base_url="https://cdn.jsdelivr.net/npm/lucide-static@1.76.0"

for name in "$@"; do
  if [[ ! "$name" =~ ^[a-z0-9-]+$ ]]; then
    printf 'invalid Lucide icon name: %s\n' "$name" >&2
    exit 2
  fi
done

mkdir -p "$icon_dir"

if [[ ! -e "$license_path" ]]; then
  curl --fail --silent --show-error --location "$base_url/LICENSE" --output "$license_path"
fi

for name in "$@"; do
  destination="$icon_dir/$name.svg"
  curl --fail --silent --show-error --location "$base_url/icons/$name.svg" --output "$destination"
  if ! head -n 1 "$destination" | rg -q '^<!-- @license lucide-static v1\.76\.0 - ISC -->$'; then
    printf 'Lucide icon %s is missing its pinned ISC license header\n' "$name" >&2
    exit 1
  fi
done
