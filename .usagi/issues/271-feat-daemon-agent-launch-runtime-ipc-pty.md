---
number: 271
title: feat(daemon): Agent launch runtime を IPC・実 PTY へ接続する
status: done
priority: high
labels: [daemon, agent, ipc, pty]
dependson: [268]
related: [263, 264, 250, 251, 252, 253, 254]
parent: 227
created_at: 2026-07-13T01:33:06.174236+00:00
updated_at: 2026-07-13T03:27:29.103002+00:00
---

## 背景・根拠

#263 は Closeup の `DaemonRequest::Agent`、producer-issued `OperationId`、pending pane、fenced `TerminalRef` による安全な attach を実装済みである。しかし daemon の IPC dispatch は `kind: agent` を accepted として echo するだけで、`AdapterRegistry`、`RuntimeCoordinator`、Codex/Claude adapter、PTY spawn、durable completion event には接続していない。

一方で `RuntimeCoordinator` は reservation → durable snapshot → injected spawn → journal/exit の順序を持ち、Codex/Claude adapter は product-private provision と public launch plan を分離している。実 `PtyTerminal` は存在するが、generic terminal runtime だけが IPC owner に接続されている。このままでは TUI の pending Agent pane が completion を受け取れず、実 Agent を launch/attach できない。

#268 は managed session lifecycle と stable workspace/session/worktree scope resolver の単一書き手であり、Agent launch はその resolver が `available` と証明した scope にのみ許可される。この issue は #268 に依存し、その session lifecycle の write-set を変更しない。

## 目的

daemon-owned `DaemonRequest::Agent` を、stable managed session scope から Codex/Claude profile を解決し、durable admission、実 PTY spawn、terminal stream、fenced completion/replay まで接続する。TUI #263 の pending Agent pane は、同じ `OperationId` の成功 completion が返す完全な `TerminalRef` にだけ attach できる。

## スコープ

- IPC server の shared daemon runtime が `DaemonRequest::Agent` を decode し、canonical `OperationId`、stable workspace/session identity、optional product-neutral profile を検証して Agent owner へ渡す。
- #268 の scope resolver を使い、`available` かつ workspace/session/worktree incarnation が一致する scope だけを Agent launch request に変換する。client supplied path/name/argv/env/secret による再探索・上書きは受け付けない。
- `AdapterRegistry` を daemon 側に置き、default policy と明示 profile を Codex/Claude adapter に解決する。adapter 固有の argv、MCP/hook artifact、credential、raw provision error を IPC/TUI state に露出しない。
- `RuntimeCoordinator` の durable reservation と operation record を Agent IPC admission に接続し、同じ operation の再送/reconnect は同じ accepted/progress/final を返す。同一 ID の異なる semantic request は typed idempotency conflict にする。
- daemon が実 PTY を spawn し、output を journal/stream に drain してから exit を durable に commit する。disconnect は attachment/subscription だけを外し、Agent process、PTY、completion worker を kill しない。
- Agent success final/event は operation、workspace/session/worktree、daemon generation、execution/lifecycle attempt を fence した `TerminalRef` を返す。failure/ambiguous/stale completion は safe feedback だけを返し、TUI/local runtime に replacement spawn や terminal 推測を許可しない。
- IPC connection、Agent owner、terminal attach/stream を一つの shared daemon runtime として composition root に束ねる。generic terminal (#264) の owner loop と ownership vocabulary を複製せず、共通 terminal registry/stream contract を利用する。
- fake adapter/provisioner/store/PTY による deterministic integration test と injected real-PTY regression を追加する。

## 対象外

- managed session lifecycle、worktree create/remove、scope resolver 自体の実装・変更（#268）。
- Closeup command UX、pending tab reducer、renderer、TUI-side attach policy の再実装（#263）。
- generic shell terminal launch/attach の再設計（#264、#265）。
- CLI/MCP の新しい Agent UX、client supplied raw command/argv/environment、daemon crash 後の PTY FD continuation。
- Codex/Claude 以外の product adapter の追加、model allowlist や adapter private configuration の UI 化。

## 依存・境界

| Issue | 境界 |
|---|---|
| #268 | stable managed session scope を消費する唯一の hard dependency。本 issue は lifecycle runtime を変更しない。 |
| #263 | TUI の launch intent / pending pane / fenced attach を consumer として検証し、TUI reducer/renderer を再実装しない。 |
| #264 | terminal IPC ownership/attach vocabulary を再利用する。generic shell の launch policy は変更しない。 |
| #250–#254 | Codex/Claude adapter と common runtime の既存 contract を実 runtime に組み立てる。adapter argv/provision contract の再設計はしない。 |

## 受け入れ条件

- #268 が返す fully fenced available session scope でのみ Agent launch が accepted される。creating/deleting/failed/stale scope、workspace/session/worktree mismatch、path/name-only target は typed safe error となり、PTY spawn しない。
- `DaemonRequest::Agent` は IPC で echo されず、same semantic `OperationId` の再送・reconnect では同じ durable operation/revision を返す。同じ ID で異なる intent は idempotency conflict となり、二重 spawn しない。
- default profile と explicit `codex`/`claude` profile は daemon registry で解決され、adapter が one-shot provision した public plan だけを durable snapshot に保存する。argv、environment values、secret、raw provision error は wire event・snapshot・TUI feedback に現れない。
- successful launch は reservation を persist してから実 PTY を一度だけ spawn し、output journal/terminal registry を開始する。spawn failure/ambiguous effect/persist-after-spawn は fenced safe failure または reconcile-required として保存され、replacement spawn を推測しない。
- operation の accepted/progress/final/replay が durable に取得でき、success final は matching operation と complete `TerminalRef` を返す。late/duplicate/wrong-generation/wrong-scope completion は pending pane を attachable にしない。
- launch → accepted → output → attach → input → detach → reattach → exit の fake IPC + injected fake PTY E2E と、Codex/Claude の少なくとも一方を用いる injected real-PTY regression を通す。client disconnect は process/PTY を残し、terminal attach のみを外す。
- TUI #263 integration test で pending Agent pane が same-operation の fenced success final にだけ attach し、daemon unavailable/rejection/ambiguous/stale final では local spawn、retry、name/path lookup を行わない。
- 実装済みの IPC/daemon contract を `document/04-ipc.md` と `document/05-daemon.md` の正本に更新し、#263 の TUI contract と相互リンクする。

## 実装順序

1. #268 の scope resolver を input port として Agent owner/admission API に接続し、operation/idempotency/fence の fake tests を置く。
2. Codex/Claude adapter registry、durable Agent runtime store、terminal registry/stream の shared owner を実装する。
3. real PTY adapter と reader/drain/exit worker を composition root に束ね、IPC connection handler から Agent/terminal の両 request を shared runtime へ routing する。
4. completion/replay event を TUI #263 adapter へ流す fake IPC E2E と injected real-PTY regression を追加し、正本ドキュメントを実装に合わせて更新する。
