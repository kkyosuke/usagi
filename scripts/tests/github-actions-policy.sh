#!/usr/bin/env bash
set -euo pipefail

repo=$(cd "$(dirname "$0")/../.." && pwd)
subject=$repo/scripts/ci/github-actions-policy.rb
tmp=$(mktemp -d "${TMPDIR:-/tmp}/usagi-actions-policy.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

mkdir -p "$tmp/ok/.github/workflows"
cp "$repo/.github/workflows/test.yml" "$tmp/ok/.github/workflows/test.yml"
LC_ALL=C LANG=C ruby "$subject" "$tmp/ok"

cp -R "$tmp/ok" "$tmp/mutable"
sed -i.bak 's/actions\/checkout@[0-9a-f]*/actions\/checkout@v7/' "$tmp/mutable/.github/workflows/test.yml"
if output=$(ruby "$subject" "$tmp/mutable" 2>&1); then
  echo "expected mutable Action fixture to fail" >&2
  exit 1
fi
case "$output" in
  *"must pin actions/checkout@v7 to a full commit SHA"*) ;;
  *) echo "unexpected mutable Action failure: $output" >&2; exit 1 ;;
esac

cp -R "$tmp/ok" "$tmp/write"
sed -i.bak 's/^  contents: read$/  contents: write/' "$tmp/write/.github/workflows/test.yml"
if output=$(ruby "$subject" "$tmp/write" 2>&1); then
  echo "expected workflow write fixture to fail" >&2
  exit 1
fi
case "$output" in
  *"grants write permission at workflow scope"*) ;;
  *) echo "unexpected permission failure: $output" >&2; exit 1 ;;
esac

cp -R "$tmp/ok" "$tmp/missing"
sed -i.bak '/^permissions:$/,/^$/d' "$tmp/missing/.github/workflows/test.yml"
if output=$(ruby "$subject" "$tmp/missing" 2>&1); then
  echo "expected missing permissions fixture to fail" >&2
  exit 1
fi
case "$output" in
  *"is missing top-level permissions"*) ;;
  *) echo "unexpected missing permissions failure: $output" >&2; exit 1 ;;
esac

echo "GitHub Actions policy fixtures: ok"
