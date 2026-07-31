#!/usr/bin/env bash
set -euo pipefail

repo=$(cd "$(dirname "$0")/../.." && pwd)
script=$repo/scripts/ci/root-readme.sh
tmp=$(mktemp -d "${TMPDIR:-/tmp}/usagi-root-readme.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

# A minimal README that satisfies every clause of the contract.
write_valid() {
  {
    echo '# usagi'
    echo
    echo '`usagi` をゼロから作り直す v2 の開発ツリー。'
    echo
    echo '旧実装は [v1/](v1/README.md) に退避してある。'
    echo '構成は [document/02-architecture.md](document/02-architecture.md) が正本。'
    echo '規約は [document/06-conventions.md](document/06-conventions.md) に従う。'
    echo '実装状態は [v2 の実装状態](document/01-overview.md#現在の実装状態) を参照する。'
    echo 'リファレンスは [v1/document/](v1/document/README.md) を参照する。'
    echo
    # Pad past the truncation floor so link/heading cases fail for their own reason.
    for i in $(seq 1 20); do
      echo "- 本文 $i"
    done
  } >"$1"
}

expect_ok() {
  if ! out=$("$script" "$2" 2>&1); then
    echo "expected $1 to pass, got: $out" >&2
    exit 1
  fi
}

expect_fail() {
  if out=$("$script" "$2" 2>&1); then
    echo "expected $1 to fail, got: $out" >&2
    exit 1
  fi
  case "$out" in
    *"$3"*) ;;
    *) echo "expected $1 to report '$3', got: $out" >&2; exit 1 ;;
  esac
}

# The real README in this checkout must satisfy the contract.
expect_ok "repository README" "$repo/README.md"

write_valid "$tmp/valid.md"
expect_ok "valid fixture" "$tmp/valid.md"

# The exact shape of the PR #1026 regression: the whole README replaced by one word.
echo 'fixture' >"$tmp/truncated.md"
expect_fail "PR #1026 truncation" "$tmp/truncated.md" "non-empty lines"

: >"$tmp/empty.md"
expect_fail "empty README" "$tmp/empty.md" "non-empty lines"

# A wrong or missing top heading is rejected even when the body is long enough.
write_valid "$tmp/wrong-heading.md"
sed '1s/.*/# something else/' "$tmp/wrong-heading.md" >"$tmp/wrong-heading.tmp"
mv "$tmp/wrong-heading.tmp" "$tmp/wrong-heading.md"
expect_fail "wrong heading" "$tmp/wrong-heading.md" "must be '# usagi'"

write_valid "$tmp/no-heading.md"
sed '1d' "$tmp/no-heading.md" >"$tmp/no-heading.tmp"
mv "$tmp/no-heading.tmp" "$tmp/no-heading.md"
expect_fail "missing heading" "$tmp/no-heading.md" "must be '# usagi'"

# Each required link is individually enforced.
for link in \
  document/01-overview.md \
  document/02-architecture.md \
  document/06-conventions.md \
  v1/README.md \
  v1/document/README.md
do
  write_valid "$tmp/no-link.md"
  grep -vF "($link" "$tmp/no-link.md" >"$tmp/no-link.tmp"
  mv "$tmp/no-link.tmp" "$tmp/no-link.md"
  expect_fail "README without $link" "$tmp/no-link.md" "missing required link to $link"
done

# An anchored link satisfies the requirement just like a bare one.
write_valid "$tmp/anchored.md"
sed 's|(document/02-architecture.md)|(document/02-architecture.md#依存ルール)|' \
  "$tmp/anchored.md" >"$tmp/anchored.tmp"
mv "$tmp/anchored.tmp" "$tmp/anchored.md"
expect_ok "anchored architecture link" "$tmp/anchored.md"

expect_fail "missing file" "$tmp/absent.md" "not a readable file"

if "$script" a b 2>/dev/null; then
  echo "expected too many arguments to fail" >&2
  exit 1
fi

echo "root-readme fixtures: ok"
