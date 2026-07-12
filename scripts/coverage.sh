#!/usr/bin/env bash
# テストカバレッジ設定の Single Source of Truth (SSoT)。
#
# CI (.github/workflows/coverage.yml) と lefthook (lefthook.yml) の両方が
# このファイルを source し、同じ除外条件・同じ閾値でカバレッジを評価する。
# 値を変更するときはこのファイルだけを直せばよい。
#
# 使い方:
#   . scripts/coverage.sh     # COVERAGE_IGNORE / COVERAGE_MIN を読み込む
#   coverage_enforce          # ローカルで計測して 100% を強制する (lefthook pre-push 用)
#                             # --no-clean で前回のビルド成果物を再利用する

# 計測対象は v2 workspace（ルートの bin パッケージ + crates/ 配下の 3 クレート）。
# v1/ は退避された旧実装で、workspace から exclude されているため計測に含まれない。
#
# 計測から外すのは実 IO、generic codec の単相化重複、および TUI projection の
# branch-merge 中に別 slice が所有する controller / workspace / command registry である。
export COVERAGE_IGNORE='(src/main\.rs|core/src/infrastructure/ipc/mod\.rs|daemon/src/presentation/ipc\.rs|daemon/src/infrastructure/unix_transport\.rs|tui/src/presentation/views/workspace\.rs|tui/src/usecase/application/controller\.rs|tui/src/usecase/(closeup|overview)/mod\.rs)'
# 100% を要求するカバレッジ指標。
export COVERAGE_MIN=100

# 直前の `cargo llvm-cov --workspace` の計測結果を workspace 全体で再集計する
# （CI の summary / enforce 用）。workspace 化後の `cargo llvm-cov report` は
# カレント（ルート）パッケージにしかスコープせず、ルートは COVERAGE_IGNORE の
# main.rs しか持たないため素の report では集計が空になる。パッケージ命名規約
# （usagi / usagi-*。document/02-architecture.md）に一致する glob で全パッケージを
# 明示的に選ぶ。
coverage_report() {
  cargo llvm-cov report -p 'usagi*' --ignore-filename-regex "$COVERAGE_IGNORE" "$@"
}

# ローカル（lefthook pre-push）で計測から 100% 強制までを一括実行する。
# CI は計測（lcov 生成）と report を分けて実行するため、こちらは使わない。
coverage_enforce() {
  if ! command -v cargo-llvm-cov >/dev/null 2>&1; then
    echo "✗ cargo-llvm-cov が見つかりません" >&2
    echo "  インストール: cargo install cargo-llvm-cov" >&2
    return 1
  fi
  # runner はインストール済みツールに左右されず、CI と同じ cargo test に固定する。
  cargo llvm-cov --workspace --no-clean \
    --ignore-filename-regex "$COVERAGE_IGNORE" \
    --fail-under-lines "$COVERAGE_MIN" \
    --fail-under-functions "$COVERAGE_MIN"
}
