#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "invalid release version: expected exactly one argument" >&2
  exit 1
fi
version=$1

# Keep workflow_dispatch input out of shell syntax, sed replacement syntax,
# generated branch names, and release titles. Cargo remains the authority for
# the manifest itself; this gate accepts the SemVer character vocabulary and
# rejects empty identifiers and leading zeroes in the three core numbers.
core='(0|[1-9][0-9]*)'
# Numeric prerelease identifiers follow the same no-leading-zero rule as the
# core. An identifier containing a letter or hyphen is alphanumeric and may
# contain leading zeroes. Build identifiers have no numeric leading-zero rule.
prerelease_identifier='(0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)'
build_identifier='[0-9A-Za-z-]+'
semver="^${core}\\.${core}\\.${core}(-${prerelease_identifier}(\\.${prerelease_identifier})*)?(\\+${build_identifier}(\\.${build_identifier})*)?$"

if [[ ! "$version" =~ $semver ]]; then
  echo "invalid release version: expected SemVer" >&2
  exit 1
fi
