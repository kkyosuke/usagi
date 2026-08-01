---
number: 604
title: fix(daemon): pre-handshake connection に deadline と同時実行上限を設ける
status: done
priority: high
labels: [review, v2, daemon, ipc, security, resilience, resource]
dependson: []
related: [216, 521, 553]
created_at: 2026-07-31T15:00:00+09:00
updated_at: 2026-08-01T08:37:41+09:00
---

## Finding（P1 runtime/security）

`src/runtime/daemon.rs::start_ipc_accept_loop` は同一 UID の connection を accept するたび stream clone と `usagi-ipc-client` thread を作り、`crates/daemon/src/presentation/ipc.rs::handshake_admitted` は最初の length-prefixed frame を blocking read する。handshake deadline と pre-admission cap がなく、`ClientWorkers` は collection/reap 用で admission を制限しない。prefix を送らない Unix socket を多数保持するだけで thread / FD / memory を枯渇させられる。

## 最小修正方針

accept 後 handshake 完了までを daemon-wide semaphore で bounded にし、短い read/write deadline、frame completion deadline、超過時の確実な close を設ける。admitted connection の上限・idle policyとは分けて観測可能な refusal reason を持たせる。

## テストと受け入れ条件

- prefix 無送信、partial prefix、partial body の socket 群が deadline 後に閉じられ worker/FD が baseline へ戻る。
- cap 超過 connection は worker を無制限生成せず拒否され、正常 client は枯渇中/解消後に bounded latency で接続できる。
- shutdown / rollover collection が handshake 待ち worker を unblock して join できる。
