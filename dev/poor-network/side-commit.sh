#!/usr/bin/env bash
# Names COMMIT as origin/main of the bench tree TREE, fetching it from SOURCE first when TREE lacks it. The trial reads its
# commit and its origin/main merge base from the repo its script runs in, which is TREE on the bench host: the synced tree
# holds only the upload's own commit. run.sh runs this before each side, and it moves only TREE's own ref.
# Usage: side-commit.sh TREE COMMIT SOURCE
set -euo pipefail
tree=$1 commit=$2 source=$3
if ! git -C "$tree" cat-file -e "$commit^{commit}" 2>/dev/null; then
  git -C "$tree" fetch -q "$source" "$commit"
fi
git -C "$tree" update-ref refs/remotes/origin/main "$commit"
