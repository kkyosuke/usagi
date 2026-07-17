---
number: 309
title: fix(daemon): generic terminal launch scope を authoritative に検証する
status: done
priority: high
labels: [daemon, terminal]
dependson: []
related: []
created_at: 2026-07-17T11:10:16.366072+00:00
updated_at: 2026-07-17T11:22:19.411972+00:00
---

## 目的
client が送る generic terminal の workspace/session/worktree scope を SessionRuntime の available managed session と照合し、実際の cwd と同一の authoritative scope でのみ launch する。

## 完了条件
- scope resolver port を terminal runtime に注入する。
- unavailable/mismatched scope は spawn 前に安全に拒否する。
- TerminalLaunchRequest scope と resolved cwd の一致を回帰テストで保証する。
- daemon 仕様ドキュメントを更新する。
