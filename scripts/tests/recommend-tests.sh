#!/usr/bin/env bash
set -eo pipefail

script=$(cd "$(dirname "$0")/../.." && pwd)/scripts/recommend-tests.sh
tmp=$(mktemp -d "${TMPDIR:-/tmp}/usagi-recommend-tests.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

git -C "$tmp" init -q
git -C "$tmp" config user.email tests@example.com
git -C "$tmp" config user.name Tests
mkdir -p "$tmp/scripts" "$tmp/src/domain" "$tmp/src/usecase" "$tmp/src/presentation/cli" "$tmp/tests" "$tmp/third_party/vt100/src"
cp "$script" "$tmp/scripts/recommend-tests.sh"
cp "${script%.sh}.tsv" "$tmp/scripts/recommend-tests.tsv"
touch "$tmp/src/domain/agent.rs" "$tmp/src/usecase/agent.rs" "$tmp/src/presentation/cli/agent.rs"
touch "$tmp/tests/old_name.rs" "$tmp/space name.txt"
touch "$tmp/third_party/vt100/src/lib.rs"
git -C "$tmp" add .
git -C "$tmp" commit -qm fixture

run() { (cd "$tmp" && bash scripts/recommend-tests.sh HEAD); }
assert_has() { case "$1" in *"$2"*) ;; *) echo "missing: $2" >&2; echo "$1" >&2; exit 1 ;; esac; }

out=$(run)
assert_has "$out" "empty diff"
assert_has "$out" "cargo test --quiet"

echo changed >"$tmp/src/domain/agent.rs"
out=$(run)
assert_has "$out" "cargo test --lib domain::agent::"
git -C "$tmp" checkout -q -- src/domain/agent.rs

echo changed >"$tmp/src/usecase/agent.rs"
out=$(run)
assert_has "$out" "cargo test --lib usecase::agent::"
assert_has "$out" "cargo test --lib domain::agent::"
git -C "$tmp" checkout -q -- src/usecase/agent.rs

git -C "$tmp" mv tests/old_name.rs tests/new_name.rs
out=$(run)
assert_has "$out" "cargo test --test new_name"
git -C "$tmp" reset -q --hard HEAD

rm "$tmp/tests/old_name.rs"
out=$(run)
assert_has "$out" "cargo test --test old_name"
git -C "$tmp" checkout -q -- tests/old_name.rs

echo changed >"$tmp/space name.txt"
out=$(run)
assert_has "$out" "space name.txt"
assert_has "$out" "cargo test --quiet"
git -C "$tmp" checkout -q -- "space name.txt"

echo changed >"$tmp/third_party/vt100/src/lib.rs"
out=$(run)
assert_has "$out" "cargo test --manifest-path third_party/vt100/Cargo.toml"
assert_has "$out" "cargo test --test tui_e2e"
assert_has "$out" "cargo test --quiet"
git -C "$tmp" checkout -q -- third_party/vt100/src/lib.rs

echo changed >"$tmp/src/domain/agent.rs"
echo changed >"$tmp/src/presentation/cli/agent.rs"
out=$(run)
assert_has "$out" "multiple layers changed"
assert_has "$out" "cargo test --quiet"

echo "recommend-tests fixtures: ok"
