---
number: 745
title: feat(workflow): run の中止・完了・再開始を操作できるようにする
status: done
priority: high
labels: [v2, daemon, tui, workflow]
dependson: []
related: [746, 751]
created_at: 2026-09-12T00:00:00+00:00
updated_at: 2026-09-12T00:00:00+00:00
---

## 問題

`WorkflowCommand` は `Start` と `Instruct` の 2 つしかない。そのため:

- 暴走した run を**中止できない**。Agent タブを終了させても record は `Waiting` で残る。
- `Ready` に到達した run を畳めないため、同じ session で次の goal を始められない
  （`session already has another workflow` で拒否される）。
- やり直しができない。

## 方針

- `WorkflowCommand` に中止・完了・再開始を追加する。いずれも idempotent な operation ID で受理する。
- 中止は「workflow record を終了状態にする」だけとし、**Agent の強制終了や worktree の削除は行わない**
  （既存の「Workflow は session/worktree を削除しない」方針を維持）。
- 完了は `Ready` の run を畳み、次の `Start` を受け付ける状態にする。
- 再開始は前の run の履歴を残したまま新しい run を作る（同じ session に 2 つの active run を作らない）。
- 終了した run の履歴は一定件数だけ保持し、無制限に積み上げない。

## 受入条件

- [x] 中止・完了・再開始が durable な operation として受理され、retry で二重実行しない。
- [x] 中止は Agent を殺さず、worktree も削除しない。
- [x] 完了後に同じ session で新しい `Start` が通る（workflow の記録は解放される。
  なお終了は Agent を残すため、前の run の Agent が生きている間は既存の
  「1 session に 1 Agent」規則が先に効く。これは #745 が挙げた
  `session already has another workflow` の詰みとは別の、意図された規則である）。
- [x] 終了した run の履歴が保持され、上限を超えない。
- [x] `document/03-tui.md` と `document/04-ipc.md` を更新する。

## 実装で変えた方針

中止と完了を別の command にせず、`Finish` 1 つにした。終了時の工程が完了か中止かを
すでに記録しているため、どちらだったかを人に尋ねる必要がない（`PR ready` なら完了、
それ以外なら中止）。再開始も専用 command を置かず、`Finish` のあとの `Start` とした。
どちらも durable な operation として独立に受理・再送でき、2 つを 1 つの command に
畳むと片方だけ失敗したときの再送が表現できなくなる。
