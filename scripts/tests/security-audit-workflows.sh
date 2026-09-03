#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "$0")/../.." && pwd)
installer='uses: taiki-e/install-action@v2.87.4'
tool='tool: cargo-audit@0.22.2'
audit='uses: rustsec/audit-check@v2.0.0'

assert_once_before_audit() {
  local workflow=$1
  local install_name='name: Install cargo-audit'
  local install_name_count tool_count audit_count
  local install_name_line audit_line install_block

  install_name_count=$(grep -Fc "$install_name" "$workflow" || true)
  tool_count=$(grep -Fc "$tool" "$workflow" || true)
  audit_count=$(grep -Fc "$audit" "$workflow" || true)
  if [[ $install_name_count -ne 1 || $tool_count -ne 1 || $audit_count -ne 1 ]]; then
    printf '%s must pin one cargo-audit installer, tool, and audit action\n' "$workflow" >&2
    exit 1
  fi

  install_name_line=$(grep -nF "$install_name" "$workflow" | cut -d: -f1)
  audit_line=$(grep -nF "$audit" "$workflow" | cut -d: -f1)
  install_block=$(sed -n "${install_name_line},$((install_name_line + 3))p" "$workflow")
  if ! grep -Fq "$installer" <<<"$install_block" || ! grep -Fq "$tool" <<<"$install_block" ||
    (( install_name_line >= audit_line )); then
    printf '%s must install the pinned cargo-audit binary before audit-check\n' "$workflow" >&2
    exit 1
  fi
}

assert_once_before_audit "$repo_root/.github/workflows/test.yml"
assert_once_before_audit "$repo_root/.github/workflows/security-audit.yml"

echo "security audit workflow contract: ok"
