#!/usr/bin/env bash
set -eo pipefail

repo=$(cd "$(dirname "$0")/../.." && pwd)
script="$repo/scripts/recommend-tests.sh"
map="$repo/scripts/recommend-tests.tsv"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/usagi-recommend-tests.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

assert_has() { case "$1" in *"$2"*) ;; *) echo "missing: $2" >&2; echo "$1" >&2; exit 1 ;; esac; }
assert_not_has() { case "$1" in *"$2"*) echo "unexpected: $2" >&2; echo "$1" >&2; exit 1 ;; *) ;; esac; }
assert_count() {
  local output=$1 needle=$2 expected=$3 actual
  actual=$(grep -Foc "$needle" <<<"$output" || true)
  [ "$actual" -eq "$expected" ] || { echo "expected $expected occurrences of $needle, got $actual" >&2; echo "$output" >&2; exit 1; }
}

# The checked-in table must be reachable and refer only to real v2 packages/targets.
(cd "$repo" && bash scripts/recommend-tests.sh --validate-map)

# Validator regressions: a shadowed rule and stale package/target references must fail.
cp "$map" "$tmp/shadow.tsv"
tail -n 1 "$map" >>"$tmp/shadow.tsv"
if (cd "$repo" && RECOMMEND_TESTS_MAP_FILE="$tmp/shadow.tsv" bash scripts/recommend-tests.sh --validate-map) >"$tmp/validation.out" 2>&1; then
  echo "shadowed rule unexpectedly passed validation" >&2
  exit 1
fi
assert_has "$(<"$tmp/validation.out")" "is shadowed"

sed 's/cargo test -p usagi-core/cargo test -p missing-package/' "$map" >"$tmp/missing-package.tsv"
if (cd "$repo" && RECOMMEND_TESTS_MAP_FILE="$tmp/missing-package.tsv" bash scripts/recommend-tests.sh --validate-map) >"$tmp/validation.out" 2>&1; then
  echo "missing package unexpectedly passed validation" >&2
  exit 1
fi
assert_has "$(<"$tmp/validation.out")" "missing package"

sed 's/--test {test}/--test absent_target/' "$map" >"$tmp/missing-target.tsv"
if (cd "$repo" && RECOMMEND_TESTS_MAP_FILE="$tmp/missing-target.tsv" bash scripts/recommend-tests.sh --validate-map) >"$tmp/validation.out" 2>&1; then
  echo "missing target unexpectedly passed validation" >&2
  exit 1
fi
assert_has "$(<"$tmp/validation.out")" "has no test target absent_target"

git -C "$tmp" init -q
git -C "$tmp" config user.email tests@example.com
git -C "$tmp" config user.name Tests
mkdir -p "$tmp/scripts" "$tmp/crates"/{core,daemon,tui,cli}/src "$tmp/crates"/{core,daemon,tui}/tests
mkdir -p "$tmp/crates/core/src/infrastructure/ipc" "$tmp/src/runtime" "$tmp/tests/support" "$tmp/v1/src" "$tmp/.github/workflows"
mkdir -p "$tmp/crates/core/src/domain" "$tmp/crates/core/src/usecase" "$tmp/crates/daemon/src/usecase"
mkdir -p "$tmp/crates/tui/src/presentation" "$tmp/crates/cli/src/mcp"
cp "$script" "$tmp/scripts/recommend-tests.sh"
cp "$map" "$tmp/scripts/recommend-tests.tsv"
touch "$tmp/Cargo.toml" "$tmp/crates/core/Cargo.toml" "$tmp/crates/core/src/lib.rs" "$tmp/crates/core/src/other.rs" "$tmp/crates/core/src/infrastructure/ipc/mod.rs"
touch "$tmp/crates/core/tests/agent_contract.rs" "$tmp/crates/daemon/src/lib.rs" "$tmp/crates/daemon/tests/agent_real_pty.rs"
touch "$tmp/crates/tui/src/lib.rs" "$tmp/crates/tui/tests/parity_suite.rs" "$tmp/crates/cli/src/lib.rs"
touch "$tmp/crates/core/src/domain/issue.rs" "$tmp/crates/core/src/usecase/issue.rs" "$tmp/crates/daemon/src/usecase/start.rs"
touch "$tmp/crates/tui/src/presentation/frame.rs" "$tmp/crates/cli/src/mcp/serve.rs"
touch "$tmp/src/runtime/cli.rs" "$tmp/src/runtime/daemon.rs" "$tmp/src/runtime/tui.rs" "$tmp/src/runtime/bootstrap.rs" "$tmp/tests/agent_ipc_e2e.rs"
touch "$tmp/tests/support/mod.rs" "$tmp/v1/src/main.rs" "$tmp/.github/workflows/test.yml" "$tmp/unknown.file"
git -C "$tmp" add .
git -C "$tmp" commit -qm fixture

run() { (cd "$tmp" && bash scripts/recommend-tests.sh HEAD); }
restore() { git -C "$tmp" checkout -q -- "$@"; }
change() { echo changed >"$tmp/$1"; }

out=$(run)
assert_has "$out" "Fallback: full workspace test"
assert_has "$out" "empty diff — no selected-test set can be proven safe"

for fixture in \
  "crates/core/src/lib.rs|cargo test -p usagi-core" \
  "crates/core/src/domain/issue.rs|cargo test -p usagi-core" \
  "crates/core/src/usecase/issue.rs|cargo test -p usagi-core" \
  "crates/daemon/src/lib.rs|cargo test -p usagi-daemon" \
  "crates/daemon/src/usecase/start.rs|cargo test -p usagi-daemon" \
  "crates/tui/src/lib.rs|cargo test -p usagi-tui" \
  "crates/tui/src/presentation/frame.rs|cargo test -p usagi-tui" \
  "crates/cli/src/lib.rs|cargo test -p usagi-cli" \
  "crates/cli/src/mcp/serve.rs|cargo test -p usagi-cli" \
  "src/runtime/cli.rs|cargo test -p usagi --bin usagi" \
  "src/runtime/daemon.rs|cargo test -p usagi --bin usagi" \
  "src/runtime/tui.rs|cargo test -p usagi --bin usagi" \
  "src/runtime/bootstrap.rs|cargo test -p usagi --bin usagi" \
  "crates/core/tests/agent_contract.rs|cargo test -p usagi-core --test agent_contract" \
  "crates/daemon/tests/agent_real_pty.rs|cargo test -p usagi-daemon --test agent_real_pty" \
  "crates/tui/tests/parity_suite.rs|cargo test -p usagi-tui --test parity_suite" \
  "tests/agent_ipc_e2e.rs|cargo test -p usagi --test agent_ipc_e2e" \
  "v1/src/main.rs|cargo test --manifest-path v1/Cargo.toml --quiet"
do
  path=${fixture%%|*}; command=${fixture#*|}
  change "$path"
  out=$(run)
  assert_has "$out" "$command"
  assert_has "$out" "Fallback: none"
  assert_not_has "$out" "cargo test --workspace --quiet"
  restore "$path"
done

for path in Cargo.toml crates/core/Cargo.toml crates/core/src/infrastructure/ipc/mod.rs tests/support/mod.rs .github/workflows/test.yml unknown.file; do
  change "$path"
  out=$(run)
  assert_has "$out" "Fallback: full workspace test"
  assert_has "$out" "$path —"
  assert_has "$out" "cargo test --workspace --quiet"
  restore "$path"
done

# Commands shared by more than one file in one area are emitted once.
change crates/core/src/lib.rs
change crates/core/src/other.rs
out=$(run)
assert_has "$out" "Fallback: none"
assert_count "$out" "cargo test -p usagi-core" 1
restore crates/core/src/lib.rs crates/core/src/other.rs

# Crossing responsibility areas remains fail-safe even when each path is known.
change crates/core/src/lib.rs
change crates/tui/src/lib.rs
out=$(run)
assert_has "$out" "multiple areas changed (core, tui)"
assert_has "$out" "cargo test --workspace --quiet"

echo "recommend-tests fixtures: ok"
