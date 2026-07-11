---
number: 191
title: perf: Rust ビルドに sccache opt-in とベンチを導入する
status: done
priority: medium
labels: [perf, dev]
dependson: []
related: [177, 181]
parent: 177
created_at: 2026-07-11T02:37:24+00:00
updated_at: 2026-07-11T03:12:33.669563+00:00
---

## 背景

複数 workspace / session worktree では Cargo の `target` が worktree ごとに分かれるため、同じ commit / toolchain でもコンパイル成果物を再利用しにくい。調査では `.cargo/config.toml`、`target-dir`、`RUSTC_WRAPPER`、`SCCACHE_*` は未設定で、CI は既に `swatinem/rust-cache@v2` を使っている。

## 対応

- `sccache` がインストール済みの場合だけ `RUSTC_WRAPPER=sccache` を付ける opt-in helper を追加する
- `SCCACHE_DIR` を usagi workspace 全体で共有できる場所にし、session worktree 間で cache を共有する
- `target-dir` は共有せず、各 session worktree の `target` を維持する
- cold/warm、単一 session、複数 session の再現可能なベンチスクリプトを追加する
- `sccache --show-stats` / `--zero-stats` による観測と cache 削除手順をドキュメント化する

## 注意点

- repo に `.cargo/config.toml` で `rustc-wrapper = "sccache"` を固定しない。未インストール環境の Cargo 実行を壊さないことを優先する
- proc macro、build script、feature 差、target triple、`RUSTFLAGS`、環境変数差で hit rate が下がることを前提に stats を記録する
- CI 導入はこの issue のベンチ結果を受けて別 issue で判断する。既存 Cargo cache と sccache の役割分担、cache restore/save time、billed minutes を比較する

## 完了条件

- sccache 未インストール時に通常 Cargo 実行へ fallback する
- sccache あり/なしで `cargo fmt --all -- --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test --quiet`、coverage 100% の結果が変わらない
- 複数 session warm の `cargo test --quiet` または `cargo clippy` が中央値で 20% 以上短縮するか、短縮しない理由が stats とともに記録される
- 導入判断と詳細手順を `document/proposals/04-sccache-rust-builds.md` の内容に沿って正本ドキュメントへ反映する
