---
number: 268
title: feat(daemon): durable session lifecycle runtime を IPC server に接続する
status: done
priority: high
labels: [daemon, session, lifecycle, ipc]
dependson: [217, 219, 220]
related: [257, 263, 264, 265]
parent: 213
created_at: 2026-07-13T00:30:00.000000+00:00
updated_at: 2026-07-13T01:35:47.126081+00:00
---

## 背景・根拠

#219 は durable reducer、`DaemonLifecycleStore`、および pure control vocabulary を導入したが、実行中の daemon には接続していない。現在の `crates/daemon/src/presentation/ipc.rs` は `kind: session` の operation ID を見て `Accepted` を返し request body を echo するだけであり、lifecycle store の初期化・永続 operation・worktree effect・snapshot・reconcile worker を所有しない。合成 root の socket server もこの stateless dispatch を thread ごとに呼ぶだけである。

そのため #257 の Overview/Ctrl+A create と #263 の Closeup agent launch は、有効な stable session/worktree scope を実 runtime から得られない。#264 は generic terminal IPC runtime を担当するため、terminal attach/stream/PTY ownership とその request vocabulary は本 issue の write-set に含めない。

## 目的

daemon を managed session lifecycle の唯一の実行時書き手にする。session create/list/overview/remove を durable operation、stable identity、revision 付き snapshot、crash/restart reconcile を通じて提供し、available session だけが agent launch/delegation に渡せる fenced worktree scope を解決できるようにする。

## スコープ

- daemon start 時に workspace identity を確定し、`DaemonLifecycleStore` を initialize/load/validate する。legacy `state.json` は managed lifecycle へ推測移行しない。
- IPC dispatch を injected daemon-owned session runtime へ接続し、typed create/list/overview/remove/get/reconcile を実処理する。request correlation cache と producer-issued `OperationId` の semantic-idempotency を保持する。
- create は reducer で reserve して durable journal に記録してから worktree を作成し、completion fence が一致したときだけ `available` を保存・通知する。失敗は safe failure にして `failed` を保存する。
- remove は新規 launch/delegate reservation より先に `deleting` を保存し、fenced target の worktree remove 成功後だけ record を消去する。old worker は remove/recreate 後の同名 session を変更できない。
- state revision を持つ workspace/session snapshot と subscribe/resume/resync を供給し、Overview の session rows は daemon snapshot だけから構成する。
- restart 時は journal と physical worktree を reconcile する。effect の完了を証明できない create/remove は自動再実行せず `ambiguous`/safe recovery にする。
- agent launch/delegate が利用する session scope resolver を提供する。`available` かつ stable `WorkspaceId`/`SessionId`/`WorktreeId` が一致する場合だけ path を返し、client supplied path/name による再探索を許可しない。
- composition root が一つの runtime instance を全 IPC connection で共有するように組み立てる。daemon process と接続をまたいで同じ durable state/operation を使う。

## 対象外

- generic terminal launch、attach、input、resize、PTY stream、terminal subscription の runtime 接続（#264）。
- Closeup の command UX / pane attach（#263, #265）、Ctrl+A フォーム UX（#257）の再実装。
- agent adapter の argv、hook、MCP injection、PTY spawn。ここでは validated managed scope を渡す境界だけを実装する。
- legacy `WorkspaceState` の managed state への自動移行。

## 受け入れ条件

- Unix IPC integration で create → accepted → progress/final → snapshot が worktree と durable state を一致して反映し、別 connection/restart 後も list/overview が同一 `SessionId` を返す。
- 同じ `OperationId`/semantic request の再送は同じ operation を返し、同一 ID の異なる body は `idempotency_conflict` になる。response loss/disconnect は local fallback や二重 worktree create を起こさない。
- remove は durable `deleting` を先に可視化し、成功後に snapshot から消える。dirty/error/unknown side effect は session identity を失わせず、safe failure または ambiguous state になる。
- create/remove completion は workspace/session/operation/generation/execution/lifecycle attempt/revision を fence する。late completion、逆順 snapshot、remove 後の同名再作成は現 incarnation を変更しない。
- daemon restart/crash injection で creating/initializing/deleting の reconcile を検証し、証明不能な external effect を自動再実行しない。
- scope resolver は available session の完全な stable identity にだけ worktree scope を返す。creating/deleting/failed、stale ID、name/path-only 指定は typed safe error になる。
- #264 と同じ terminal IPC dispatch/registry/PTY runtime files を変更せず、session runtime の integration test は fake worktree runner と in-memory Unix stream で create/list/overview/remove/restart/fence を通す。

## 実装順序

1. typed session snapshot/request/response と runtime port を `usagi-core` に定義し、state-store-backed fake integration test を置く。
2. daemon session runtime に durable admission、worker、worktree adapter、reconcile、scope resolver を実装する。
3. shared runtime を IPC server と composition root へ接続し、socket-level integration test を追加する。
4. 実装済み契約を `document/04-ipc.md` と `document/05-daemon.md` に更新し、#257/#263 の fake client を real snapshot contract に切り替える。
