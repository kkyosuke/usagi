#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
checker="$script_dir/../ci/security-audit-exceptions.rb"
fixture=$(mktemp -d)
trap 'rm -rf "$fixture"' EXIT

valid="$fixture/valid.json"
cat > "$valid" <<'JSON'
{
  "version": 1,
  "exceptions": [
    {
      "advisory": "RUSTSEC-2029-0001",
      "owner": "@security-owner",
      "expires": "2030-02-01",
      "rationale": "Upgrade is blocked by an upstream release."
    }
  ]
}
JSON

output=$(GITHUB_OUTPUT= SECURITY_AUDIT_TODAY=2030-01-01 ruby "$checker" "$valid")
test "$output" = $'ignore=RUSTSEC-2029-0001\nvalidated 1 RustSec exception(s)'

for field in owner expires rationale; do
  invalid="$fixture/missing-$field.json"
  ruby -rjson -e 'data = JSON.parse(File.read(ARGV[0])); data["exceptions"][0].delete(ARGV[1]); File.write(ARGV[2], JSON.pretty_generate(data))' \
    "$valid" "$field" "$invalid"
  if GITHUB_OUTPUT= SECURITY_AUDIT_TODAY=2030-01-01 \
    ruby "$checker" "$invalid" >/dev/null 2>&1; then
    echo "missing $field was accepted" >&2
    exit 1
  fi
done

assert_rejected() {
  local label=$1
  local path=$2
  local today=${3:-2030-01-01}
  if GITHUB_OUTPUT= SECURITY_AUDIT_TODAY="$today" \
    ruby "$checker" "$path" >/dev/null 2>&1; then
    echo "$label was accepted" >&2
    exit 1
  fi
}

assert_rejected "expired exception" "$valid" 2030-03-01

make_invalid_entry() {
  local name=$1
  local ruby_expression=$2
  local invalid="$fixture/$name.json"
  ruby -rjson -e '
  data = JSON.parse(File.read(ARGV[0]))
  entry = data["exceptions"][0]
  eval(ARGV[2])
  File.write(ARGV[1], JSON.pretty_generate(data))
' "$valid" "$invalid" "$ruby_expression"
  assert_rejected "$name" "$invalid"
}

make_invalid_entry "invalid owner" 'entry["owner"] = "@trailing-"'
make_invalid_entry "non-string owner" 'entry["owner"] = true'
make_invalid_entry "short rationale" 'entry["rationale"] = "short"'
make_invalid_entry "far expiry" 'entry["expires"] = "2031-01-01"'
make_invalid_entry "invalid expiry" 'entry["expires"] = "not-a-date"'
make_invalid_entry "invalid advisory" 'entry["advisory"] = "CVE-2030-0001"'
make_invalid_entry "unknown entry field" 'entry["typo"] = true'
make_invalid_entry "duplicate advisory" 'data["exceptions"] << entry.dup'

ruby -rjson -e '
  data = JSON.parse(File.read(ARGV[0]))
  data["typo"] = true
  File.write(ARGV[1], JSON.pretty_generate(data))
' "$valid" "$fixture/unknown-root-field.json"
assert_rejected "unknown root field" "$fixture/unknown-root-field.json"

printf '%s\n' '[]' > "$fixture/not-an-object.json"
assert_rejected "non-object manifest" "$fixture/not-an-object.json"

printf '%s\n' '{' > "$fixture/invalid-json.json"
assert_rejected "invalid JSON" "$fixture/invalid-json.json"

echo "security audit exception fixtures passed"
