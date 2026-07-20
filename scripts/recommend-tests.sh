#!/usr/bin/env bash
set -eo pipefail

root=$(git rev-parse --show-toplevel)
base=${1:-HEAD}
map_file=${RECOMMEND_TESTS_MAP_FILE:-"$root/scripts/recommend-tests.tsv"}

if [ "$base" = "--validate-map" ]; then
  metadata=$(mktemp "${TMPDIR:-/tmp}/usagi-recommend-metadata.XXXXXX")
  trap 'rm -f "$metadata"' EXIT
  cargo metadata --manifest-path "$root/Cargo.toml" --no-deps --format-version 1 >"$metadata"
  ruby "$root/scripts/validate-recommend-tests.rb" "$root" "$map_file" "$metadata"
  exit
fi

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
areas=()
fallback=false
fallback_reasons=()

add_unique() {
  local value=$1 existing
  [ -z "$value" ] && return
  for existing in "${commands[@]}"; do [ "$existing" = "$value" ] && return; done
  commands+=("$value")
}

requires_full_test() {
  case "$1" in
    Cargo.toml|*/Cargo.toml|Cargo.lock|*/Cargo.lock|build.rs|*/build.rs|rust-toolchain*|src/lib.rs|src/main.rs|src/test_support.rs) return 0 ;;
    crates/core/src/infrastructure/ipc/*|crates/*/src/test_support.rs|tests/support/*) return 0 ;;
    scripts/*|hooks/*|.githooks/*|.github/workflows/*|lefthook*.yml) return 0 ;;
    *) return 1 ;;
  esac
}

for path in "${paths[@]}"; do
  matched=false
  while IFS=$'\t' read -r pattern area reason templates witness; do
    [ -z "$pattern" ] && continue
    case "$pattern" in \#*) continue ;; esac
    case "$path" in
      $pattern)
        matched=true
        areas+=("$area")
        reasons+=("$path — $reason")
        leaf=${path##*/}; leaf=${leaf%.rs}
        test_name=${leaf}
        old_ifs=$IFS; IFS='|'; read -r -a entries <<<"$templates"; IFS=$old_ifs
        for command in "${entries[@]}"; do
          command=${command//\{test\}/$test_name}
          add_unique "$command"
        done
        break
        ;;
    esac
  done <"$map_file"
  if requires_full_test "$path"; then
    matched=true
    fallback_reasons+=("$path — shared build/test/CI surface")
    fallback=true
  fi
  if [ "$matched" = false ]; then
    fallback_reasons+=("$path — unknown path")
    fallback=true
  fi
done

if [ ${#paths[@]} -eq 0 ]; then
  fallback_reasons+=("empty diff — no selected-test set can be proven safe")
  fallback=true
fi

first_area=""
for area in "${areas[@]}"; do
  if [ -n "$first_area" ] && [ "$area" != "$first_area" ]; then
    fallback_reasons+=("multiple areas changed ($first_area, $area)")
    fallback=true
    break
  fi
  first_area=$area
done

if [ "$fallback" = true ]; then add_unique "cargo test --workspace --quiet"; fi

echo "Reasons:"
for reason in "${reasons[@]}"; do printf '  %s\n' "$reason"; done
if [ "$fallback" = true ]; then
  echo "Fallback: full workspace test"
  echo "Fallback reasons:"
  for reason in "${fallback_reasons[@]}"; do printf '  %s\n' "$reason"; done
else
  echo "Fallback: none"
fi
echo "Recommended commands:"
for command in "${commands[@]}"; do printf '  %s\n' "$command"; done
echo "Guardrail: selected tests are fast feedback only; they do not replace the full PR gate."
