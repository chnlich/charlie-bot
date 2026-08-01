#!/usr/bin/env bash
# Builds web/static/css/tailwind.css from tailwind.config.js via the Tailwind CLI.
# Idempotent: safe to rerun any time the templates/JS class usage changes.
# Usage: build-css.sh [output-path]  (default: web/static/css/tailwind.css)
set -euo pipefail

# Resolve paths relative to this script so the build works from any cwd.
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/.." && pwd)
cd "$REPO_ROOT"

# node/npm/npx are not on PATH in a non-login shell on this host.
NODE_BIN_DIR="$HOME/.local/nodeenvs/charliebot-node-20/bin"
if [[ -d "$NODE_BIN_DIR" ]]; then
  PATH="$NODE_BIN_DIR:$PATH"
fi

if ! command -v npm >/dev/null 2>&1; then
  echo "error: npm not found on PATH (looked in $NODE_BIN_DIR)" >&2
  exit 1
fi

# Installs the pinned tailwindcss devDependency into node_modules/ (gitignored).
npm install --no-audit --no-fund

ENTRY_CSS=$(mktemp -t tailwind-entry-XXXXXX.css)
trap 'rm -f "$ENTRY_CSS"' EXIT
cat > "$ENTRY_CSS" <<'CSS'
@tailwind base;
@tailwind components;
@tailwind utilities;
CSS

OUT_CSS="${1:-$REPO_ROOT/web/static/css/tailwind.css}"
mkdir -p "$(dirname "$OUT_CSS")"

npx tailwindcss \
  --config "$REPO_ROOT/tailwind.config.js" \
  --input "$ENTRY_CSS" \
  --output "$OUT_CSS"

echo "Built $OUT_CSS"
