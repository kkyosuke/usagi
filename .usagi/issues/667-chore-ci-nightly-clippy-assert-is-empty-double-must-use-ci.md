---
number: 667
title: chore(ci): 新しい nightly clippy の assert_is_empty / double_must_use で CI が落ちるのを解消する
status: done
priority: high
labels: [v2, ci, chore]
dependson: []
related: []
created_at: 2026-08-12T04:27:45.690683+00:00
updated_at: 2026-08-12T04:27:51.002475+00:00
---

## 問題・影響

`rust-toolchain.toml` は `channel = "nightly"` で pin していないため、CI（`dtolnay/rust-toolchain@nightly`）は毎回その時点で最新の nightly を引く。2026-08-11 の nightly で clippy に `assert_is_empty` が追加され、`[workspace.lints.clippy]` が `all` / `pedantic` を warn、CI が `-D warnings` にしているため、**変更と無関係に `Rust lint` job が失敗する**。`main` の最新 CI（#1475 のマージ）も同じ理由で赤い。

内訳は 2 種類である。

| lint | 件数 | 内容 |
|---|---|---|
| `assert_is_empty` | 85 | `assert!(value.is_empty())` を `assert_eq!(value, [] as [T; 0])` へ書き換えさせる |
| `double_must_use` | 5 | `#[must_use]` を付けた関数が、すでに must_use な型（`impl Iterator`）を返している |

## 対象責務

1. `assert_is_empty` は workspace 全体で allow にする。自動修正は要素型を明示する注記（`[] as [domain::recent::Recent; 0]` 等）を伴い、85 箇所の test assertion で「何を確かめているか」が読み取りにくくなる。失敗時に値を出す利点より、`is_empty()` のまま読める可読性を採る。理由は lint 表（`Cargo.toml` の `[workspace.lints.clippy]`）に併記する。
2. `double_must_use` は冗長な `#[must_use]` を削除して解消する（返り値型がすでに must_use を持つ）。

## 非対象

nightly の pin（`channel = "nightly-YYYY-MM-DD"`）は本 issue では行わない。次の新規 lint で同じ形の停止が再発するため、pin するかどうかは別途判断する。

## 受入条件

- [x] 新しい nightly（rustc 1.99.0-nightly, 2026-08-11）で `cargo clippy --workspace --all-targets -- -D warnings` が clean になる。
- [x] `cargo fmt --all -- --check` が clean。
- [x] full test が回帰しない。
- [x] allow の理由が lint 表に記録されている。
