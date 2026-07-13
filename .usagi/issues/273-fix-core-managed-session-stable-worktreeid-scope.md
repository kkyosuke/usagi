---
number: 273
title: fix(core): managed session の stable WorktreeId scope を永続化する
status: done
priority: high
labels: [core, daemon, session, lifecycle]
dependson: [268]
related: [271, 263, 264]
parent: 213
created_at: 2026-07-13T01:42:29.996225+00:00
updated_at: 2026-07-13T01:43:50.176467+00:00
---

## 背景

#908 で daemon の durable session lifecycle runtime は接続されたが、`ManagedSession` は `SessionId`、name、lifecycle、attempt だけを保持し、physical checkout incarnation を表す `WorktreeId` を永続化していない。また `SessionRuntime` は available session を stable workspace/session/worktree scope と path に解決する API を公開していない。

そのため `DaemonRequest::Agent`（#271）は session ID だけから worktree path/name を再探索するか、再起動ごとに新しい `WorktreeId` を生成するしかなくなる。どちらも stale completion、remove/recreate、path reuse を別 incarnation と区別できず、fenced `TerminalRef` の安全性を満たさない。

## 目的

managed session lifecycle の一部として `WorktreeId` を create reservation 時に一度だけ発行・永続化し、daemon-owned scope resolver が available な完全 identity と canonical worktree path を返せるようにする。

## スコープ

- `ManagedSession` に stable `WorktreeId` を追加し、create/recreate は fresh ID、既存 lifecycle state の deserialize は安全に拒否または明示 migration policy に従う。
- create / remove / completion fence / snapshot / reducer tests を worktree incarnation と整合させる。
- `SessionRuntime` に workspace ID、session ID、lifecycle available、stored `WorktreeId` の全一致を検証する read-only scope resolver を追加する。
- resolver は daemon-owned canonical session path を返し、name/path-only lookup、creating/deleting/failed、stale ID を typed safe error にする。
- restart/load、remove 後の同名再作成、old scope による launch を regression test で固定する。

## 対象外

- Agent adapter、PTY spawn、Agent operation/completion（#271）。
- generic terminal runtime（#264）。
- legacy state の暗黙 migration。既存 stored lifecycle data の compatibility 方針は fail-closed を基本とする。

## 受け入れ条件

- available session は durable `WorkspaceId + SessionId + WorktreeId` と canonical path を持ち、daemon restart 後も同じ `WorktreeId` を返す。
- remove/recreate した同名 session は異なる `SessionId` と `WorktreeId` を持ち、古い scope の解決・launch は拒否される。
- resolver は client supplied name/path を引数に取らず、available 以外または workspace/session/worktree mismatch を typed safe error にする。
- lifecycle snapshot、fence、store round-trip、fake Git integration test が worktree incarnation を検証する。
