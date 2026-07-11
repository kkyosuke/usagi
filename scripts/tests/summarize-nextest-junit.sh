#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

for run in 1 2; do
  cat > "$tmp/run-$run.xml" <<XML
<testsuites><testsuite><testcase classname="suite" name="slow" time="$((run + 1)).0"/><testcase classname="suite" name="fast" time="0.${run}"/></testsuite></testsuites>
XML
done

ruby "$root/scripts/summarize-nextest-junit.rb" "$tmp/run-1.xml" "$tmp/run-2.xml" > "$tmp/summary.md"
grep -Fq 'Runs: 2; tests observed: 2; retries: disabled' "$tmp/summary.md"
grep -Fq '| `suite::slow` | 2.500 | 2.000 | 3.000 | 1.000 | 2 |' "$tmp/summary.md"
grep -Fq '| `suite::fast` | 0.150 | 0.100 | 0.200 | 0.100 | 2 |' "$tmp/summary.md"

echo "summarize-nextest-junit: ok"
