---
number: 666
title: fix(ci): nightly toolchain を日付で pin する
status: done
priority: high
labels: []
dependson: []
related: []
created_at: 2026-08-12T00:19:18.722858+00:00
updated_at: 2026-08-12T00:19:23.928386+00:00
---

## 症状

2026-08-11 以降、`main` と全 PR の `Rust lint` job が落ちる。PR の変更内容とは無関係で、`usagi-core` だけで 87 件の clippy error が出る。

## 原因

`rust-toolchain.toml` が `channel = "nightly"`（日付なし）なので、CI は実行日ごとに最新 nightly を取る。新しく安定化した lint（`clippy::assert_is_empty` 85 件、`clippy::double_must_use` 2 件）が既存コードで一斉に発火し、`-D warnings` で全部 error になった。

最後に green だった CI（2026-08-07）は `rustc 1.99.0-nightly (84b36a78a 2026-08-06)` を使っていた。

## 受入条件

- `rust-toolchain.toml` の nightly を日付 pin し、CI の `Rust lint` が green に戻る。
- pin した toolchain に fmt / clippy / coverage が必要とする component が届く（workflow の `dtolnay/rust-toolchain@nightly` は日付なし `nightly` にしか component を入れないため、`components` を toolchain file 側の正本にする）。
- toolchain 更新は「pin を上げる PR」で意図的に行い、その PR で新 lint の対応もまとめる、という運用を規約に書く。
