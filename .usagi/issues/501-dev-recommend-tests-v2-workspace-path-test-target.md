---
number: 501
title: dev: recommend-tests を v2 workspace path と test target に対応させる
status: done
priority: medium
labels: [review, v2, test, developer-experience]
dependson: []
related: [180]
parent: 453
created_at: 2026-07-20T12:07:09.334989+00:00
updated_at: 2026-07-20T23:41:14.163753+00:00
---

## 問題・影響

`scripts/recommend-tests.tsv` は旧 `src/{domain,usecase,infrastructure,presentation}` と v1 paths を中心にし、root/v2 の `crates/*` と `src/runtime/*` を知らない。多くの局所変更が unknown path として full workspace suite に fallback し、pre-push/開発 cycle を不必要に遅くする。

## 成立条件 / 再現フロー

各 v2 crate の単一 source file や `src/runtime/{cli,daemon,tui}.rs` だけを diff として `scripts/recommend-tests.sh` に与える。対応 package/test target ではなく full suite が推奨される。

## 対象責務と非対象

path classifier table、v2 package/runtime/integration target mapping、fixture test を対象とする。安全性のための multi-layer/Cargo/shared CI full fallback 廃止、test suite 自体の高速化は非対象。

## 受入条件

- [ ] `crates/core`、`daemon`、`tui`、`cli` と root `src/runtime`/integration paths を最小安全な `cargo test -p` / target に分類する。
- [ ] Cargo manifests、shared protocol/CI、複数領域変更、unknown path は fail-safe full fallback を維持する。
- [ ] table の shadow/未到達 rule と actual package/target 不在を検出する。
- [ ] 推奨 command の重複を正規化し、利用者へ fallback 理由を表示する。

## 必須回帰テスト

代表 v1/v2 path、各 crate/module、root runtime、integration、Cargo/shared、multi-file、unknown の fixture を追加し、expected command/fallback reason を snapshot または table test する。

## docs / 移行影響

developer workflow docs に v2 mapping と full fallback 条件を追記する。runtime/data migration はない。
