---
number: 184
title: feat(orchestration): worker 終端 event と durable owner 通知を追加する
status: done
priority: high
labels: [orchestration, agent]
dependson: [183]
related: []
parent: 182
created_at: 2026-07-10T23:56:43.112615+00:00
updated_at: 2026-07-11T01:00:59.757559+00:00
---

## 背景

worker の成功・失敗・中断通知が prompt 規約に依存すると、通知漏れと重複を状態機械で扱えない。

## やること

- session/agent lifecycle 境界から `pr_opened` / `succeeded` / `failed` / `interrupted` / `timed_out` event を発行する。
- 決定的 event id、worker generation、atomic create、ack を実装する。
- owner live 時は live queue、不在時は launch queue を wake-up に使い、event 自体は ack まで保持する。
- 通知先不在、queue 失敗、再起動、古い generation を扱う。

## 受け入れ条件

- hook の重複実行でも action が一度だけ適用される。
- owner 不在から再起動後に event が適用・ack される。
- stale worker event が現 attempt を完了させない。
- queue 永続化失敗でも event が失われない。
