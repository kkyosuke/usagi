---
number: 180
title: dev: git diff から fail-safe に推奨テストを提示する
status: in-progress
priority: medium
labels: [test, developer-experience]
dependson: []
related: []
parent: 177
created_at: 2026-07-10T23:35:22.796411+00:00
updated_at: 2026-07-11T00:00:55.081478+00:00
---

## 目的

開発中に `git diff` の touched path から近い test command を提示する。ただし PR gate の全件を置き換えない。

## 最小設計

`scripts/recommend-tests.sh [base]`（名称は実装時決定）を追加し、変更 path と理由、推奨コマンドを表示する。実行は利用者/agent が明示的に行う。

- `src/domain/<m>.rs` → `cargo test --lib domain::<m>::`
- `src/usecase/<m>` → 同 module + 対応 domain module（存在時）
- `src/infrastructure/<m>` → 同 module + 関連 usecase、git/session/storage は broad 扱い
- `src/presentation/<surface>/<m>` → 同 module + 対応 usecase
- `tests/<name>.rs` → `cargo test --test <name>`
- `third_party/vt100/**` → vt100 tests + usagi terminal/TUI tests + full gate required
- Cargo/lock/lib/main/test_support/scripts/hooks/workflows、未知 path、複数層 → `cargo test --quiet` を推奨

mapping はデータ表として SSoT 化し、fixture/table-driven test で代表 path、rename/delete、空 diff、space を含む path、unknown fallback を確認する。Rust の module 宣言とファイル名は常に 1:1 ではないため、自動推論だけに依存せず例外 mapping を明示する。

## ガードレール

出力には「selected tests は fast feedback であり PR 前 full gate の代替ではない」と表示する。exit 0 で何も選ばない状態を作らず、unknown は全件へ倒す。
