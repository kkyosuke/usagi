---
number: 498
title: fix(v1/config): TUI と env editor の設定保存を revision-aware にする
status: in-progress
priority: medium
labels: [review, v1, tui, config, concurrency]
dependson: []
related: [15, 22, 29, 153]
parent: 453
created_at: 2026-07-20T12:07:08.331319+00:00
updated_at: 2026-07-21T13:24:40.886304+00:00
---

## 問題・影響

出荷中 v1 の TUI Config は長時間保持した full settings snapshot を `settings::save` し、home env editor も load→mutate→full save する。別 TUI/MCP/CLI process の変更後に保存すると、store lock は write 単体しか守らず disjoint field まで lost update する。

## 成立条件 / 再現フロー

TUI Config を開いたまま別 process が field A を更新し、TUI で field B を保存する。古い full snapshot が A を元へ戻す。同様に env editor 同士または Config と env editor で再現する。

## 対象責務と非対象

v1 TUI Config/env editor の revision-aware CAS または lock 内 field patch、conflict UX を対象とする。CLI external editor は #499、settings schema/新項目は非対象。

## 受入条件

- [ ] editor open 時の revision/content identity を保存し、save 時に concurrent change を検出する。
- [ ] disjoint field edits は明示 merge policyで保持し、same-field conflict は利用者へ提示して無断上書きしない。
- [ ] load→patch→save は 1 store transaction/CAS とし、full stale snapshot を保存しない。
- [ ] draft は conflict/error 後も保持する。

## 必須回帰テスト

2 writer barrier で Config+setter、env+setter、Config+env の disjoint/same field、save failure、retry/draft retention を検証する。

## docs / 移行影響

v1 Config/env editor docs に conflict/merge UX を記載する。settings schema migration は不要だが revision metadata を加える場合は legacy file の初期値を定義する。
