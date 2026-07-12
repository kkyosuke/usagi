---
number: 212
title: docs(ipc): v2 IPC／ID 契約と実装ロードマップを設計する
status: done
priority: high
labels: [design, ipc, daemon]
dependson: []
related: [159, 163, 209]
created_at: 2026-07-12T11:30:19.508414+00:00
updated_at: 2026-07-12T12:09:54.278892+00:00
---

## 目的

v2 の daemon 所有モデルを前提に、ID 不変条件、IPC envelope／transport、terminal API、session/control API、security、restart 契約、clean architecture 配置を未実装 proposal として具体化する。v1 実装を丸ごと移植せず、MVP から段階導入できる実装 issue に分割する。

## 調査対象

- `document/02-architecture.md`
- `document/proposals/01-entry-surfaces.md`
- `v1/document/proposals/02-daemon.md`
- `v1/document/proposals/05-session-lifecycle.md`
- v1 daemon IPC／PTY 実装・テストと関連 issue
- 現行 v2 の実装・テスト

## 完了条件

- 誤配送・二重 spawn・stale update を防ぐ ID／generation／revision 不変条件が表と ASCII 図で定義されている。
- handshake、correlation、push、resume/resync、idempotency、backpressure、timeout、error taxonomy が wire 契約として定義されている。
- terminal と session/control の command／event、ACK、複数 client、long operation 契約が定義されている。
- socket／peer／workspace／launch intent の security boundary と daemon crash 時の非継続契約が明記されている。
- clean architecture の配置、MVP、テスト戦略が定義され、実装 issue が依存順に分割されている。
- proposal、issue、Markdown link check、commit、push、PR が完了している。
