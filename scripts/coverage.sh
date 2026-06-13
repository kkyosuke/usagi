#!/usr/bin/env bash
# テストカバレッジ設定の Single Source of Truth (SSoT)。
#
# CI (.github/workflows/coverage.yml) と lefthook (lefthook.yml) の両方が
# このファイルを source し、同じ除外条件・同じ閾値でカバレッジを評価する。
# 値を変更するときはこのファイルだけを直せばよい。
#
# 使い方:
#   . scripts/coverage.sh     # COVERAGE_IGNORE / COVERAGE_MIN を読み込む
#   coverage_enforce          # ローカルで計測して 100% を強制する (lefthook 用)

# 対象から外す端末 I/O 専用の薄いラッパ（テスト不能なため計測しない）。
export COVERAGE_IGNORE='(src/main\.rs|cli/hop\.rs|tui/term_reader\.rs|tui/welcome/mod\.rs|tui/home/mod\.rs|tui/new/mod\.rs|tui/open/mod\.rs|tui/config/mod\.rs)'
# 100% を要求するカバレッジ指標。
export COVERAGE_MIN=100

# ローカル（lefthook pre-commit）で計測から 100% 強制までを一括実行する。
# CI は計測（lcov 生成）と report を分けて実行するため、こちらは使わない。
coverage_enforce() {
  if ! command -v cargo-llvm-cov >/dev/null 2>&1; then
    echo "✗ cargo-llvm-cov が見つかりません" >&2
    echo "  インストール: cargo install cargo-llvm-cov" >&2
    return 1
  fi
  cargo llvm-cov --workspace \
    --ignore-filename-regex "$COVERAGE_IGNORE" \
    --fail-under-lines "$COVERAGE_MIN" \
    --fail-under-functions "$COVERAGE_MIN"
}
