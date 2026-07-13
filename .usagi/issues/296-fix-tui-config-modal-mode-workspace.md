---
number: 296
title: fix(tui): Config の Modal mode を永続化して Workspace へ反映する
status: done
priority: high
labels: [tui, settings, persistence]
dependson: []
related: [244, 263, 264, 268, 271]
created_at: 2026-07-13T12:25:43.605504+00:00
updated_at: 2026-07-13T12:37:41.177429+00:00
---

## 背景

`ModalSelectionMode`（Action / Prompt）は domain の serde、Config の draft/save、Workspace UI への selection-mode 注入を持つ。しかし実 TUI 合成層は `VolatileSettingsPort` を使うため、Save はメモリだけで消える。さらに `usagi open <path>` の Workspace 直接起動は常に Action を渡しており、保存済み Prompt を無視する。

## 目的

Config の global Modal mode を durable settings に保存し、Welcome/Open/Recent と Workspace 直接起動の次回 Workspace entry へ一貫して反映する。

## スコープ

- 既存 domain の `ModalSelectionMode` と global scope を維持する。workspace/session の mode scope は新設しない。
- global `settings.json` を atomic に読み書きする production `SettingsPort` を TUI composition root に接続する。
- Config の Save 成功時だけ保存値を更新し、Esc/cancel、validation/load/save error は既存 draft/notice semantics を維持する。
- Workspace entry 時に保存済み global mode を読み、Overview と Closeup の両 modal に渡す。実行中 Workspace の focus、session selection、Switch/Closeup は変更しない。
- missing settings、metadata 欠損、未知/旧 mode は既存 serde fallback の Action に安全に収束する。

## 受け入れ条件

- Prompt を Save 後に TUI を再起動して Config を開くと Prompt が復元される。
- Welcome/Open/Recent と `usagi open <path>` のどの入口でも、次回 Workspace の Overview/Closeup が保存済み mode になる。
- Esc/cancel と保存失敗は mode を durable state や Workspace UI へ適用しない。
- unknown/old token と missing field は Action へ安全に fallback する。
- store、runtime settings adapter、Workspace modal selection の regression test を追加する。

## 対象外

- environment binding、daemon IPC、Agent/terminal launch、workspace/session scoped mode。
