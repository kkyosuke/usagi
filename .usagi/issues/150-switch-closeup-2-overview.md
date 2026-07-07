---
number: 150
title: Switch / Closeup 2モード化と Overview / Focus モーダル化
status: done
priority: medium
labels: [design, tui]
dependson: []
related: []
created_at: 2026-07-07T10:03:08.521988+00:00
updated_at: 2026-07-07T10:03:08.521988+00:00
---

## 完了内容

ホーム画面のトップレベル mode を `Switch` / `Closeup` の 2 つに整理した。

- `Switch`: セッション群の操作。
- `Closeup`: 選択中セッションの中の操作。ライブ端末は Closeup の内部状態。
- `:` は Workspace スコープの **Overview モーダル**を開く。
- `Ctrl-O a` は Session スコープの **Focus モーダル**を開く。
- 旧 `Attached` は top-level mode から外し、Closeup の `closeup_attached` sub-state にした。
- design/home の正本ドキュメントを実装済み仕様に更新した。

## 確認

- `cargo check`
- `cargo test presentation::tui::home --quiet`
