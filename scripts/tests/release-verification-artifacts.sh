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

# Release availability must not depend on a separate AI notes job.
ruby -ryaml -e '
  release = YAML.load_file(ARGV[0])
  auto = YAML.load_file(ARGV[1])
  jobs = release.fetch("jobs")
  abort "unexpected release prerequisite job" unless jobs.keys == ["build-and-release"]
  build = jobs.fetch("build-and-release")
  abort "release must not wait for notes" if build.key?("needs")
  publish = build.fetch("steps").find { |step| step.fetch("uses", "").start_with?("softprops/action-gh-release@") }
  options = publish.fetch("with")
  abort "GitHub release notes must be enabled" unless options["generate_release_notes"] == true
  abort "AI notes must not be injected" if options.key?("body")
  [release, auto].each do |workflow|
    workflow.fetch("jobs").each_value do |job|
      abort "Models permission is unnecessary" if job.fetch("permissions", {}).key?("models")
    end
  end
' "$WORKFLOW" "$ROOT/.github/workflows/auto-release.yml"

echo "release verification artifact checks passed"
