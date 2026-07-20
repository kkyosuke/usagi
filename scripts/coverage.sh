#!/usr/bin/env bash
# テストカバレッジ設定の Single Source of Truth (SSoT)。
#
# CI (.github/workflows/coverage.yml) と任意のローカル full gate が
# このファイルを source し、同じ exclusion lint・同じ閾値で評価する。
# 値を変更するときはこのファイルだけを直せばよい。
#
# 使い方:
#   . scripts/coverage.sh     # COVERAGE_MIN を読み込む
#   coverage_enforce          # ローカルで lint・計測・100% を強制する
#                             # --no-clean で前回のビルド成果物を再利用する

# 計測対象は v2 workspace（ルートの bin パッケージ + crates/ 配下の 3 クレート）。
# v1/ は退避された旧実装で、workspace から exclude されているため計測に含まれない。
#
# 計測から外す item は、ソースコードの `#[coverage(off)]` を正本とする。
# ファイル名ベースの除外は使わず、理由と使用条件は document/06-conventions.md に従う。
# 100% を要求するカバレッジ指標。
export COVERAGE_MIN=100

# coverage 計測前に exclusion policy を検査する。allowlist にない属性、source から
# 消えた stale entry、期限切れ entry は coverage 率にかかわらず失敗させる。
coverage_off_lint() {
  ruby scripts/coverage-off-lint.rb
}

# 直前の `cargo llvm-cov --workspace` の計測結果を workspace 全体で再集計する
# （CI の summary / enforce 用）。workspace 化後の `cargo llvm-cov report` は
# カレント（ルート）パッケージにしかスコープせず、ルートは `#[coverage(off)]` の
# main.rs しか持たないため素の report では集計が空になる。パッケージ命名規約
# （usagi / usagi-*。document/02-architecture.md）に一致する glob で全パッケージを
# 明示的に選ぶ。
coverage_report() {
  cargo llvm-cov report -p 'usagi*' "$@"
}

# ローカルで exclusion lint、計測、100% 強制までを一括実行する。
# CI は計測（lcov 生成）と report を分けて実行するため、こちらは使わない。
coverage_enforce() {
  if ! command -v cargo-llvm-cov >/dev/null 2>&1; then
    echo "✗ cargo-llvm-cov が見つかりません" >&2
    echo "  インストール: cargo install cargo-llvm-cov" >&2
    return 1
  fi
  coverage_off_lint || return 1
  # runner はインストール済みツールに左右されず、CI と同じ cargo test に固定する。
  cargo llvm-cov --workspace --no-clean \
    --fail-under-lines "$COVERAGE_MIN" \
    --fail-under-functions "$COVERAGE_MIN"
}
