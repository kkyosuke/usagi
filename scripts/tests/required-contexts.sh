#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "$0")/../.." && pwd)
subject="$repo_root/scripts/ci/required-contexts.sh"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

assert_classification() {
  local expected=$1
  shift
  actual=$($subject classify "$@")
  test "$actual" = "$expected" || {
    printf 'classification mismatch for %s\nexpected:\n%s\nactual:\n%s\n' "$*" "$expected" "$actual" >&2
    exit 1
  }
}

assert_classification $'rust=true\nv1_rust=false\nmarkdown=false' crates/core/src/lib.rs
assert_classification $'rust=false\nv1_rust=true\nmarkdown=false' v1/src/lib.rs
assert_classification $'rust=false\nv1_rust=false\nmarkdown=true' v1/document/06-conventions.md
assert_classification $'rust=false\nv1_rust=false\nmarkdown=true' document/06-conventions.md
assert_classification $'rust=false\nv1_rust=false\nmarkdown=false' document/assets/update-selector.svg
assert_classification $'rust=true\nv1_rust=true\nmarkdown=false' scripts/coverage.sh scripts/v1-coverage.sh
assert_classification $'rust=true\nv1_rust=false\nmarkdown=true' README.md scripts/coverage.sh
assert_classification $'rust=true\nv1_rust=false\nmarkdown=false' unknown/new-format.data

# Rust PR: all Rust aggregates report the heavy jobs; Markdown reports a skip.
for context in test full-test coverage; do
  "$subject" report true success success
done
"$subject" report false success skipped # v1-coverage
"$subject" report false success skipped
# Markdown-only PR: Rust aggregates report skips; Markdown reports its heavy job.
for context in test full-test coverage; do
  "$subject" report false success skipped
done
"$subject" report false success skipped # v1-coverage
"$subject" report true success success
# Unrelated static asset: every conditional aggregate reports a skip.
for context in test full-test coverage v1-coverage markdown-link-check; do
  "$subject" report false success skipped
done
# v1 Rust PR: only the v1 aggregate runs its heavy job.
for context in test full-test coverage; do
  "$subject" report false success skipped
done
"$subject" report true success success
if "$subject" report true success failure 2>/dev/null; then
  echo "report accepted a failed required job" >&2
  exit 1
fi
if "$subject" report false success skipped failure 2>/dev/null; then
  echo "report accepted a failed unconditional dependency" >&2
  exit 1
fi

audit_root="$tmp/audit-repo"
mkdir -p "$audit_root/.github/workflows"
cp "$repo_root/.github/required-contexts.json" "$audit_root/.github/required-contexts.json"
for workflow in test.yml enforce-pr-base.yml coverage.yml v1-coverage.yml markdown-link-check.yml; do
  cp "$repo_root/.github/workflows/$workflow" "$audit_root/.github/workflows/$workflow"
done
REQUIRED_CONTEXTS_REPO_ROOT="$audit_root" "$subject" audit-workflows
sed -i.bak 's/^    name: coverage$/    name: coverage-renamed/' \
  "$audit_root/.github/workflows/coverage.yml"
if REQUIRED_CONTEXTS_REPO_ROOT="$audit_root" "$subject" audit-workflows 2>/dev/null; then
  echo "audit-workflows accepted a renamed stable context" >&2
  exit 1
fi

cat > "$tmp/snapshot.json" <<'JSON'
{
  "id": 17627257,
  "name": "main",
  "target": "branch",
  "enforcement": "active",
  "conditions": {"ref_name": {"exclude": [], "include": ["~DEFAULT_BRANCH"]}},
  "rules": [
    {"type": "deletion"},
    {"type": "required_status_checks", "parameters": {
      "strict_required_status_checks_policy": true,
      "do_not_enforce_on_create": false,
      "required_status_checks": [{"context": "test", "integration_id": 15368}]
    }}
  ],
  "bypass_actors": []
}
JSON

$subject prepare-ruleset "$tmp/snapshot.json" "$tmp/update.json" "$tmp/rollback.json"
jq -e '.rules[] | select(.type == "required_status_checks")
  | .parameters.required_status_checks | length == 6' "$tmp/update.json" >/dev/null
jq -e '.bypass_actors == [{"actor_id":5,"actor_type":"RepositoryRole","bypass_mode":"always"}]' "$tmp/update.json" >/dev/null
jq -e '.rules[] | select(.type == "required_status_checks")
  | .parameters.required_status_checks | length == 1' "$tmp/rollback.json" >/dev/null

jq '. + {id: 17627257}' "$tmp/update.json" > "$tmp/readback.json"
$subject verify-ruleset "$tmp/readback.json"
jq '.rules |= map(if .type == "required_status_checks" then .parameters.required_status_checks = [] else . end)' \
  "$tmp/readback.json" > "$tmp/bad-readback.json"
if $subject verify-ruleset "$tmp/bad-readback.json"; then
  echo "verify-ruleset accepted missing contexts" >&2
  exit 1
fi

echo "required context fixtures passed"
