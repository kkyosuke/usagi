---
number: 488
title: fix(tui): workspace settings を全 production entry path に注入する
status: done
priority: medium
labels: [review, v2, tui, settings]
dependson: []
related: [296, 315, 397]
parent: 453
created_at: 2026-07-20T12:06:50.486704+00:00
updated_at: 2026-07-20T23:27:40.853055+00:00
---

## 問題・影響

root/v2 の `src/runtime/tui.rs::PersistentSettingsPort::open` は workspace scope を `Settings::default()` の in-memory 値にし、save も memory 更新だけである。`open_snapshot_via_controller` / `drive_workspace_controller` / direct `launch_workspace` に workspace settings が注入されず、保存済み modal selection mode 等が通常起動へ反映されない。完了済み #296 の regression である。

## 成立条件 / 再現フロー

workspace local settings で Prompt/Action mode を global と異なる値に保存し、direct と Welcome/Open/Recent から開く。Config surface と Home runtime の値、再起動後の値が一致しない。

## 対象責務と非対象

workspace identity から local settings store を解決し、全 entry path の `SettingsPort` に read/save を注入する。新しい設定項目、global settings schema、Config UX #397 は非対象。

## 受入条件

- [ ] direct と screen graph の全入口が対象 workspace の persisted local settings を読む。
- [ ] workspace save は durable store を lock/atomic write し、次回 entry に反映する。
- [ ] global/local overlay と missing/unknown value fallback が core settings contract と一致する。
- [ ] workspace A の port/state を workspace B に持ち越さない。

## 必須回帰テスト

Prompt/Action mode を含む異なる global/workspace 値で direct/Welcome/Open/Recent、save→reopen、workspace switch、missing/corrupt/unknown fallback を production composition test する。

## docs / 移行影響

`document/03-tui.md` と settings docs に scope resolution と entry lifecycle を追記する。既存 local settings format を再利用し、migration は不要。
