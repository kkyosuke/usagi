#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "invalid release bump: expected current version and major, minor, or patch" >&2
  exit 1
fi

current=$1
bump=$2
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
"$script_dir/release-version.sh" "$current"

case "$bump" in
  major | minor | patch) ;;
  *)
    echo "invalid release bump: expected major, minor, or patch" >&2
    exit 1
    ;;
esac

increment_decimal() {
  local value=$1 result= digit next carry=1 index
  for ((index = ${#value} - 1; index >= 0; index--)); do
    digit=${value:index:1}
    if [[ $carry -eq 1 ]]; then
      case "$digit" in
        0) next=1 ;;
        1) next=2 ;;
        2) next=3 ;;
        3) next=4 ;;
        4) next=5 ;;
        5) next=6 ;;
        6) next=7 ;;
        7) next=8 ;;
        8) next=9 ;;
        9) next=0 ;;
      esac
      [[ $digit == 9 ]] || carry=0
      digit=$next
    fi
    result="${digit}${result}"
  done
  [[ $carry -eq 0 ]] || result="1${result}"
  printf '%s\n' "$result"
}

core=${current%%[-+]*}
IFS=. read -r major minor patch <<< "$core"

case "$bump" in
  major)
    major=$(increment_decimal "$major")
    minor=0
    patch=0
    ;;
  minor)
    minor=$(increment_decimal "$minor")
    patch=0
    ;;
  patch)
    patch=$(increment_decimal "$patch")
    ;;
esac

printf '%s.%s.%s\n' "$major" "$minor" "$patch"
