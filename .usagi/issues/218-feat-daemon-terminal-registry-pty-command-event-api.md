---
number: 218
title: feat(daemon): terminal registry と PTY command／event API を実装する
status: todo
priority: high
labels: [daemon, terminal, ipc]
dependson: [216, 217]
related: []
parent: 213
created_at: 2026-07-12T11:39:09.473448+00:00
updated_at: 2026-07-12T12:08:49.188460+00:00
---

## 目的

v1 の daemon PTY／vt100 authority を責務分解して v2 に再実装し、spawn/list/attach/detach/keys/resize/scrollback/kill と snapshot/output/exited を generation-bound な API にする。設計は [terminal API](../../document/proposals/04-daemon-api.md#terminal-api) を正本とする。

## 対象

- daemon usecase の `TerminalRegistry`／attach subscription／input dedupe／kill state machine。
- daemon infrastructure の PTY/process group、vt100 parser、bounded output journal、process identity。
- presentation の terminal command dispatch と typed response/event。
- `LaunchIntent`（agent／shell／recovery）を daemon で settings・allowlistへ解決し、wire の raw command／argv／secret env を廃止。
- canonical registered worktree の再検証、複数 client、複数 Agent pane、resize policy。

## 受け入れ条件

- detach／client disconnect は PTY を生かし、kill だけが process teardown を開始する。
- `Killed` completed ACK は process group の消滅確認と最終 output drain 後に返し、timeout／disconnect は `unknown` として ownership metadata を保持する。
- output byte cursor が連続し、gap は full `TerminalSnapshot` resync、最終 `Output` の後に `Exited` を配信する。
- acknowledged input は PTY 全 byte write 後だけ成功し、同じ request/operation の retryで二重 writeしない。partial write後の失敗はprefix適用済み`ambiguous`として全量retryせず、interactive timeoutは自動retryしない。
- attach登録とinitial snapshot／output cursorを同じterminal actor turnで捕捉し、handshake/attachと同じreadにbufferされたframeを先にdrainする。
- list／attach は別 generation・workspace・session incarnation・worktree の ref を `stale_target` として拒否する。
- pure registry、fake PTY、実 PTY process E2E で detach/re-attach、multi-client、slow client、resize、scrollback、kill ACK、early exit を検証する。
