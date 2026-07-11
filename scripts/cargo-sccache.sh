#!/usr/bin/env bash
set -eo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/cargo-sccache.sh <cargo-args...>

Runs cargo with RUSTC_WRAPPER=sccache only when sccache is installed.
If sccache is unavailable, the command falls back to plain cargo.

Environment overrides:
  SCCACHE_DIR          Cache directory. Defaults to <workspace>/.usagi/cache/sccache.
  SCCACHE_CACHE_SIZE   Cache size. Defaults to 10G.
USAGE
}

if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
  usage
  exit 0
fi

if [ $# -eq 0 ]; then
  usage >&2
  exit 64
fi

root=$(git rev-parse --show-toplevel)
git_common_dir=$(git -C "$root" rev-parse --path-format=absolute --git-common-dir)
workspace_root=$(cd "$(dirname "$git_common_dir")" && pwd)

if ! command -v sccache >/dev/null 2>&1; then
  echo "warning: sccache not found; running plain cargo" >&2
  exec cargo "$@"
fi

export RUSTC_WRAPPER=${RUSTC_WRAPPER:-sccache}
export SCCACHE_DIR=${SCCACHE_DIR:-"$workspace_root/.usagi/cache/sccache"}
export SCCACHE_CACHE_SIZE=${SCCACHE_CACHE_SIZE:-10G}

mkdir -p "$SCCACHE_DIR"
exec cargo "$@"
