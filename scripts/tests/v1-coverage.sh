#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "$0")/../.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

crate="$tmp/fixture"
mkdir -p "$crate/src"
cat > "$crate/Cargo.toml" <<'TOML'
[package]
name = "v1-coverage-fixture"
version = "0.0.0"
edition = "2021"

[workspace]
TOML
cat > "$crate/src/lib.rs" <<'RS'
pub fn covered() -> u8 { 1 }

#[cfg(test)]
mod tests {
    #[test]
    fn covers_everything() { assert_eq!(super::covered(), 1); }
}
RS

export V1_COVERAGE_MANIFEST="$crate/Cargo.toml"
export CARGO_TARGET_DIR="$tmp/target"
. "$repo_root/scripts/v1-coverage.sh"

v1_coverage_enforce >/dev/null

cat >> "$crate/src/lib.rs" <<'RS'

pub fn uncovered() -> u8 { 2 }
RS
if v1_coverage_enforce >"$tmp/failure.log" 2>&1; then
  echo "v1 coverage accepted an uncovered line/function" >&2
  exit 1
fi
grep -Eq '^TOTAL.+66\.67%.+66\.67%' "$tmp/failure.log" || {
  cat "$tmp/failure.log" >&2
  echo "v1 coverage failure did not expose the uncovered function/line totals" >&2
  exit 1
}

echo "v1 coverage fixtures passed"
