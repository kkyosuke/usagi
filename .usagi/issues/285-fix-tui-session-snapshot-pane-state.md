---
number: 285
title: fix(tui): session snapshot と pane state を同期する
status: done
priority: high
labels: [tui, session, sidebar, regression]
dependson: []
related: [257, 258, 280, 281]
created_at: 2026-07-13T11:54:00.338601+00:00
updated_at: 2026-07-13T11:57:34.244310+00:00
---

## 背景

Overview の session create 完了で daemon snapshot は sidebar に反映されるが、新規 session の pane state が初期化されない。新しい行を選択すると `Workspace::pane()` が pane map の欠損を panic し、作成直後の選択・描画が成立しない。

## 完了条件

- daemon snapshot の反映時に sidebar rows、選択、session-scoped pane state を同じ session 集合へ同期する。
- 作成直後の session が skeleton 解消後に選択でき、Closeup を開いても panic しない。
- 削除済み session の local pane state を保持しない。
- snapshot 反映後の選択・pane 操作を検証する回帰 test を追加する。
