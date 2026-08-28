#!/usr/bin/env bash
# Verify the root README still describes v2 instead of having been truncated.
#
# PR #1026 replaced the 63-line root README with the single word "fixture" and the
# regression survived 13 days on main: fmt / clippy / test / coverage only read Rust,
# and the lychee link check passes trivially on a README that has no links left.
# This checker closes that gap by asserting the README's minimum contract.
set -euo pipefail

if [ "$#" -gt 1 ]; then
  echo "usage: $0 [readme-path]" >&2
  exit 2
fi

readme=${1:-README.md}

# Guard against truncation: the accident left a single line, so require enough
# non-empty lines that an emptied or fixture-stubbed README cannot pass.
min_content_lines=20

# Links the README must keep so readers can reach the project SSoT.
required_links=(
  document/01-overview.md
  document/02-architecture.md
  document/06-conventions.md
)

if [ ! -f "$readme" ]; then
  echo "$readme: not a readable file" >&2
  exit 1
fi

failures=0
fail() {
  echo "$readme: $1" >&2
  failures=$((failures + 1))
}

first_heading=$(grep -m 1 -E '^[[:space:]]*#' "$readme" || true)
if [ "$first_heading" != "# usagi" ]; then
  fail "first heading must be '# usagi', found '${first_heading:-<none>}'"
fi

content_lines=$(grep -c -E '[^[:space:]]' "$readme" || true)
if [ "$content_lines" -lt "$min_content_lines" ]; then
  fail "only $content_lines non-empty lines; expected at least $min_content_lines (truncated?)"
fi

# Accept both the bare target and an anchored one, e.g. (document/01-overview.md#現在の実装状態).
for link in "${required_links[@]}"; do
  if ! grep -qF "($link)" "$readme" && ! grep -qF "($link#" "$readme"; then
    fail "missing required link to $link"
  fi
done

if [ "$failures" -ne 0 ]; then
  echo "root README contract: $failures problem(s)" >&2
  exit 1
fi

echo "root README contract: ok ($content_lines content lines)"
