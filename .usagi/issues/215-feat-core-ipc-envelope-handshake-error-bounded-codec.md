---
number: 215
title: feat(core): IPC envelope・handshake・error・bounded codec を実装する
status: todo
priority: high
labels: [core, ipc, protocol]
dependson: [214]
related: []
parent: 213
created_at: 2026-07-12T11:38:29.898924+00:00
updated_at: 2026-07-12T12:09:54.278892+00:00
---

## 目的

現行 `Ping/Pong` と無上限 allocate の frame reader を、version negotiation、correlation、server push、resume/resync、idempotency を表現できる transport-independent protocol core へ置き換える。設計は [v2 IPC protocol proposal](../../document/proposals/03-ipc-protocol.md#ipc-envelope-と-handshake) を正本とする。

## 対象

- `ClientHello`／`ServerHello`: protocol generation/revision range、capabilities、build identity、client/daemon generation、limits。
- request／response／event envelope と `RequestId` correlation、producer-issued `OperationId` の accepted response。
- `StreamRef/stream_sequence` と resource revision／terminal output cursorを分離したresume token／`resync_required`。
- stable machine error code、retry mode、side-effect state、safe details、current generation/revision。
- `(ClientId, RequestId)` のbounded response cacheと、`OperationId + target scope + semantic digest`のdurable idempotency decision。
- u32 BE length framing の上限、partial prefix/payload、invalid JSON/unknown required capability の fail-closed。

## 受け入れ条件

- build identity は診断情報であり、互換可否を protocol generation/revision/capability から決定する。
- handshake 完了前の通常 request と、target daemon generation 不一致 request を拒否する。
- response と push が同じ connection で交差しても RequestId／`StreamRef + stream_sequence` で一意に振り分けられる。
- frame 長は payload allocate 前に検証し、空・分割・連結・過大・途中 EOF を pure/fake IO test で網羅する。
- request timeout／response loss は side effect の失敗を意味しない。同processのRPC retryは同じRequestId、process/connectionをまたぐdurable mutationは同じOperationIdで同じoutcomeへ収束する。
