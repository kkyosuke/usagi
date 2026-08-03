#!/usr/bin/env bash
# v1（現在の出荷バイナリ）の coverage gate 設定の Single Source of Truth (SSoT)。
#
# CI (.github/workflows/v1-coverage.yml) と任意のローカル検証がこのファイルを
# source し、v1 の独立 manifest、除外条件、閾値を共有する。

export V1_COVERAGE_MIN=100
export V1_COVERAGE_MANIFEST="${V1_COVERAGE_MANIFEST:-v1/Cargo.toml}"

# filename exclusion はテスト可能な判断を分離したあとの real IO 境界だけに限る。
# business logic、parser、error path は除外しない。各境界の純粋ロジックは隣接する
# domain / presentation module へ分離され、fake / unit / integration test で検証される。
#
# - main.rs: clap dispatch と production dependency の合成ルート
# - pty/resource/release/op_cli/secret_store/setup_runner: PTY、OS process、network、
#   keychain、subprocess の real IO
# - tui/io/{term_reader,signals,loading}: live terminal、signal、thread/clock の real IO
# - listed TUI orchestrators: real terminal / PTY / thread を束ねる薄い境界
export V1_COVERAGE_IGNORE='(^|/)(src/main\.rs|src/infrastructure/(pty|resource|release|secret_store|setup_runner)\.rs|src/infrastructure/env_resolver/op_cli\.rs|src/presentation/tui/io/(term_reader|signals|loading)\.rs|src/presentation/tui/(app|chat|home|open|config|welcome)/mod\.rs|src/presentation/tui/config/provisioning\.rs|src/presentation/tui/home/terminal/(pane|pool)\.rs)$'

v1_coverage_report() {
  cargo llvm-cov report \
    --manifest-path "$V1_COVERAGE_MANIFEST" \
    --ignore-filename-regex "$V1_COVERAGE_IGNORE" \
    "$@"
}

v1_coverage_enforce() {
  if ! command -v cargo-llvm-cov >/dev/null 2>&1; then
    echo "✗ cargo-llvm-cov が見つかりません" >&2
    echo "  インストール: cargo install cargo-llvm-cov" >&2
    return 1
  fi

  cargo llvm-cov \
    --manifest-path "$V1_COVERAGE_MANIFEST" \
    --workspace \
    --no-clean \
    --ignore-filename-regex "$V1_COVERAGE_IGNORE" \
    --fail-under-lines "$V1_COVERAGE_MIN" \
    --fail-under-functions "$V1_COVERAGE_MIN"
}
