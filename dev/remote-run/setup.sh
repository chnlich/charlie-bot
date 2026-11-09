#!/usr/bin/env bash
# Install the build host's toolchain state: uv, Rust, sccache, Node, the build slots, and
# remote-exec.pl.
#
#   setup.sh
#
# Run from the directory to work in. A project with pyproject.toml needs its .venv present,
# because setup installs the CPython patch release it pins; a directory without one gets no
# Python. The state lands under the remote base, $HOME-relative on the build host; see README.md.
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd -P)
source "$SCRIPT_DIR/common.sh"

if (( $# != 0 )); then
  printf 'usage: %s\n' "$0" >&2
  exit 2
fi

remote_run_read_config
remote_run_validate_base
remote_run_local_preflight
remote_run_resolve_root
python_version=''
if remote_run_has_python_project "$REMOTE_RUN_WORKTREE_ROOT"; then
  remote_run_read_local_python_version "$REMOTE_RUN_WORKTREE_ROOT"
  python_version=$REMOTE_RUN_LOCAL_PYTHON_VERSION
fi
remote_run_connectivity_check

uv_release=0.13.1
uv_checksum=9041c73c99fdc5892cf128140beb354eca9231d502068cc36e6cb53a8e59ad14
sccache_release=0.18.0
sccache_checksum=2b3284d5da3b46a47dc4229e75bb7b88ac4aa99c8d754fb7d2f84997e5a4354a
# The node runner of the 0.1 s unit-test gate (dev/check-unit-test-time.mjs) reruns each test from the JUnit
# testcase's file attribute, which this release's reporter writes and Node 20's omits.
node_release=24.22.2
node_checksum=95c264150aceb7f3437a29fd6984924f5f8d1511ea3cbd6c508b83f6c555dae2

remote_setup=$(cat <<'REMOTE'
set -euo pipefail
umask 077
base="$HOME/__REMOTE_BASE__"
install -d -m 700 "$base" "$base/bin" "$base/python" "$base/cache/uv" \
  "$base/cargo/bin" "$base/rustup" "$base/trees" "$base/tree-owners" \
  "$base/cache/sccache" "$base/node" "$base/slots"
for tool in curl sha256sum tar perl; do
  command -v "$tool" >/dev/null || { printf 'setup: %s is missing on the build host\n' "$tool" >&2; exit 2; }
done
for index in 1 2 3 4; do
  lock="$base/slots/slot-$index.lock"
  if [[ ! -e "$lock" ]]; then
    install -m 600 /dev/null "$lock"
  fi
done

export CARGO_HOME="$base/cargo"
export RUSTUP_HOME="$base/rustup"
export UV_CACHE_DIR="$base/cache/uv"
export SCCACHE_DIR="$base/cache/sccache"
export SCCACHE_CACHE_SIZE=50G
export SCCACHE_IDLE_TIMEOUT=0
export PATH="$base/node/bin:$base/bin:$CARGO_HOME/bin:$HOME/.local/bin:$PATH"
uv="$base/bin/uv"
uv_archive="$base/uv-aarch64-unknown-linux-gnu-__UV_RELEASE__.tar.gz"
uv_url="https://releases.astral.sh/github/uv/releases/download/__UV_RELEASE__/uv-aarch64-unknown-linux-gnu.tar.gz"
if [[ ! -f "$uv_archive" ]]; then
  curl --proto '=https' --tlsv1.2 -fsSL "$uv_url" -o "$uv_archive"
fi
printf '%s  %s\n' '__UV_CHECKSUM__' "$uv_archive" | sha256sum --check --status
uv_version_line=''
if [[ -x "$uv" ]]; then
  uv_version_line=$("$uv" --version)
fi
if [[ "$uv_version_line" != 'uv __UV_RELEASE__ (aarch64-unknown-linux-gnu)' ]]; then
  tar -xzf "$uv_archive" --strip-components=1 -C "$base/bin"
  uv_version_line=$("$uv" --version)
  if [[ "$uv_version_line" != 'uv __UV_RELEASE__ (aarch64-unknown-linux-gnu)' ]]; then
    printf 'setup: expected uv __UV_RELEASE__ on the build host, got %s\n' "$uv_version_line" >&2
    exit 2
  fi
fi
if [[ ! -x "$CARGO_HOME/bin/rustup" ]]; then
  curl -fsSL https://sh.rustup.rs | sh -s -- -y --no-modify-path --default-toolchain none
fi

export UV_PYTHON_INSTALL_DIR="$base/python"
local_python_version='__LOCAL_PYTHON_VERSION__'
if [[ -n "$local_python_version" ]]; then
  "$uv" python install --no-bin "$local_python_version"
  python_path=$("$uv" python find "$local_python_version")
  host_python_version=$("$python_path" --version)
  if [[ "$host_python_version" != "Python $local_python_version" ]]; then
    printf 'setup: Python version mismatch: the local project is Python %s; the build host uv is %s\n' \
      "$local_python_version" "$host_python_version" >&2
    exit 2
  fi
fi

rustup="$CARGO_HOME/bin/rustup"
sccache="$base/bin/sccache"
sccache_archive="$base/sccache-v__SCCACHE_RELEASE__-aarch64-unknown-linux-musl.tar.gz"
sccache_url="https://github.com/mozilla/sccache/releases/download/v__SCCACHE_RELEASE__/sccache-v__SCCACHE_RELEASE__-aarch64-unknown-linux-musl.tar.gz"
sccache_version_line=''
if [[ -x "$sccache" ]]; then
  sccache_version_line=$("$sccache" --version)
fi
if [[ "$sccache_version_line" != 'sccache __SCCACHE_RELEASE__' ]]; then
  if [[ ! -f "$sccache_archive" ]]; then
    curl --proto '=https' --tlsv1.2 -fsSL "$sccache_url" -o "$sccache_archive"
  fi
  printf '%s  %s\n' '__SCCACHE_CHECKSUM__' "$sccache_archive" | sha256sum --check --status
  tar -xzf "$sccache_archive" --strip-components=1 -C "$base/bin"
  sccache_version_line=$("$sccache" --version)
  if [[ "$sccache_version_line" != 'sccache __SCCACHE_RELEASE__' ]]; then
    printf 'setup: expected sccache __SCCACHE_RELEASE__ on the build host, got %s\n' "$sccache_version_line" >&2
    exit 2
  fi
fi
node="$base/node/bin/node"
node_archive="$base/node-v__NODE_RELEASE__-linux-arm64.tar.gz"
node_url="https://nodejs.org/dist/v__NODE_RELEASE__/node-v__NODE_RELEASE__-linux-arm64.tar.gz"
node_version_line=''
if [[ -x "$node" ]] && "$base/node/bin/npm" --version >/dev/null 2>&1; then
  node_version_line=$("$node" --version)
fi
if [[ "$node_version_line" != 'v__NODE_RELEASE__' ]]; then
  if [[ ! -f "$node_archive" ]]; then
    curl --proto '=https' --tlsv1.2 -fsSL "$node_url" -o "$node_archive"
  fi
  printf '%s  %s\n' '__NODE_CHECKSUM__' "$node_archive" | sha256sum --check --status
  # A release unpacked over another mixes the two npm module trees, and npm then fails to load. The release
  # unpacks into a fresh directory that replaces the old one.
  rm -rf -- "$base/node.new" "$base/node.old"
  install -d -m 700 "$base/node.new"
  tar -xzf "$node_archive" --strip-components=1 -C "$base/node.new"
  mv -- "$base/node" "$base/node.old"
  mv -- "$base/node.new" "$base/node"
  rm -rf -- "$base/node.old"
  node_version_line=$("$node" --version)
  if [[ "$node_version_line" != 'v__NODE_RELEASE__' ]]; then
    printf 'setup: expected Node v__NODE_RELEASE__ on the build host, got %s\n' "$node_version_line" >&2
    exit 2
  fi
fi

"$rustup" toolchain install 1.99.0 --profile minimal --component clippy --component rustfmt
"$rustup" default 1.99.0
"$CARGO_HOME/bin/rustc" --version
"$CARGO_HOME/bin/cargo" --version
"$uv" --version
"$sccache" --version
"$sccache" --show-stats >/dev/null
"$node" --version
"$base/node/bin/npm" --version
REMOTE
)
remote_setup=${remote_setup//__REMOTE_BASE__/$REMOTE_RUN_REMOTE_BASE}
remote_setup=${remote_setup//__UV_RELEASE__/$uv_release}
remote_setup=${remote_setup//__UV_CHECKSUM__/$uv_checksum}
remote_setup=${remote_setup//__SCCACHE_RELEASE__/$sccache_release}
remote_setup=${remote_setup//__SCCACHE_CHECKSUM__/$sccache_checksum}
remote_setup=${remote_setup//__NODE_RELEASE__/$node_release}
remote_setup=${remote_setup//__NODE_CHECKSUM__/$node_checksum}
remote_setup=${remote_setup//__LOCAL_PYTHON_VERSION__/$python_version}

if remote_run_stream "$remote_setup"; then
  :
else
  status=$?
  printf 'setup.sh: the build host toolchain setup failed (status %s)\n' "$status" >&2
  exit "$status"
fi

remote_run_refresh_remote_exec

python_note=''
if [[ -n "$python_version" ]]; then
  python_note=", CPython $python_version"
fi
printf 'setup complete: Node %s with npm, Rust 1.99.0%s, sccache, clippy, rustfmt, and four Cargo slots are ready\n' \
  "$node_release" "$python_note"
