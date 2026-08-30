#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
VALIDATE="$ROOT/scripts/ci/release-version.sh"

for version in 0.1.0 12.34.56 1.2.3-alpha.1 1.2.3-0A.1 1.2.3+build.05 1.2.3-rc.1+build.5; do
  "$VALIDATE" "$version"
done

for version in '' 01.2.3 1.02.3 1.2.03 1.2 1.2.3-01 1.2.3-alpha.01 '1.2.3; touch injected' '1.2.3$(id)' '1.2.3&'; do
  if "$VALIDATE" "$version" >/dev/null 2>&1; then
    echo "unsafe or invalid version was accepted: $version" >&2
    exit 1
  fi
done

if "$VALIDATE" 1.2.3 ignored >/dev/null 2>&1; then
  echo "validator accepted trailing arguments" >&2
  exit 1
fi

echo "release-version fixtures: ok"
