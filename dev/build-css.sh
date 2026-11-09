#!/usr/bin/env bash
# Builds the two committed Tailwind stylesheets via the Tailwind CLI:
#   web/static/css/tailwind.css         the app pages, from web/tailwind.config.js
#   web/static/css/events-tailwind.css  the events viewer, from web/tailwind.events.config.js
# Idempotent: safe to rerun any time the templates/JS class usage changes.
set -euo pipefail

# Resolve paths relative to this script so the build works from any cwd.
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/.." && pwd)
WEB_DIR="$REPO_ROOT/web"
cd "$WEB_DIR"

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

build_sheet() {
  local config=$1 output=$2
  npx tailwindcss --config "$WEB_DIR/$config" --input "$ENTRY_CSS" --output "$WEB_DIR/$output"
  echo "Built $WEB_DIR/$output"
}

build_sheet tailwind.config.js static/css/tailwind.css
build_sheet tailwind.events.config.js static/css/events-tailwind.css
