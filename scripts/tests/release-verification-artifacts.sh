#!/usr/bin/env bash

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
WORKFLOW="$ROOT/.github/workflows/release.yml"

grep -F 'printf '\''%s  %s\n'\'' "$SHA256" "$ASSET_FILENAME" > "$ASSET_FILENAME.sha256"' "$WORKFLOW" >/dev/null
grep -F 'printf '\''%s\n'\'' "$RELEASE_TAG" > "$ASSET_FILENAME.version"' "$WORKFLOW" >/dev/null
grep -F '${{ env.CHECKSUM_FILENAME }}' "$WORKFLOW" >/dev/null
grep -F '${{ env.VERSION_FILENAME }}' "$WORKFLOW" >/dev/null
if grep -F 'cp scripts/install.sh dist/' "$WORKFLOW" >/dev/null; then
    echo "release archive must contain only the expected binary" >&2
    exit 1
fi

echo "release verification artifact checks passed"
