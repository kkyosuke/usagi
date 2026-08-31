---
number: 623
title: fix(daemon): session remove の operation identity に force intent を含める
status: done
priority: medium
labels: [review, v2, daemon, session, lifecycle, idempotency, correctness]
dependson: []
related: [268]
created_at: 2026-08-02T22:57:15.800325+00:00
updated_at: 2026-08-02T23:53:28.898452+00:00
---

## Finding（P2 correctness / idempotency）

`crates/daemon/src/usecase/session_runtime.rs::SessionRuntime::begin_remove` は `force(payload)` を読み、`DeletePlan.force` として worktree teardown の effect を変える。一方、durable な `OperationJournal.semantic_key` は `semantic_key(SessionAction::Remove, &name)` だけで、`force` を含まない。

このため、同じ `OperationId` と session name を使いながら `force: false` と `force: true` を入れ替えた異なる request が `idempotency_conflict` にならず、先に記録された operation の accepted / failed / succeeded outcome を replay する。

## 発生条件と影響

1. client が `session_remove(name, force=false, operation=O)` を送る。
2. 応答消失・client bug・operation ID の誤再利用後に、同じ `O` と `name` で `force=true` を送る（逆順も同じ）。
3. daemon は semantic key が一致すると判断し、後続 request を別 intent として拒否しない。

`force=false → true` では caller が要求した強制 teardown が実行されず、以前の failure / outcome が別 request の結果として返る。`force=true → false` では、すでに行われた強制 effect が non-force request と同一 intent だったように replay される。いずれも durable mutation の相関が壊れ、caller はどの intent が effect を所有したか判別できない。

## 具体的根拠

- `SessionRuntime::begin_remove` は request の `force` を解析して `DeletePlan { force, ... }` を永続化する。
- 同関数の operation 再利用判定は `semantic_key(SessionAction::Remove, &name)` の一致だけを見る。
- `document/04-ipc.md#attempt-deadline-と-reconnect-budget` は durable mutation を同じ producer `OperationId` と semantic digest で照合し、異なる intent は conflict にする。
- #268 の受入条件も、同じ `OperationId` の異なる body を `idempotency_conflict` とする。
- create は role を `create_semantic_key` に含めているため、effecting option を durable identity に含める既存 precedent がある。

## 修正方針

remove の canonical semantic identity に session name と `force` を含め、同じ operation ID の異なる force intent を effect zero の `idempotency_conflict` で拒否する。既存 journal の旧 key は force intent を証明できないため、無条件に現行 request と同一視せず、互換 replay と fail-closed refusal の境界を明示する。

compensating teardown の `delete_branch` / origin は client request と別の内部 intent なので、同じ identity builder に含めるべき範囲も合わせて固定する。

## 必要な回帰テスト

- 同じ `OperationId` / name の `force=false → true` と `true → false` が conflict になり、後続 effect は 0 回。
- 同じ `OperationId` / name / force の再送は accepted・failed・succeeded の各段階と daemon restart 後に同じ outcome へ収束し、worktree effect は 1 回だけ。
- response loss を模した再送でも上記が変わらない。
- legacy semantic key を持つ snapshot の same/different intent 判定が、定めた互換・fail-closed 契約どおりになる。
- compensating teardown と通常 remove が誤って同じ durable intent として相関しない。
