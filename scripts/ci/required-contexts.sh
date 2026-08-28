#!/usr/bin/env bash
set -euo pipefail

repo_root=${REQUIRED_CONTEXTS_REPO_ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}
contract="$repo_root/.github/required-contexts.json"
jq_command=${REQUIRED_CONTEXTS_JQ:-jq}

usage() {
  cat >&2 <<'EOF'
usage:
  required-contexts.sh classify [PATH ...]
  required-contexts.sh changed BASE HEAD
  required-contexts.sh report REQUIRED CHANGES_RESULT RUN_RESULT [DEPENDENCY_RESULT ...]
  required-contexts.sh audit-workflows
  required-contexts.sh prepare-ruleset SNAPSHOT UPDATE ROLLBACK
  required-contexts.sh verify-ruleset READBACK
EOF
  exit 2
}

require_jq() {
  command -v "$jq_command" >/dev/null 2>&1 || {
    echo "required-contexts.sh: '$jq_command' is required for $1" >&2
    exit 127
  }
}

jq_run() {
  "$jq_command" "$@"
}

classify_paths() {
  local rust=false markdown=false path
  for path in "$@"; do
    case "$path" in
      *.md|lychee.toml|.github/workflows/markdown-link-check.yml) markdown=true ;;
    esac

    case "$path" in
      *.rs|Cargo.toml|Cargo.lock|*/Cargo.toml|*/Cargo.lock|build.rs|*/build.rs|rust-toolchain*|coverage-off-allowlist.json|scripts/*|.github/workflows/*|.github/actions/*)
        rust=true
        ;;
      *.md|lychee.toml|LICENSE|.gitignore|.gitattributes|document/assets/*)
        ;;
      *)
        # Unknown paths fail safe. A path may be added to the lightweight list
        # only when it cannot affect the Rust build/test/coverage contract.
        rust=true
        ;;
    esac
  done
  printf 'rust=%s\nmarkdown=%s\n' "$rust" "$markdown"
}

audit_workflows() {
  local context workflow job file section
  while IFS=$'\t' read -r context workflow job; do
    file="$repo_root/.github/workflows/$workflow"
    test -f "$file" || { echo "missing workflow: $workflow" >&2; return 1; }
    grep -Eq "^  ${job}:$" "$file" || {
      echo "missing stable job '$job' in $workflow" >&2
      return 1
    }
    section=$(awk -v job="$job" '
      $0 == "  " job ":" { found=1 }
      found && /^  [A-Za-z0-9_-]+:$/ && $0 != "  " job ":" { exit }
      found { print }
    ' "$file")
    grep -Fqx "    name: $context" <<<"$section" || {
      echo "job '$job' must declare stable name '$context' in $workflow" >&2
      return 1
    }
  done < <(jq_run -r '.required_status_checks[] | [.context, .workflow, .job] | @tsv' "$contract")
}

mutable_ruleset() {
  jq_run '{name, target, enforcement, bypass_actors, conditions, rules}' "$1"
}

case "${1:-}" in
  classify)
    shift
    classify_paths "$@"
    ;;
  changed)
    test "$#" -eq 3 || usage
    paths=()
    while IFS= read -r -d '' path; do
      paths+=("$path")
    done < <(git -C "$repo_root" diff --name-only -z "$2" "$3")
    classify_paths "${paths[@]}"
    ;;
  report)
    test "$#" -ge 4 || usage
    required=$2 changes_result=$3 run_result=$4
    test "$changes_result" = success
    case "$required:$run_result" in
      true:success|false:skipped) ;;
      *)
        echo "gate result mismatch: required=$required run_result=$run_result" >&2
        exit 1
        ;;
    esac
    shift 4
    for dependency_result in "$@"; do
      test "$dependency_result" = success
    done
    ;;
  audit-workflows)
    test "$#" -eq 1 || usage
    require_jq "$1"
    audit_workflows
    ;;
  prepare-ruleset)
    test "$#" -eq 4 || usage
    require_jq "$1"
    snapshot=$2 update=$3 rollback=$4
    test "$(jq_run -r '.id' "$snapshot")" = "$(jq_run -r '.ruleset_id' "$contract")" || {
      echo "snapshot ruleset id does not match contract" >&2
      exit 1
    }
    mutable_ruleset "$snapshot" > "$rollback"
    jq_run --slurpfile contract "$contract" '
      .rules |= map(
        if .type == "required_status_checks" then
          .parameters.required_status_checks = (
            $contract[0].required_status_checks | map({context, integration_id})
          )
        elif .type == "pull_request" then
          .parameters.required_approving_review_count =
            $contract[0].required_approving_review_count
        else . end
      )
      | .bypass_actors = $contract[0].bypass_actors
    ' "$rollback" > "$update"
    ;;
  verify-ruleset)
    test "$#" -eq 2 || usage
    require_jq "$1"
    jq_run -e --slurpfile contract "$contract" '
      .id == $contract[0].ruleset_id
      and .enforcement == "active"
      and .bypass_actors == $contract[0].bypass_actors
      and ([.rules[] | select(.type == "pull_request")
            | .parameters.required_approving_review_count] ==
          [$contract[0].required_approving_review_count])
      and ([.rules[] | select(.type == "required_status_checks")
            | .parameters.required_status_checks[].context] | sort)
          == ([$contract[0].required_status_checks[].context] | sort)
    ' "$2" >/dev/null
    ;;
  *) usage ;;
esac
