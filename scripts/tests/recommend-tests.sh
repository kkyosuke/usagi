#!/usr/bin/env bash
set -eo pipefail

script=$(cd "$(dirname "$0")/../.." && pwd)/scripts/recommend-tests.sh
tmp=$(mktemp -d "${TMPDIR:-/tmp}/usagi-recommend-tests.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

git -C "$tmp" init -q
git -C "$tmp" config user.email tests@example.com
git -C "$tmp" config user.name Tests
mkdir -p "$tmp/scripts" "$tmp/src/domain" "$tmp/src/usecase" "$tmp/src/presentation/cli" "$tmp/tests" "$tmp/third_party/vt100/src" "$tmp/v1/document"
cp "$script" "$tmp/scripts/recommend-tests.sh"
cp "${script%.sh}.tsv" "$tmp/scripts/recommend-tests.tsv"
touch "$tmp/Cargo.toml" "$tmp/src/domain/agent.rs" "$tmp/src/usecase/agent.rs" "$tmp/src/presentation/cli/agent.rs"
touch "$tmp/tests/old_name.rs" "$tmp/space name.txt" "$tmp/README.md" "$tmp/v1/document/06-conventions.md"
touch "$tmp/third_party/vt100/src/lib.rs"
git -C "$tmp" add .
git -C "$tmp" commit -qm fixture

run() { (cd "$tmp" && bash scripts/recommend-tests.sh HEAD); }
assert_has() { case "$1" in *"$2"*) ;; *) echo "missing: $2" >&2; echo "$1" >&2; exit 1 ;; esac; }
assert_not_has() { case "$1" in *"$2"*) echo "unexpected: $2" >&2; echo "$1" >&2; exit 1 ;; *) ;; esac; }

out=$(run)
assert_has "$out" "empty diff"
assert_has "$out" "cargo test --workspace --quiet"

echo changed >"$tmp/src/domain/agent.rs"
out=$(run)
assert_has "$out" "cargo test --lib domain::agent::"
assert_not_has "$out" "cargo test --workspace --quiet"
git -C "$tmp" checkout -q -- src/domain/agent.rs

echo changed >"$tmp/src/usecase/agent.rs"
out=$(run)
assert_has "$out" "cargo test --lib usecase::agent::"
assert_has "$out" "cargo test --lib domain::agent::"
assert_not_has "$out" "cargo test --workspace --quiet"
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
assert_has "$out" "cargo test --workspace --quiet"
git -C "$tmp" checkout -q -- "space name.txt"

echo changed >"$tmp/README.md"
out=$(run)
assert_has "$out" "lychee --config lychee.toml"
assert_not_has "$out" "cargo test --workspace --quiet"
git -C "$tmp" checkout -q -- README.md

echo changed >"$tmp/v1/document/06-conventions.md"
out=$(run)
assert_has "$out" "lychee --config lychee.toml"
assert_not_has "$out" "cargo test --workspace --quiet"
git -C "$tmp" checkout -q -- v1/document/06-conventions.md

echo changed >"$tmp/Cargo.toml"
out=$(run)
assert_has "$out" "shared build/test/CI surface"
assert_has "$out" "cargo test --workspace --quiet"
git -C "$tmp" checkout -q -- Cargo.toml

echo changed >"$tmp/third_party/vt100/src/lib.rs"
out=$(run)
assert_has "$out" "cargo test --manifest-path third_party/vt100/Cargo.toml"
assert_has "$out" "cargo test --test tui_e2e"
assert_has "$out" "cargo test --workspace --quiet"
git -C "$tmp" checkout -q -- third_party/vt100/src/lib.rs

echo changed >"$tmp/src/domain/agent.rs"
echo changed >"$tmp/src/presentation/cli/agent.rs"
out=$(run)
assert_has "$out" "multiple layers changed"
assert_has "$out" "cargo test --workspace --quiet"

echo "recommend-tests fixtures: ok"
