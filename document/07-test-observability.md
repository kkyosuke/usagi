# 7. テスト観測

> [ドキュメント目次](README.md) ｜ ← 前へ [6. 開発規約](06-conventions.md)

slow/flaky test の計測方法と test runner の採否判断は本書を正本とする。correctness と coverage の gate は [開発規約](06-conventions.md#品質チェックリスク比例の-gate)に従う。

## 目次

- [継続計測](#継続計測)
- [sccache opt-in](#sccache-opt-in)
- [runner 比較](#runner-比較)
- [nextest の採否](#nextest-の採否)
- [flaky と subprocess](#flaky-と-subprocess)
- [slow 上位](#slow-上位)

## 継続計測

`Test metrics` workflow は毎週月曜と手動実行時に full suite を 3 回走らせる。各 test の duration を含む JUnit 3 個と、平均時間が長い上位 20 件および min/max/range をまとめた `summary.md` を 90 日間 artifact として保存する。

- runner: cargo-nextest の `metrics` profile
- retries: 0。失敗を成功に置き換えない
- fail-fast: 無効。1 回の実行で全 failure を観測する
- slow timeout: 10 秒ごとに警告し、30 秒で終了する
- required gate: 従来どおり `cargo test --quiet`。観測 workflow は required pass を水増ししない

ローカルで同じ JUnit を作る場合は `cargo nextest run --profile metrics --workspace --no-fail-fast` を使う。複数回の結果は次で集約する。

```bash
ruby scripts/summarize-nextest-junit.rb target/nextest/metrics/junit.xml ...
```

## sccache opt-in

Rust ビルドの sccache 利用手順とベンチ観測方法は本節を正本とする。repo-wide の `.cargo/config.toml` で
`rustc-wrapper = "sccache"` は固定しない。未インストール環境では通常の Cargo 実行を維持する。

| 項目 | 仕様 |
|---|---|
| 有効化 | `scripts/cargo-sccache.sh <cargo-args...>` を明示的に使う |
| fallback | `sccache` が `PATH` に無い場合は警告を出し、`RUSTC_WRAPPER` / `SCCACHE_DIR` を付けずに `cargo` を実行する |
| cache dir | 既定は `<workspace>/.usagi/cache/sccache`。session worktree 間で共有する |
| target dir | 共有しない。各 session worktree の `<worktree>/target` を使う |
| cache size | 既定は `SCCACHE_CACHE_SIZE=10G`。必要に応じて環境変数で上書きする |
| CI | この helper では導入しない。ローカルベンチ結果を受けて別 issue で判断する |

通常の gate を sccache 付きで実行する場合は、Cargo サブコマンドだけを helper に渡す。

```bash
scripts/cargo-sccache.sh fmt --all -- --check
scripts/cargo-sccache.sh clippy --all-targets -- -D warnings
scripts/cargo-sccache.sh test --quiet
```

`cargo llvm-cov` も cargo サブコマンドなので、helper 経由で `scripts/cargo-sccache.sh llvm-cov ...` と実行できる。
coverage の正本 command と閾値は [開発規約](06-conventions.md#品質チェックリスク比例の-gate)に従う。

観測では `sccache` の stats を command ごとに残す。

```bash
sccache --zero-stats
scripts/cargo-sccache.sh test --quiet
sccache --show-stats
```

cache を削除して cold run を作る場合は、helper の既定 `SCCACHE_DIR` である `<workspace>/.usagi/cache/sccache` を削除する。
stats だけをリセットする場合は `sccache --zero-stats` を使う。cache dir 削除、`sccache --zero-stats`、`cargo clean` は役割が違う。

| 操作 | 消えるもの | 用途 |
|---|---|---|
| `cargo clean` | 現在の worktree の `target` | worktree local artifact の影響を消す |
| `sccache --zero-stats` | sccache の統計値 | 次の command だけの hit/miss を見る |
| `rm -rf <workspace>/.usagi/cache/sccache` | session 間で共有する sccache object cache | sccache cold run を作る |

ベンチは `scripts/sccache-benchmark.sh` を使う。既定は `cargo test --quiet` を 3 回実行し、raw TSV と
`sccache --show-stats` を `target/sccache-benchmark/` に保存する。

```bash
scripts/sccache-benchmark.sh --runs 3 --command "test --quiet"
scripts/sccache-benchmark.sh --runs 3 --command "clippy --all-targets -- -D warnings" --peer-worktree ../issue-xxx
```

ベンチ結果には cold/warm、単一 session、複数 session の wall time と stats file を残す。複数 session の warm run は、
session A で sccache cache を温めたあと、session B の別 worktree で `cargo clean` して同じ command を実行する。
比較時は中央値を使い、複数 session warm の `cargo test --quiet` または `cargo clippy` が 20% 以上短縮するか、
短縮しない理由を stats とともに記録する。

sccache の hit rate は proc macro、build script、feature 差、target triple、`RUSTFLAGS`、環境変数差で下がる。
CI 導入を検討する場合は、既存 Cargo cache と sccache の役割分担、cache restore/save time、workflow wall time、
billed minutes を同じ PR 上で比較する。

## runner 比較

2026-07-11、macOS arm64、Rust stable、cargo-nextest 0.9.140、cargo-llvm-cov 0.8.7 で計測した。cold は runner ごとに `cargo clean` 後、warm は同じ runner の直後の再実行である。

| 実行 | cargo test | nextest | 差 |
|---|---:|---:|---:|
| 通常 cold wall | 229.47s | 210.56s | nextest 8.2% 短縮 |
| 通常 warm wall / runner summary | 70.02s | 115.75s | nextest 65.3% 増加 |
| 通常 warm daemon IPC target | 12.18s | 5.49s（最長 test） | 実行モデルが異なるため参考値 |
| coverage warm runner | 86.07s | 143.16s | nextest 66.3% 増加 |

cargo test は lib test を 1 process で実行するのに対し、nextest は test ごとに process を分ける。本 repository は短い unit test が 3,000 件超あるため process 起動 overhead が warm 実行を支配した。

## nextest の採否

nextest を required runner としては採用しない。cold の改善は 10% 未満で、開発中に頻出する warm 実行と coverage 実行は明確に悪化したため、依存追加に見合う wall time 改善ではない。`cargo llvm-cov nextest` も required coverage 経路にせず、CI と lefthook は cargo test runner と coverage 100% を維持する。

nextest は test 単位の duration/JUnit を安定して得られるため、定期観測 workflow にだけ限定する。`.config/nextest.toml` の profile を明示し、インストール有無によってローカル coverage runner が暗黙に切り替わらないようにする。

## flaky と subprocess

通常 runner の full suite 2 回と nextest の full suite 2 回（合計 13,520 test executions）で test failure は観測されなかった。nextest では daemon IPC 6 件がすべて成功し、各 test の専用 data dir、daemon stop、異常時 SIGKILL fallback により残留 daemon process は観測されなかった。PTY の `tui_e2e` 2 件も成功し、capture 下で終了した。

coverage では両 runner とも全 test が成功した一方、`presentation/tui/home/event/mod.rs` の `Key::Char('v') => state.diff_toggle_layout()` が 1 回未到達となり、line gate が 99.999%（表示上 100.00%）で失敗した。通常 full suite では再現せず、coverage instrumentation または実行順に依存する既存 flaky として後続 issue で追跡する。nextest coverage も同じ未到達を解消せず、coverage 同等性の根拠にはならなかった。

定期 workflow の 3 反復で failure が出た場合は、JUnit の test name・attempt・duration と artifact の run URL を添えて issue 化する。retry は診断時にも明示指定し、required pass には用いない。

## slow 上位

初回 nextest cold run で目立った領域は次のとおりである。継続的な順位と variance は artifact の `summary.md` を参照する。

| 領域 / test | duration | 主なコスト |
|---|---:|---|
| `usecase::update::distributes_the_default_branch...` | 8.63s | 複数 Git repository / session 更新 |
| `daemon_ipc_test::spawn_runs_the_given_command...` | 7.68s | daemon・PTY・command lifecycle |
| `daemon_ipc_test::daemon_owned_terminal_survives...` | 4.86s | daemon・PTY detach/cleanup |
| `usecase::update::resolves_the_workspace_root...` | 4.45s | workspace/worktree Git fixture |
| daemon IPC の attach/list/key tests | 3.43–3.95s | daemon process / socket wait |

個別改善は issue ストアの `perf,test` ラベル付き後続 issue で管理する。
