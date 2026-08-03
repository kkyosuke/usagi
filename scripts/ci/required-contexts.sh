#!/usr/bin/env bash
set -euo pipefail

repo_root=${REQUIRED_CONTEXTS_REPO_ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}
contract="$repo_root/.github/required-contexts.json"

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

classify_paths() {
  local rust=false v1_rust=false markdown=false path
  for path in "$@"; do
    case "$path" in
      *.md|lychee.toml|.github/workflows/markdown-link-check.yml) markdown=true ;;
    esac

    case "$path" in
      v1/*.rs|v1/Cargo.toml|v1/Cargo.lock|v1/build.rs|scripts/v1-coverage.sh|.github/workflows/v1-coverage.yml|.github/workflows/v1-test.yml)
        v1_rust=true
        ;;
    esac

    case "$path" in
      v1/*)
        # v1 is an independent Cargo project. Its shipping gate is classified
        # separately so a v1-only PR never measures the v2 workspace.
        ;;
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
  printf 'rust=%s\nv1_rust=%s\nmarkdown=%s\n' "$rust" "$v1_rust" "$markdown"
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
  done < <(jq -r '.required_status_checks[] | [.context, .workflow, .job] | @tsv' "$contract")
}

mutable_ruleset() {
  jq '{name, target, enforcement, bypass_actors, conditions, rules}' "$1"
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
    audit_workflows
    ;;
  prepare-ruleset)
    test "$#" -eq 4 || usage
    snapshot=$2 update=$3 rollback=$4
    test "$(jq -r '.id' "$snapshot")" = "$(jq -r '.ruleset_id' "$contract")" || {
      echo "snapshot ruleset id does not match contract" >&2
      exit 1
    }
    mutable_ruleset "$snapshot" > "$rollback"
    jq --slurpfile contract "$contract" '
      .rules |= map(
        if .type == "required_status_checks" then
          .parameters.required_status_checks = (
            $contract[0].required_status_checks | map({context, integration_id})
          )
        else . end
      )
      | .bypass_actors = $contract[0].bypass_actors
    ' "$rollback" > "$update"
    ;;
  verify-ruleset)
    test "$#" -eq 2 || usage
    jq -e --slurpfile contract "$contract" '
      .id == $contract[0].ruleset_id
      and .enforcement == "active"
      and .bypass_actors == $contract[0].bypass_actors
      and ([.rules[] | select(.type == "required_status_checks")
            | .parameters.required_status_checks[].context] | sort)
          == ([$contract[0].required_status_checks[].context] | sort)
    ' "$2" >/dev/null
    ;;
  *) usage ;;
esac
