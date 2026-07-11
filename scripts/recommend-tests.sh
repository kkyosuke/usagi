#!/usr/bin/env bash
set -eo pipefail

root=$(git rev-parse --show-toplevel)
base=${1:-HEAD}
map_file="$root/scripts/recommend-tests.tsv"
tmp_paths=$(mktemp "${TMPDIR:-/tmp}/usagi-recommend-paths.XXXXXX")
trap 'rm -f "$tmp_paths"' EXIT

git -C "$root" diff --name-only -z --diff-filter=ACDMRTUXB "$base" -- >"$tmp_paths"

paths=()
while IFS= read -r -d '' path; do
  paths+=("$path")
done <"$tmp_paths"

echo "Changed paths:"
if [ ${#paths[@]} -eq 0 ]; then
  echo "  (empty diff)"
else
  for path in "${paths[@]}"; do printf '  %s\n' "$path"; done
fi

commands=()
reasons=()
layers=()
fallback=false

add_unique() {
  local value=$1 existing
  [ -z "$value" ] && return
  for existing in "${commands[@]}"; do [ "$existing" = "$value" ] && return; done
  commands+=("$value")
}

requires_full_test() {
  case "$1" in
    Cargo.toml|Cargo.lock|build.rs|rust-toolchain*|src/lib.rs|src/main.rs|src/test_support.rs) return 0 ;;
    scripts/*|hooks/*|.githooks/*|.github/workflows/*|lefthook*.yml) return 0 ;;
    *) return 1 ;;
  esac
}

module_for_path() {
  local path=$1
  path=${path#src/}
  path=${path%.rs}
  path=${path%/mod}
  printf '%s' "${path//\//::}"
}

for path in "${paths[@]}"; do
  matched=false
  while IFS=$'\t' read -r pattern layer reason templates; do
    [ -z "$pattern" ] && continue
    case "$pattern" in \#*) continue ;; esac
    case "$path" in
      $pattern)
        matched=true
        layers+=("$layer")
        reasons+=("$path — $reason")
        module=$(module_for_path "$path")
        leaf=${path##*/}; leaf=${leaf%.rs}
        test_name=${leaf}
        domain_command=""
        usecase_command=""
        if [ -f "$root/src/domain/$leaf.rs" ] || [ -f "$root/src/domain/$leaf/mod.rs" ]; then
          domain_command="cargo test --lib domain::$leaf::"
        fi
        if [ -f "$root/src/usecase/$leaf.rs" ] || [ -f "$root/src/usecase/$leaf/mod.rs" ]; then
          usecase_command="cargo test --lib usecase::$leaf::"
        fi
        old_ifs=$IFS; IFS='|'; read -r -a entries <<<"$templates"; IFS=$old_ifs
        for command in "${entries[@]}"; do
          command=${command//\{module\}/$module}
          command=${command//\{test\}/$test_name}
          command=${command//\{domain_command\}/$domain_command}
          command=${command//\{usecase_command\}/$usecase_command}
          add_unique "$command"
        done
        break
        ;;
    esac
  done <"$map_file"
  if requires_full_test "$path"; then
    matched=true
    reasons+=("$path — shared build/test/CI surface; fail-safe full test")
    fallback=true
  fi
  if [ "$matched" = false ]; then
    reasons+=("$path — unknown path; fail-safe full test")
    fallback=true
  fi
done

if [ ${#paths[@]} -eq 0 ]; then
  reasons+=("empty diff — no selected-test set can be proven safe")
  fallback=true
fi

first_layer=""
for layer in "${layers[@]}"; do
  if [ -n "$first_layer" ] && [ "$layer" != "$first_layer" ]; then
    reasons+=("multiple layers changed — fail-safe full test")
    fallback=true
    break
  fi
  first_layer=$layer
done

if [ "$fallback" = true ]; then add_unique "cargo test --quiet"; fi

echo "Reasons:"
for reason in "${reasons[@]}"; do printf '  %s\n' "$reason"; done
echo "Recommended commands:"
for command in "${commands[@]}"; do printf '  %s\n' "$command"; done
echo "Guardrail: selected tests are fast feedback only; they do not replace the full PR gate."
