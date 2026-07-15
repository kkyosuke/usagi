---
number: 93
title: feat(tui): ステータスラベルキーの発見可能性（チートシート/フッター追加）
status: done
priority: medium
labels: [tui, review]
dependson: []
related: []
created_at: 2026-07-03T23:21:27.413912+00:00
updated_at: 2026-07-03T23:21:27.413912+00:00
---

UI/UX レビュー（2026-07 branch `usagi/ui`）由来。

## 問題
切替のステータスラベル操作（`Tab`/`Shift+Tab` でラベル循環、`1`〜`9` で直接指定、`0` で解除。#560/#419）が、切替フッター（`home/ui/chrome.rs`）にも `?` チートシート（`home/ui/content.rs` の `cheatsheet`）にも**出ない**。直近追加機能なのに画面内の発見手段がゼロ。04-keys.md は「全キーの一覧は `?` で確認できます」と謳っており実装と乖離。

## 対応
- チートシートの Switch グループに `Tab / Shift+Tab`（ラベル循環）・`1-9 / 0`（直接指定/解除）の行を追加。
- 切替フッターに簡潔な 1 セグメント（例 `Tab label`）を追加検討。

## 受け入れ条件
- `?` チートシートにステータスラベルのキーが載る。
- カバレッジ 100% 維持。
