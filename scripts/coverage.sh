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

# 計測から外すファイル。いずれも「テスト可能なロジックを取り除いたあとに残る、
# 実 IO そのもの」だけを持つ層に限定する:
#   - src/main\.rs            : バイナリの合成ルート（clap ディスパッチと実 IO の注入）。
#   - infrastructure/pty\.rs  : 擬似端末・スレッドの実 IO。
#   - infrastructure/release\.rs : `git ls-remote` のネットワーク IO とタイムアウト監視。
#   - tui/term_reader\.rs     : 実端末からのキー入力（live TTY が必要）。
#   - tui/app/mod\.rs / tui/home/mod\.rs / home/terminal_pane\.rs / home/terminal_pool\.rs
#     / tui/open/mod\.rs / tui/config/mod\.rs / tui/config/provisioning\.rs
#                             : 実端末・実 PTY・実スレッドを束ねるオーケストレータ。
# これら以外の薄いラッパ（hop/run/mcp/llm_mcp/agent_phase/clean、splash/gallery/
# welcome/new、echo）は依存を注入してテスト可能にし、計測対象に含めている。
export COVERAGE_IGNORE='(src/main\.rs|infrastructure/pty\.rs|infrastructure/release\.rs|tui/term_reader\.rs|tui/app/mod\.rs|tui/home/mod\.rs|tui/home/terminal_pane\.rs|tui/home/terminal_pool\.rs|tui/open/mod\.rs|tui/config/mod\.rs|tui/config/provisioning\.rs)'
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
