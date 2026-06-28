---
number: 81
title: feat(tui): uniteスコープ解決 — c/r/close・コマンドパレットをカーソルグループ基準に
status: todo
priority: medium
labels: [feat, tui]
dependson: [80]
related: []
parent: 77
created_at: 2026-06-28T00:08:28.759963+00:00
updated_at: 2026-06-28T00:08:28.759963+00:00
---

親 #77 のフェーズ4。ワークスペーススコープのコマンドを統合対応に。

- `c`(新規作成)・`r`(表示名)・`close`・`session create/remove`・`issue`・`config` の対象ワークスペースを**カーソルがいるグループ**から解決する。
- `event::Wiring` の create/remove/preview などの closure を、単一 `workspace_root` ではなく行のグループ root を受けて動くよう改修。
- `resume-focus` のキーを (workspace, session) で修飾（フェーズ1の名前修飾と整合）。
- フッター/スコープ表示にカーソルグループのワークスペース名を出す。

## 確認方法

- 単一グループは挙動不変。複数グループでカーソルグループに作成/削除/設定が効く。
- `cargo fmt` / `clippy` / `test`（カバレッジ 100%）。
