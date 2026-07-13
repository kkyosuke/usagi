#!/usr/bin/env bash
set -euo pipefail

script=$(cd "$(dirname "$0")/../.." && pwd)/scripts/ci/v1-version-changed.sh
tmp=$(mktemp -d "${TMPDIR:-/tmp}/usagi-v1-version-changed.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

git -C "$tmp" init -q
git -C "$tmp" config user.email tests@example.com
git -C "$tmp" config user.name Tests
mkdir -p "$tmp/v1"

write_manifest() {
  cat >"$tmp/v1/Cargo.toml" <<EOF
[package]
name = "usagi"
version = "$1"
description = "fixture"
EOF
}

write_manifest 1.0.0
git -C "$tmp" add v1/Cargo.toml
git -C "$tmp" commit -qm base
base=$(git -C "$tmp" rev-parse HEAD)

echo '# unrelated package metadata' >>"$tmp/v1/Cargo.toml"
git -C "$tmp" commit -am metadata -q
same_version=$(git -C "$tmp" rev-parse HEAD)
out=$(cd "$tmp" && "$script" "$base" "$same_version")
case "$out" in *"changed=false"*) ;; *) echo "expected unchanged version" >&2; exit 1 ;; esac

write_manifest 1.1.0
git -C "$tmp" add v1/Cargo.toml
git -C "$tmp" commit -qm version-bump
bumped_version=$(git -C "$tmp" rev-parse HEAD)
out=$(cd "$tmp" && "$script" "$base" "$bumped_version")
case "$out" in *"changed=true"*) ;; *) echo "expected changed version" >&2; exit 1 ;; esac

echo "v1-version-changed fixtures: ok"
