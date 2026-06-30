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

# 計測から外すファイル。いずれも「テスト可能なロジックを取り除いたあとに残る、
# 実 IO そのもの」だけを持つ層に限定する:
#   - src/main\.rs            : バイナリの合成ルート（clap ディスパッチと実 IO の注入）。
#   - infrastructure/pty\.rs  : 擬似端末・スレッドの実 IO。
#   - infrastructure/resource\.rs : `sysinfo` による実プロセスの CPU/メモリ計測 IO。
#       集計・整形の純ロジックは domain/resource.rs に切り出して計測対象に含めてある。
#   - infrastructure/release\.rs : `git ls-remote` のネットワーク IO とタイムアウト監視。
#   - tui/io/term_reader\.rs  : 実端末からのキー入力（live TTY が必要）。
#   - tui/app/mod\.rs / tui/home/mod\.rs / home/terminal/pane\.rs / home/terminal/pool\.rs
#     / tui/open/mod\.rs / tui/config/mod\.rs / tui/config/provisioning\.rs
#     / tui/welcome/mod\.rs
#                             : 実端末・実 PTY・実スレッドを束ねるオーケストレータ。
#       terminal/pane / terminal/pool の純ロジック（キー/マウスの入力変換は
#       home/pane_input.rs、タブのインデックス・ラベル算術は home/terminal/tabs.rs）は
#       別モジュールへ切り出して計測対象に含めてあり、ここに残るのは実 IO の束ねだけ。
# これら以外の薄いラッパ（hop/run/mcp/llm_mcp/agent_phase/clean、splash/gallery/
# new、io/echo）は依存を注入してテスト可能にし、計測対象に含めている。
#   - infrastructure/setup_runner\.rs : セッション setup コマンドを子プロセスで実行する
#       実 IO。cargo llvm-cov --workspace ではバイナリクレートと lib クレートの両方が
#       コンパイルされ、テストが触れるのは lib 側のシンボルだけのため、binary 側の
#       インスタンスが常に未カバーになってしまう（二重ビルド問題）。実 IO 層として
#       除外することで正確な計測結果を維持する。
export COVERAGE_IGNORE='(src/main\.rs|infrastructure/pty\.rs|infrastructure/resource\.rs|infrastructure/release\.rs|infrastructure/setup_runner\.rs|tui/io/term_reader\.rs|tui/app/mod\.rs|tui/home/mod\.rs|tui/home/terminal/pane\.rs|tui/home/terminal/pool\.rs|tui/open/mod\.rs|tui/config/mod\.rs|tui/config/provisioning\.rs|tui/welcome/mod\.rs)'
# 100% を要求するカバレッジ指標。
export COVERAGE_MIN=100

# ローカル（lefthook pre-push）で計測から 100% 強制までを一括実行する。
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
