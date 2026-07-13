---
number: 260
title: feat(tui): Overview から daemon 管理 session を操作する
status: todo
priority: high
labels: [tui, overview, session, lifecycle]
dependson: [217, 219, 220, 231, 234, 257]
related: [226]
created_at: 2026-07-13T00:09:23.746382+00:00
updated_at: 2026-07-13T00:09:23.746382+00:00
---

## 背景

Overview の `session` は registry 表示と generic `WorkspaceCommand` effect までで、handler は NotImplemented stub である。Closeup の `close` は stable `SessionId` を持つ typed remove effect へ変換され、Home の create は daemon-authoritative lifecycle intent を使用する。Overview だけがこの既存経路に接続されていない。

## 目的

Workspace scope の Overview から、daemon を唯一の mutation owner として session create / list / overview / remove を実行可能にする。TUI は store、git worktree、PTY を直接操作せず、typed effect と既存 SessionLifecycle adapter / daemon client の境界だけを通る。

## スコープ

- `session create <name>` は Home の validated product-neutral create intent と同じ typed lifecycle effect に正規化する。profile/model の CLI 固有文法はこの command に持ち込まない。
- `session list` と `session overview` は daemon snapshot の読み取り投影を再利用し、local state / state.json を直読しない。
- `session remove <stable target> [--force]` は selected SessionId を解決した後、Closeup と同じ typed remove lifecycle effect に正規化する。root、曖昧 name、古い snapshot、invalid force は安全な notice で拒否する。
- accepted/progress/final/reconnect/stale/duplicate は #231/#234 の operation/reconcile policy に従い、local fallback や operation ID の再生成をしない。

## 対象外

- daemon lifecycle、IPC wire、worktree/PTY worker、CLI/MCP syntax の再設計。
- Closeup command の機能拡張。
- session note / todo / PR / prompt 操作。

## 受け入れ条件

- Overview の各 supported session subcommand は typed controller effect に変換され、generic raw `WorkspaceCommand` に session mutation を残さない。
- create/remove は producer-issued OperationId を一度だけ使い、接続断時にも local fallback や blind retry をしない。
- list/overview は daemon snapshot projection を表示し、同名再作成や snapshot 消失で name/path を identity に用いない。
- pure parser/controller/adapter fake と socket lifecycle scenario で success、validation、ambiguous target、force、disconnect/replay/stale を検証する。
- 実装済み仕様 document を更新する。

## 依存・境界

#257 の Home create と Closeup remove の typed effect を正本として再利用する。#259 は入力補完だけを先に扱い、本 issue の実行 semantics は持たない。
