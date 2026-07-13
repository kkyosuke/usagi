#!/usr/bin/env bash
# Report whether the distributable v1 package version differs between two refs.
set -euo pipefail

if [ "$#" -ne 2 ]; then
  echo "usage: $0 <base-ref> <head-ref>" >&2
  exit 2
fi

base_ref=$1
head_ref=$2
manifest=v1/Cargo.toml

package_version() {
  git show "$1:$manifest" | awk '
    /^\[package\][[:space:]]*$/ { in_package = 1; next }
    in_package && /^\[/ { exit }
    in_package && /^[[:space:]]*version[[:space:]]*=/ {
      line = $0
      sub(/^[^=]*=[[:space:]]*"/, "", line)
      sub(/"[[:space:]]*(#.*)?$/, "", line)
      print line
      exit
    }
  '
}

for ref in "$base_ref" "$head_ref"; do
  if ! git cat-file -e "$ref:$manifest" 2>/dev/null; then
    echo "cannot read $manifest at $ref" >&2
    exit 1
  fi
done

base_version=$(package_version "$base_ref")
head_version=$(package_version "$head_ref")

if [ -z "$base_version" ] || [ -z "$head_version" ]; then
  echo "cannot read [package].version from $manifest" >&2
  exit 1
fi

echo "base version: $base_version"
echo "head version: $head_version"

if [ "$base_version" = "$head_version" ]; then
  echo "changed=false"
else
  echo "changed=true"
fi
