#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
NEXT="$ROOT/scripts/ci/next-release-version.sh"
WORKFLOW="$ROOT/.github/workflows/create-release-pr.yml"

assert_version() {
  local current=$1 bump=$2 expected=$3 actual
  actual="$("$NEXT" "$current" "$bump")"
  if [[ "$actual" != "$expected" ]]; then
    echo "expected $current + $bump to produce $expected, got $actual" >&2
    exit 1
  fi
}

assert_version 3.3.4 patch 3.3.5
assert_version 3.3.4 minor 3.4.0
assert_version 3.3.4 major 4.0.0
assert_version 1.2.3-rc.1+build.5 patch 1.2.4
assert_version 999999999999999999.9.9 major 1000000000000000000.0.0
assert_version 1.999999999999999999.9 minor 1.1000000000000000000.0
assert_version 1.2.999999999999999999 patch 1.2.1000000000000000000

for arguments in '1.2.3 build' '1.2 invalid' 'invalid patch' '1.2.3 patch extra'; do
  read -r -a words <<< "$arguments"
  if "$NEXT" "${words[@]}" >/dev/null 2>&1; then
    echo "invalid release bump was accepted: $arguments" >&2
    exit 1
  fi
done

grep -F 'type: choice' "$WORKFLOW" >/dev/null
grep -F 'RELEASE_BUMP: ${{ inputs.bump }}' "$WORKFLOW" >/dev/null
grep -F 'scripts/ci/next-release-version.sh "$CURRENT_VERSION" "$RELEASE_BUMP"' "$WORKFLOW" >/dev/null
grep -F 'steps.version.outputs.version' "$WORKFLOW" >/dev/null
grep -F 'RELEASE_PR_TOKEN: ${{ secrets.RELEASE_PR_TOKEN }}' "$WORKFLOW" >/dev/null
grep -F 'token: ${{ secrets.RELEASE_PR_TOKEN }}' "$WORKFLOW" >/dev/null
grep -F 'id: create-pr' "$WORKFLOW" >/dev/null
grep -F "if: steps.create-pr.outputs.pull-request-number != ''" "$WORKFLOW" >/dev/null
grep -F 'gh pr merge "$PR_NUMBER" --repo "$GITHUB_REPOSITORY" --squash --auto' "$WORKFLOW" >/dev/null
for option in major minor patch; do
  grep -F -- "- $option" "$WORKFLOW" >/dev/null
done
if grep -F '${{ inputs.version }}' "$WORKFLOW" >/dev/null; then
  echo "release workflow still consumes a free-form version input" >&2
  exit 1
fi
if grep -F 'token: ${{ secrets.GITHUB_TOKEN }}' "$WORKFLOW" >/dev/null; then
  echo "release workflow still creates pull requests with GITHUB_TOKEN" >&2
  exit 1
fi

echo "next-release-version fixtures: ok"
