# sccache による Rust ビルド高速化

> [ドキュメント目次](../README.md) ｜ [設計提案一覧](README.md)

`usagi` の複数 workspace / session worktree で Rust コンパイル成果物を共有し、ローカル開発と CI の build latency を下げるための調査メモ。ローカル opt-in helper とベンチ手順の現在の仕様は [テスト観測](../07-test-observability.md#sccache-opt-in) を正本とする。本書は CI 導入など、追加判断が必要な設計提案を残す。

## 目次

- [現状調査](#現状調査)
- [推奨構成](#推奨構成)
- [実装タスク案](#実装タスク案)
- [ベンチマークと受け入れ基準](#ベンチマークと受け入れ基準)
- [CI 実験の初回観測](#ci-実験の初回観測)
- [go / no-go 判断](#go--no-go-判断)

## 現状調査

| 観点 | 現状 |
|---|---|
| Cargo workspace | `cargo metadata --no-deps` では workspace member は `usagi` 1 package のみ。`third_party/vt100` は `[patch.crates-io]` で差し替える vendored dependency で、workspace member ではない。 |
| target dir | `.cargo/config.toml` は存在せず、`target_directory` は各 session worktree の `<worktree>/target`。`CARGO_TARGET_DIR` / `target-dir` は未設定。 |
| wrapper | `RUSTC_WRAPPER` / `SCCACHE_*` は repo と CI で未設定。ローカル確認環境にも `sccache` は未インストール。 |
| ローカル gate | `lefthook.yml` は commit 時に staged Rust file を `cargo fmt`、push 時に `cargo clippy --all-targets -- -D warnings` と `scripts/coverage.sh` 経由の `cargo llvm-cov` を実行する。 |
| CI gate | `test.yml` は fmt/clippy job と full-test job を分離し、どちらも `swatinem/rust-cache@v2` を使用する。`coverage.yml`、`test-metrics.yml`、release build 系 workflow も Rust cache を使用する。 |
| 既存観測 | `document/07-test-observability.md` と issue #181/#177 に、warm `cargo test` が約 70〜105 秒、cold が約 229 秒という基準値がある。重さは Git fixture、TUI render、daemon/PTY test にもあり、コンパイルだけが支配要因ではない。 |

## 推奨構成

| 領域 | 推奨 |
|---|---|
| ローカルの有効化 | repo に `.cargo/config.toml` で `rustc-wrapper = "sccache"` を固定しない。未インストール環境で全 Cargo コマンドが壊れるため、まずは opt-in helper を用意する。 |
| workspace 共有 | `usagi` workspace 全体のローカル設定として扱う。例: `~/.usagi/cache/sccache/<workspace-key>` を `SCCACHE_DIR` にし、各 session worktree から同じ cache を参照する。 |
| wrapper 指定 | helper 経由で `command -v sccache` を確認できる場合だけ `RUSTC_WRAPPER=sccache` を付ける。見つからない場合は警告に留め、通常の Cargo 実行に fallback する。 |
| target dir | `target-dir` は共有しない。session ごとの `<worktree>/target` を維持し、link artifact / incremental / test binary の衝突を避ける。共有対象は sccache の object cache に限定する。 |
| 容量上限 | ローカルは `SCCACHE_CACHE_SIZE=10G` を既定候補にする。小さい disk では `5G` に下げられるよう環境変数上書きを許す。 |
| 削除・失効 | 削除は `sccache --zero-stats` と cache dir の削除を明示する。失効は rustc version、crate hash、features、env、target triple 等に任せ、manual key bump は基本不要。 |
| 観測 | helper に `sccache --show-stats` / `sccache --zero-stats` を案内するか、`scripts/sccache-stats.sh` を追加する。hit rate、cache size、non-cacheable reasons を PR 前後で記録する。 |
| CI | required Rust gate の ubuntu job（`test.yml` と `coverage.yml`）だけで実験する。release matrix は target triple 差と LTO release build の特性が違うため後段にする。 |

`swatinem/rust-cache` は Cargo registry/git cache と `target` 復元を担当し、sccache は rustc のコンパイル出力を compiler invocation 単位で再利用する。役割は重なるが同一ではない。CI では `swatinem/rust-cache` を残し、sccache の cache dir だけ `actions/cache` で追加保存する構成が比較しやすい。

複数 session worktree 間での共有は安全と判断する。sccache の key は rustc、target、crate inputs、features、compiler flags、関連 env を含むため、worktree path が違っても同じソース・同じ lock・同じ toolchain なら hit し、差があれば miss する。ただし `build.rs` が volatile な env や絶対 path を出力する場合、hit rate は下がる。`usagi` には `build.rs` は無く、proc macro は依存 crate 側に限定されるため、初期導入の correctness risk は低い。

## 実装タスク案

1. `scripts/cargo-sccache.sh` を追加し、`sccache` がある場合だけ `RUSTC_WRAPPER=sccache` / `SCCACHE_DIR` / `SCCACHE_CACHE_SIZE` を設定して渡された Cargo command を実行する。
2. `scripts/sccache-benchmark.sh` を追加し、cold/warm、単一 session、複数 session の計測、`sccache --show-stats` 採取、結果 TSV 出力を自動化する。
3. `document/06-conventions.md` または `document/07-test-observability.md` に、採用後の開発者向け opt-in 手順と観測手順だけを正本として追記する。
4. CI 実験 PR で `test.yml` / `coverage.yml` の ubuntu job に explicit install + `actions/cache` を追加し、既存 `swatinem/rust-cache` との併用結果を記録する。
5. 効果が基準を満たした場合のみ、release build check / release workflow への展開を別 issue に分ける。

初回 issue は「ローカル opt-in helper とベンチスクリプト」に絞った。CI 導入は runner cache の安定性と billed minutes
の評価が必要なため、次段階の実験 PR として required Rust gate の ubuntu job だけに限定する。

## ベンチマークと受け入れ基準

| ケース | 手順 |
|---|---|
| baseline cold | `cargo clean` 後に `/usr/bin/time -p cargo test --quiet`、`cargo clippy --all-targets -- -D warnings`、必要なら `cargo llvm-cov --workspace --no-clean ...` を実行する。 |
| sccache cold | `sccache --zero-stats`、cache dir 削除、`cargo clean` 後に同じ command を helper 経由で実行し、wall time と `sccache --show-stats` を保存する。 |
| sccache warm | `target` は `cargo clean` で消し、sccache dir は残して同じ command を再実行する。compile cache の効果と test runtime の下限を分ける。 |
| 単一 session | 同じ worktree で cold/warm を 3 回ずつ実行し、中央値を比較する。 |
| 複数 session | session A で cache を温め、session B の別 worktree で `cargo clean` 後に同じ commit / same toolchain で実行する。cross-worktree hit rate を見る。 |
| CI | PR で sccache なし/ありの workflow duration、job setup time、cache restore/save time、billed minutes を比較する。 |

受け入れ基準は次の通り。

| 項目 | 基準 |
|---|---|
| correctness | `cargo fmt --all -- --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test --quiet`、coverage 100% が sccache あり/なしで同じ結果になる。 |
| ローカル効果 | 複数 session の warm `cargo test --quiet` または `cargo clippy` の wall time 中央値が 20% 以上短縮する。 |
| CI 効果 | setup/cache save を含む required Rust jobs の wall time が 10% 以上短縮し、billed minutes が悪化しない。 |
| 観測性 | PR に `sccache --show-stats` の hit/miss/non-cacheable と cache size が残る。 |
| 失敗時 fallback | `sccache` 未インストール、cache dir 削除、cache miss で通常 Cargo 実行に戻せる。 |

## CI 実験の初回観測

2026-07-11 に #201 / PR #747 の merge commit `40f4ffb` 以降の GitHub Actions を確認した。
`Test` は main push run、`Coverage` は #747 の PR run を観測対象にした。`Coverage` workflow は PR trigger のため、
main push run は存在しない。

| workflow / run | 対象 | 結果 | workflow duration | job duration | 主 command | cache / stats |
|---|---|---|---:|---:|---|---|
| `Test` [29141744270](https://github.com/kkyosuke/usagi/actions/runs/29141744270) | #747 main push | success | 1m54s | `test`: 1m14s / `full-test`: 1m51s | clippy 47.34s / tests 1m26s | sccache cache miss。`test`: 0 hits / 165 misses / 51 non-cacheable calls / 45 MiB。`full-test`: 0 hits / 155 misses / 50 non-cacheable calls / 144 MiB。 |
| `Test` [29141389888](https://github.com/kkyosuke/usagi/actions/runs/29141389888) | #745 main push baseline | success | 1m19s | `test`: 37s / `full-test`: 1m10s | clippy 20.19s / tests 46s | swatinem/rust-cache hit。sccache なし。 |
| `Test` [29138072798](https://github.com/kkyosuke/usagi/actions/runs/29138072798) | #743 main push baseline | success | 1m24s | `test`: 39s / `full-test`: 1m05s | clippy 18s / tests 44s | swatinem/rust-cache hit。sccache なし。 |
| `Coverage` [29141542248](https://github.com/kkyosuke/usagi/actions/runs/29141542248) | #747 PR | success | 1m58s | `coverage`: 1m56s | coverage command 1m22s | sccache cache miss。0 hits / 155 misses / 55 non-cacheable calls / 144 MiB。Lines / Functions とも 100%。 |
| `Coverage` [29138217046](https://github.com/kkyosuke/usagi/actions/runs/29138217046) | #745 PR baseline | success | 1m58s | `coverage`: 1m56s | coverage command 1m28s | swatinem/rust-cache miss。sccache なし。Lines / Functions とも 100%。 |

初回 #747 は `RUSTC_WRAPPER=sccache` が `swatinem/rust-cache` の環境 hash に入ったため、既存 baseline の
`rust-lint` / `rust-full-test` cache とは別 key になった。sccache の `actions/cache` も初回 miss で、`test` / `full-test`
/ `coverage` の sccache hit rate はすべて 0% だった。その結果、`Test` の wall time は近い baseline より悪化し、
required Rust jobs の 10% 短縮基準を満たさない。coverage は workflow duration が baseline と同等で、100% gate は維持した。

ログ上、`swatinem/rust-cache` は Cargo registry / git / `target` を対象にし、sccache は
`.usagi/cache/sccache-ci` だけを `actions/cache` で保存している。保存先の重複や restore/save failure は見当たらない。
ただし初回 run だけでは warmed sccache の効果が未観測であり、billed minutes 相当は `Test` で悪化している。

## go / no-go 判断

Go 条件は、ローカル opt-in helper で correctness が変わらず、複数 session warm の build-heavy command で 20% 以上の短縮が確認できること。CI は追加で 10% 以上の required job 短縮と billed minutes 非悪化を満たした場合に go とする。

現時点の判断は「local opt-in と required Rust gate の ubuntu CI 実験は go、repo-wide 強制と CI required 全面導入は
no-go」。理由は、未インストール環境を壊せないこと、既に `swatinem/rust-cache` が入っていること、`usagi` の test
wall time にはコンパイル以外の Git/PTY/TUI runtime が大きく含まれることである。CI 実験では setup/cache
restore/save を含めて required Rust jobs の wall time が 10% 以上短縮し、billed minutes が悪化しない場合だけ、
release build check / release workflow / test-metrics workflow への展開を別 issue で検討する。

#201 / PR #747 merge 後の初回 CI 実測では、required Rust jobs の 10% 短縮は確認できず、`Test` は baseline より遅い。
これは初回 cache miss と `RUSTC_WRAPPER` による `swatinem/rust-cache` key 変更を含むため、即時 rollback ではなく
「データ不足」と判断する。実験導入は required ubuntu Rust jobs に限定して継続し、release build check / release workflow /
test-metrics には広げない。次は warmed sccache cache が効く同一 key の main / PR run を 3 回観測し、hit rate、
job duration、workflow duration、cache restore/save time、billed minutes 相当を再評価する。
