---
number: 55
title: fix(tui-home): 右ペイン状態（view/tabs/バッジ）の二重書き込みを単一所有化する
status: done
priority: high
labels: [refactor, tui, review]
dependson: []
related: []
created_at: 2026-06-19T22:15:34.983774+00:00
updated_at: 2026-07-01T22:25:31.456655+00:00
---

## 背景

home 画面の右ペイン状態（`terminal_view` / `terminal_tabs` / バッジ集合 `running`/`waiting`/`live`/`done`）を **event loop と pane driver の 2 者が書いている**。

- event loop 側: `src/presentation/tui/home/event/mod.rs:176-179`, `:206-214`（フレーム冒頭で clear → 再導出）
- pane driver 側: `src/presentation/tui/home/terminal_pane.rs:234-237`（没入中に `set_terminal_view` 等で直接書き込み）

「制御が厳密に受け渡される」という慣習だけで正しさが保たれており、**型でもコメントでも所有権が強制されていない**。pane が制御を yield するタイミングを将来変えると stale スナップショットが残るリスクがある（本サブシステム唯一の correctness リスク）。

## 対応方針

- 書き込み主体を片方に一本化する（例: event loop を唯一の writer とし、pane は借用スナップショットから描画して書き戻さない）。
- もしくは右ペイン状態を専用の所有型に括り出し、`&mut` を持てる主体を 1 つに限定して型で強制する。

## 確認方法

- 切替（Switch）⇔ 没入（Focus/Attached）を往復しても view/tabs/バッジが常に最新を指すこと。
- 既存テストが通ること（カバレッジ 100% 維持）。
