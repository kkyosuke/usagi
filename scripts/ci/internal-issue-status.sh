#!/usr/bin/env bash
set -euo pipefail

base_sha=${1:?base SHA is required}
head_sha=${2:?head SHA is required}
event_name=${GITHUB_EVENT_NAME:-pull_request}

# Push has no PR body or assigned internal issue. The PR invocation below is the
# point where implementation and backlog state must move together.
if [[ "$event_name" != "pull_request" ]]; then
  exit 0
fi

body=${PR_BODY:-}
marker=$(printf '%s\n' "$body" | sed -nE 's/^Internal-Issue:[[:space:]]*(#[0-9]+|none)[[:space:]]*$/\1/p')
if [[ -z "$marker" ]]; then
  echo "PR body must contain exactly one 'Internal-Issue: #<number>' or 'Internal-Issue: none' line" >&2
  exit 1
fi
if [[ $(printf '%s\n' "$marker" | wc -l | tr -d ' ') != 1 ]]; then
  echo "PR body must contain exactly one Internal-Issue marker" >&2
  exit 1
fi
issue_status() {
  git show "$1:$2" 2>/dev/null | awk '
    NR == 1 { if ($0 != "---") exit; frontmatter = 1; next }
    frontmatter && $0 == "---" { exit }
    frontmatter && /^status:[[:space:]]*/ {
      sub(/^status:[[:space:]]*/, "")
      sub(/[[:space:]]*$/, "")
      print
    }
  '
}

completed=()
while IFS= read -r issue_file; do
  [[ -n "$issue_file" ]] || continue
  head_status=$(issue_status "$head_sha" "$issue_file" || true)
  base_status=$(issue_status "$base_sha" "$issue_file" || true)
  if [[ "$head_status" == "done" && "$base_status" != "done" ]]; then
    completed+=("$issue_file")
  fi
done < <(git diff --name-only --diff-filter=AM "$base_sha" "$head_sha" -- .usagi/issues)

if [[ "$marker" == "none" ]]; then
  if [[ ${#completed[@]} -ne 0 ]]; then
    echo "Internal-Issue: none cannot complete an internal issue" >&2
    exit 1
  fi
  exit 0
fi

number=${marker#\#}
shopt -s nullglob
matches=(.usagi/issues/"$number"-*.md)
if [[ ${#matches[@]} -ne 1 ]]; then
  echo "Internal-Issue #$number must resolve to exactly one tracked issue file" >&2
  exit 1
fi
issue_file=${matches[0]}
if [[ ${#completed[@]} -ne 1 || "${completed[0]:-}" != "$issue_file" ]]; then
  echo "$issue_file must be the PR's only non-done to done internal issue transition" >&2
  exit 1
fi
