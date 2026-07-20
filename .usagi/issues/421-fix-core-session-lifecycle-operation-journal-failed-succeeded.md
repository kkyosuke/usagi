---
number: 421
title: fix(core): session_lifecycle の operation journal が Failed 経路でも Succeeded を記録する問題を直す
status: todo
priority: medium
labels: [fix, core, review]
dependson: []
related: []
created_at: 2026-07-20T11:57:19.469500+00:00
updated_at: 2026-07-20T11:57:19.469500+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。

## 根拠（検証済み）

- `crates/core/src/domain/session_lifecycle.rs:497` — `complete()` が `op.status = OperationStatus::Succeeded;` を設定（:443 にも Succeeded 代入）。
- `OperationStatus::Failed` / `Cancelled` / `Ambiguous` の**代入箇所は全コードベースでゼロ**（grep 検証。Succeeded の代入は :443・:497・テスト :757 のみ）。

## 問題

durable operation journal が「操作が失敗した」ことを表現できず、Failed イベントでも Succeeded として記録される。crash 後のリカバリや冪等判定で journal を信頼できない（Failed/Cancelled/Ambiguous は定義だけ存在する見せかけの語彙になっている）。

## 改善案（要検討）

- Failed 経路（create 失敗・remove 失敗等）で `op.status = Failed` を記録する reducer 遷移を追加する。
- Cancelled / Ambiguous は使う計画がなければ削除する（記載＝実装済みの原則）。

## 受け入れ条件

- [ ] 失敗イベントに対して journal に Failed が記録されることがテストで固定されている。
- [ ] 未使用の status variant が「使われる」か「削除される」かのどちらかになっている。
- [ ] coverage 100% を維持する。
