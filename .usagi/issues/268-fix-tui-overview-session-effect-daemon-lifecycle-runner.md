---
number: 268
title: fix(tui): Overview session effect を daemon lifecycle runner へ接続する
status: todo
priority: high
labels: [tui, overview, session, lifecycle, daemon]
dependson: []
related: [260]
created_at: 2026-07-13T00:48:33.719911+00:00
updated_at: 2026-07-13T00:49:48.128755+00:00
---

## 背景

#260 は Overview の `session create <name>` / `list` / `overview` / `remove [--force]` を controller の typed `Effect` に正規化した。しかし、実際に起動される Workspace UI は legacy `WorkspaceView` のループであり、controller の `BackendPort`、`SessionLifecycleAdapter`、daemon IPC client を生成・dispatch していない。そのため実行時の Overview modal は session command を `NotImplemented` として表示するだけで、daemon lifecycle effect runner へ届かない。

## 目的

起動中の Workspace UI を controller と daemon-authoritative lifecycle runner に接続し、Overview の supported session command を実際の daemon lifecycle operation / snapshot refresh として実行する。

## スコープ

- Overview modal の `session` command を controller の typed effect 経由で runtime backend に dispatch する。
- create/remove は producer-issued `OperationId` を保った daemon lifecycle operation とし、成功・失敗・disconnect/reconnect/replay を既存 adapter policy どおり reducer へ投影する。
- list/overview は daemon snapshot を refresh し、ローカル `state.json` の再読込を mutation/read model に使わない。
- remove は active の stable `SessionId` に限り、root と stale target は safe notice にする。
- 実ランナーと socket lifecycle scenario を追加し、controller-only fake test で終わらせない。
- 実装済み仕様 document を runtime の実態に合わせて更新する。

## 対象外

- daemon lifecycle protocol / worktree / PTY worker の再設計。
- session note / todo / PR / prompt 操作の接続。
- Closeup command の追加。

## 受け入れ条件

- 対話 Workspace runtime から Overview の各 supported session command が daemon lifecycle runner まで到達する。
- session command は generic `WorkspaceCommand` や `NotImplemented` stub を経由しない。
- operation ID は一度だけ発行・送信され、ack 喪失時に local fallback / blind retry を行わない。
- list/overview は daemon snapshot を表示へ反映し、同名や表示順で identity を解決しない。
- pure controller test に加え runtime adapter と socket lifecycle integration scenario が success、validation、force、root/stale、disconnect/replay を検証する。
- `document/03-tui.md` と `document/01-overview.md` が実装済みの挙動だけを記載する。
