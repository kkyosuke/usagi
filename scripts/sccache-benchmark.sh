#!/usr/bin/env bash
set -eo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/sccache-benchmark.sh [options]

Runs reproducible local sccache benchmarks and writes raw TSV results.

Options:
  --runs N              Number of runs per case. Default: 3.
  --command COMMAND     Cargo arguments to benchmark. Default: test --quiet.
  --peer-worktree PATH  Second session worktree for cross-session warm runs.
  --output FILE         Result TSV. Default: target/sccache-benchmark/results.tsv.
  --stats-dir DIR       sccache stats directory. Default: target/sccache-benchmark/stats.
  -h, --help            Show this help.

Cases:
  baseline_cold         cargo clean; plain cargo.
  sccache_cold          zero stats; delete sccache cache; cargo clean; helper.
  sccache_warm_single   keep sccache cache; cargo clean; helper in this worktree.
  sccache_warm_multi    warm in this worktree; cargo clean; helper in --peer-worktree.
USAGE
}

runs=3
cargo_args="test --quiet"
root=$(git rev-parse --show-toplevel)
output="$root/target/sccache-benchmark/results.tsv"
stats_dir="$root/target/sccache-benchmark/stats"
peer_worktree=""

while [ $# -gt 0 ]; do
  case "$1" in
    --runs)
      runs=${2:?missing value for --runs}
      shift 2
      ;;
    --command)
      cargo_args=${2:?missing value for --command}
      shift 2
      ;;
    --peer-worktree)
      peer_worktree=${2:?missing value for --peer-worktree}
      shift 2
      ;;
    --output)
      output=${2:?missing value for --output}
      shift 2
      ;;
    --stats-dir)
      stats_dir=${2:?missing value for --stats-dir}
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown option: $1" >&2
      usage >&2
      exit 64
      ;;
  esac
done

case "$runs" in
  ''|*[!0-9]*)
    echo "--runs must be a positive integer" >&2
    exit 64
    ;;
esac
if [ "$runs" -lt 1 ]; then
  echo "--runs must be a positive integer" >&2
  exit 64
fi

if ! command -v ruby >/dev/null 2>&1; then
  echo "ruby is required for monotonic timing" >&2
  exit 1
fi
if ! command -v sccache >/dev/null 2>&1; then
  echo "sccache is required for benchmark cases; install sccache or use scripts/cargo-sccache.sh for fallback checks" >&2
  exit 1
fi

git_common_dir=$(git -C "$root" rev-parse --path-format=absolute --git-common-dir)
workspace_root=$(cd "$(dirname "$git_common_dir")" && pwd)
export SCCACHE_DIR=${SCCACHE_DIR:-"$workspace_root/.usagi/cache/sccache"}
export SCCACHE_CACHE_SIZE=${SCCACHE_CACHE_SIZE:-10G}

mkdir -p "$(dirname "$output")" "$stats_dir"
if [ ! -s "$output" ]; then
  printf 'case\trun\tworktree\tcommand\texit_code\tseconds\tstats_file\n' >"$output"
fi

now() {
  ruby -e 'printf "%.6f\n", Process.clock_gettime(Process::CLOCK_MONOTONIC)'
}

elapsed() {
  ruby -e 'printf "%.3f\n", ARGV[1].to_f - ARGV[0].to_f' "$1" "$2"
}

safe_delete_sccache_dir() {
  case "$SCCACHE_DIR" in
    */.usagi/cache/sccache|*/.usagi/cache/sccache/*) ;;
    *)
      echo "refusing to delete unexpected SCCACHE_DIR: $SCCACHE_DIR" >&2
      exit 1
      ;;
  esac
  rm -rf "$SCCACHE_DIR"
}

split_cargo_args() {
  # shellcheck disable=SC2206
  CARGO_ARGV=($cargo_args)
}

show_stats() {
  local file=$1
  sccache --show-stats >"$file" 2>&1 || true
}

run_plain() {
  local dir=$1
  split_cargo_args
  (cd "$dir" && env -u RUSTC_WRAPPER -u SCCACHE_DIR cargo "${CARGO_ARGV[@]}")
}

run_with_sccache() {
  local dir=$1
  split_cargo_args
  (cd "$dir" && SCCACHE_DIR=$SCCACHE_DIR SCCACHE_CACHE_SIZE=$SCCACHE_CACHE_SIZE scripts/cargo-sccache.sh "${CARGO_ARGV[@]}")
}

record_case() {
  local case_name=$1 run_no=$2 dir=$3 mode=$4 stats_file exit_code start end seconds
  cargo clean --manifest-path "$dir/Cargo.toml" >/dev/null
  start=$(now)
  set +e
  if [ "$mode" = plain ]; then
    run_plain "$dir"
  else
    run_with_sccache "$dir"
  fi
  exit_code=$?
  set -e
  end=$(now)
  seconds=$(elapsed "$start" "$end")
  stats_file=""
  if [ "$mode" = sccache ]; then
    stats_file="$stats_dir/${case_name}-run${run_no}.txt"
    show_stats "$stats_file"
  fi
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$case_name" "$run_no" "$dir" "$cargo_args" "$exit_code" "$seconds" "$stats_file" >>"$output"
  return "$exit_code"
}

for run_no in $(seq 1 "$runs"); do
  record_case baseline_cold "$run_no" "$root" plain

  sccache --zero-stats >/dev/null 2>&1 || true
  safe_delete_sccache_dir
  record_case sccache_cold "$run_no" "$root" sccache

  record_case sccache_warm_single "$run_no" "$root" sccache

  if [ -n "$peer_worktree" ]; then
    sccache --zero-stats >/dev/null 2>&1 || true
    cargo clean --manifest-path "$root/Cargo.toml" >/dev/null
    run_with_sccache "$root" >/dev/null
    record_case sccache_warm_multi "$run_no" "$peer_worktree" sccache
  fi
done

echo "results: $output"
echo "stats: $stats_dir"
