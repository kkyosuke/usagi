---
number: 206
title: fix(orchestration): build 不一致 daemon で新規 queued session の autostart が永久停止する
status: done
priority: high
labels: [fix, orchestration, daemon]
dependson: []
related: [205]
created_at: 2026-07-11T11:03:12.309830+00:00
updated_at: 2026-07-11T11:13:29.557807+00:00
---

## 症状

`autostart_queued_prompts=true` でも、実行中 daemon と TUI の build handshake が不一致だと、新規 session の queued prompt が毎回 requeue され agent_phase=none のまま進まない。

## 原因

unattended launch は build handshake failure を一律拒否する。同一 worktree を daemon が所有していないことが永続 terminal registry から確認できる場合も拒否するため、安全な新規作業まで永久停止する。

## 方針

- daemon terminal registry の存在と対象 worktree の ownership を確認する。
- registry が存在し対象 worktree の terminal がない場合だけ unattended local fallback を許可する。
- registry 不在・破損・対象記録ありは従来どおり拒否し、二重起動を防ぐ。
- policy を純粋関数化し、回帰テストと仕様を更新する。

## 受け入れ条件

- build mismatch 中でも ownership known-absent の新規 queued session は自動起動できる。
- ownership present/unknown の unattended launch は拒否する。
- user initiated と通常 daemon unavailable の既存 fallback は不変。
