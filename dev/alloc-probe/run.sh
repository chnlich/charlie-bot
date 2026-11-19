#!/usr/bin/env bash
# Builds the allocator probe once per allocator and runs every build on the same workload.
#
#   dev/alloc-probe/run.sh [runs-per-variant]
#
# Each output line is one run of one variant: the variant name, the probe's RSS figures in kB, then the run's CPU
# seconds. The variants:
#   glibc              glibc malloc (the server before the allocator change)
#   mimalloc           mimalloc at its default purge delay (1000 ms)
#   mimalloc-purge0    mimalloc with purge delay 0, the server's configuration
#   jemalloc           jemalloc at its default decay (10 s dirty, muzzy)
#   jemalloc-decay0    jemalloc with dirty and muzzy decay 0, the same purge-at-once intent
set -euo pipefail

cd "$(dirname "$0")"
runs=${1:-2}

# A host's cargo config can put Android NDK headers in CPATH, which breaks the C builds of mimalloc and jemalloc.
export CPATH=

BIN_DIR=target/bins
mkdir -p "$BIN_DIR"

build() {
  local name=$1
  shift
  cargo build --release "$@" > /dev/null
  cp target/release/alloc-probe "$BIN_DIR/$name"
}

build glibc
build mimalloc --features mimalloc
build mimalloc-purge0 --features mimalloc-purge0
build jemalloc --features jemalloc
build jemalloc-decay0 --features jemalloc

TIMEFORMAT='cpu_user_s=%U cpu_sys_s=%S'
for _ in $(seq "$runs"); do
  for name in glibc mimalloc mimalloc-purge0 jemalloc jemalloc-decay0; do
    if [ "$name" = jemalloc-decay0 ]; then
      export _RJEM_MALLOC_CONF=dirty_decay_ms:0,muzzy_decay_ms:0
    else
      unset _RJEM_MALLOC_CONF
    fi
    printf 'variant=%s ' "$name"
    { time "$BIN_DIR/$name" ; } 2>&1 | tr '\n' ' ' | sed 's/$/\n/'
  done
done
