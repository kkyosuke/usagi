#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
lint="$root/scripts/coverage-off-lint.rb"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

run_case() {
  local name=$1 expected=$2 pattern=$3
  local case_root="$tmp/$name"
  shift 3
  mkdir -p "$case_root/src"
  "$@" "$case_root"
  set +e
  output=$(ruby "$lint" --root "$case_root" --manifest allowlist.json --today 2026-07-21 2>&1)
  status=$?
  set -e
  if [[ $status -ne $expected ]]; then
    echo "FAIL: $name returned $status, expected $expected" >&2
    echo "$output" >&2
    exit 1
  fi
  if [[ -n $pattern ]] && ! grep -Fq "$pattern" <<<"$output"; then
    echo "FAIL: $name did not contain: $pattern" >&2
    echo "$output" >&2
    exit 1
  fi
}

allowed_io() {
  local dir=$1
  printf '%s\n' '#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=pty_integration' 'fn read_pty() {}' > "$dir/src/lib.rs"
  printf '%s\n' '{"version":1,"entries":[]}' > "$dir/allowlist.json"
}

forbidden_reducer() {
  local dir=$1
  printf '%s\n' '#[coverage(off)] // coverage: reason=reducer owner=core expires=2027-01-31 tests=reducer_test' 'fn reduce() {}' > "$dir/src/lib.rs"
  printf '%s\n' '{"version":1,"entries":[]}' > "$dir/allowlist.json"
}

missing_reason() {
  local dir=$1
  printf '%s\n' '#[coverage(off)] // coverage: owner=root expires=2027-01-31 tests=composition_test' 'fn compose() {}' > "$dir/src/lib.rs"
  printf '%s\n' '{"version":1,"entries":[]}' > "$dir/allowlist.json"
}

stale_symbol() {
  local dir=$1
  printf '%s\n' 'fn renamed() {}' > "$dir/src/lib.rs"
  printf '%s\n' '{"version":1,"entries":[{"path":"src/lib.rs","symbol":"fn:old","reason":"migration_debt","owner":"core","expires":"2027-01-31","tracking":"#485"}]}' > "$dir/allowlist.json"
}

added_symbol() {
  local dir=$1
  printf '%s\n' '#[coverage(off)]' 'fn existing() {}' '#[coverage(off)]' 'fn added() {}' > "$dir/src/lib.rs"
  printf '%s\n' '{"version":1,"entries":[{"path":"src/lib.rs","symbol":"fn:existing","reason":"migration_debt","owner":"core","expires":"2027-01-31","tracking":"#485"}]}' > "$dir/allowlist.json"
}

deleted_symbol() {
  local dir=$1
  printf '%s\n' '#[coverage(off)]' 'fn remaining() {}' > "$dir/src/lib.rs"
  printf '%s\n' '{"version":1,"entries":[{"path":"src/lib.rs","symbol":"fn:remaining","reason":"migration_debt","owner":"core","expires":"2027-01-31","tracking":"#485"},{"path":"src/lib.rs","symbol":"fn:deleted","reason":"migration_debt","owner":"core","expires":"2027-01-31","tracking":"#485"}]}' > "$dir/allowlist.json"
}

expired() {
  local dir=$1
  printf '%s\n' '#[coverage(off)] // coverage: reason=composition owner=root expires=2026-07-20 tests=cli_integration' 'fn main() {}' > "$dir/src/main.rs"
  printf '%s\n' '{"version":1,"entries":[]}' > "$dir/allowlist.json"
}

run_case allowed-io 0 'ok (1 exclusions)' allowed_io
run_case forbidden-reducer 1 'forbidden reason "reducer"' forbidden_reducer
run_case missing-reason 1 'missing reason' missing_reason
run_case stale-symbol 1 'stale symbol src/lib.rs:fn:old:1' stale_symbol
run_case added-symbol 1 'unregistered coverage(off) for fn:added' added_symbol
run_case deleted-symbol 1 'stale symbol src/lib.rs:fn:deleted:1' deleted_symbol
run_case expired 1 'expired on 2026-07-20' expired

echo "coverage-off-lint: fixtures ok"
