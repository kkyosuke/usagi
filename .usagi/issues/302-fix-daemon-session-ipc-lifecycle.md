---
number: 302
title: fix(daemon): session 作成時の IPC 公開順序を lifecycle に整合させる
status: done
priority: high
labels: [daemon, ipc, session, lifecycle, regression]
dependson: []
related: []
created_at: 2026-07-15T00:03:14.053296+00:00
updated_at: 2026-07-15T00:04:53.800119+00:00
---

## 背景・根拠

Unix IPC endpoint が singleton lock の取得と daemon PID record の登録より先に公開される。restart の切替中、lock を取得できない replacement が session IPC を受理してから終了でき、session 作成成功の直後に一時的な `daemon failed` 表示を生む。

## 目的

IPC、daemon process lifecycle、session reconcile の順序を整合させ、session 作成の成功を誤って daemon failure として表示しない。

## スコープ

- IPC endpoint を lock 取得・PID 登録の後に公開する。
- session lifecycle の durable operation replay を維持し、reconnect/retry が worktree effect を二重実行しないようにする。
- 未証明の中断 effect は既存どおり fail-closed に reconcile する。

## 受け入れ条件

- lock を取得できない replacement は IPC request を受理できない。
- endpoint の公開時点で PID record は登録済みである。
- 成功済み create の同一 operation ID を daemon restart 後に再送しても worktree 作成は一度だけである。
- 中断した create/remove は自動再実行せず safe failure に収束する。

## テスト方針

- daemon lifecycle の公開順序を境界テストで確認する。
- restart/reconnect 後の create replay を counting fake Git で検証し、worktree create の呼出回数が 1 回であることを確認する。
