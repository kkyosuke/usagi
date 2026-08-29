#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
checker="$script_dir/../ci/internal-issue-status.sh"
fixture=$(mktemp -d)
trap 'rm -rf "$fixture"' EXIT

git -C "$fixture" init -q
git -C "$fixture" config user.name test
git -C "$fixture" config user.email test@example.invalid
mkdir -p "$fixture/.usagi/issues"
printf '%s\n' '---' 'number: 7' 'status: todo' '---' > "$fixture/.usagi/issues/7-work.md"
git -C "$fixture" add .
git -C "$fixture" commit -qm base
base=$(git -C "$fixture" rev-parse HEAD)

printf '%s\n' '---' 'number: 7' 'status: done' '---' > "$fixture/.usagi/issues/7-work.md"
git -C "$fixture" add .
git -C "$fixture" commit -qm done
head=$(git -C "$fixture" rev-parse HEAD)

git -C "$fixture" switch -q --detach "$base"
printf '%s\n' unrelated > "$fixture/other"
git -C "$fixture" add .
git -C "$fixture" commit -qm unrelated
unchanged=$(git -C "$fixture" rev-parse HEAD)

(
  cd "$fixture"
  PR_BODY='Internal-Issue: #7' GITHUB_EVENT_NAME=pull_request bash "$checker" "$base" "$head"
  PR_BODY='Internal-Issue: none' GITHUB_EVENT_NAME=pull_request bash "$checker" "$base" "$unchanged"
  GITHUB_EVENT_NAME=push bash "$checker" "$base" "$head"
)

for invalid in 'missing marker' $'Internal-Issue: #7\nInternal-Issue: none'; do
  if (
    cd "$fixture"
    PR_BODY="$invalid" GITHUB_EVENT_NAME=pull_request bash "$checker" "$base" "$head"
  ) >/dev/null 2>&1; then
    echo "invalid marker was accepted" >&2
    exit 1
  fi
done

assert_rejected() {
  local body=$1
  local comparison_base=$2
  local candidate=$3
  local description=$4
  if (
    cd "$fixture"
    PR_BODY="$body" GITHUB_EVENT_NAME=pull_request \
      bash "$checker" "$comparison_base" "$candidate"
  ) >/dev/null 2>&1; then
    echo "$description was accepted" >&2
    exit 1
  fi
}

assert_rejected 'Internal-Issue: #7' "$base" "$unchanged" 'an unchanged todo issue'
assert_rejected 'Internal-Issue: none' "$base" "$head" 'a done transition marked none'

git -C "$fixture" switch -q --detach "$base"
printf '%s\n' '---' 'number: 7' 'status: todo' '---' 'body changed' \
  > "$fixture/.usagi/issues/7-work.md"
git -C "$fixture" add .
git -C "$fixture" commit -qm still-todo
still_todo=$(git -C "$fixture" rev-parse HEAD)
assert_rejected 'Internal-Issue: #7' "$base" "$still_todo" 'a body-only issue edit'

git -C "$fixture" switch -q --detach "$head"
printf '%s\n' body >> "$fixture/.usagi/issues/7-work.md"
git -C "$fixture" add .
git -C "$fixture" commit -qm already-done
already_done=$(git -C "$fixture" rev-parse HEAD)
assert_rejected 'Internal-Issue: #7' "$head" "$already_done" 'an already-done issue edit'
