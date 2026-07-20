---
number: 433
title: refactor(tui): views/new.rs の std::fs::read_dir 直呼びを注入化し 1,245 行のファイル全体 coverage 除外を解消する
status: todo
priority: high
labels: [refactor, tui, review]
dependson: []
related: []
created_at: 2026-07-20T12:00:20.361349+00:00
updated_at: 2026-07-20T12:00:20.361349+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。1 箇所の実 IO 直呼びが 1,245 行の view 全体を coverage 除外に追いやっており、費用対効果が最も高い修復対象。

## 根拠（検証済み）

- `crates/tui/src/presentation/views/new.rs:289` — Tab 補完 `complete_directory`（:273）内の `let Ok(entries) = std::fs::read_dir(&parent) else {`（実 IO の直呼び）。
- `crates/tui/src/presentation/views/mod.rs:11-12` — この read_dir を理由に `pub mod new;` がモジュールごと `#[coverage(off)]` 指定（new.rs は 1,245 行）。

## 問題

「テストできないから」と view 全体を計測対象外に逃がしており、06-conventions の除外規約（実 IO そのものに限る）から逸脱。New フォームの純ロジック（入力検証・描画・状態遷移）が回帰してもゲートに掛からない。

## 改善案（要検討）

- `complete_directory` にディレクトリ列挙関数（`Fn(&Path) -> Vec<String>` 相当）を注入し、実 `read_dir` は合成ルートのアダプタ 1 点に置く。
- `views/mod.rs` のモジュール一括 off を剥がし、必要ならアダプタのみ item off にする。
- fake リスタで Tab 補完（前方一致・隠しファイル・非存在ディレクトリ）をユニットテストで固定する。

## 受け入れ条件

- [ ] new.rs に実 IO 直呼びがなく、モジュール一括 coverage(off) が解消されている。
- [ ] Tab 補完の挙動が fake でテストされている。
- [ ] coverage 100% を維持する。
