---
number: 208
title: feat(daemon): durable start claim と冪等な queued prompt consumer を実装する
status: done
priority: high
labels: [daemon, orchestration, architecture]
dependson: [207]
related: [185, 207, 209]
parent: 159
created_at: 2026-07-11T12:13:48.800921+00:00
updated_at: 2026-07-12T00:00:00.000000+09:00
---

## 背景

現状の queued prompt autostart は TUI の sync/idle tick が consumer であり、daemon は PTY を保持しても新規 session の start request を消費しない。#207 は TUI が戻って daemon 所有 Agent へ再 attach した後の `auto` prompt 滞留を解消するが、TUI が戻らない限り新規 worker は開始しない。

#207 で TUI から daemon terminal への prompt input には request id と PTY write 後の ACK が入る。ただし request id と配送中の ownership はプロセス内だけで、ACK 応答喪失後の再送 dedupe、daemon 単独での queue claim / spawn、再起動後の lease 回収はまだ持たない。

## 方針

- start request を `queued → claimed(lease) → spawned(terminal id) → input-acknowledged → running` として永続化する。
- enqueue 時の authoritative `SessionAgent`（CLI/model）または state generation を start request に固定し、claim 後に別 prompt が state を更新しても先行 request が後続の launch pair へすり替わらないようにする。state 更新と request publish の世代対応を検証する。
- daemon と TUI fallback が同じ claim/CAS を使い、単一 consumer・重複 spawn 防止を保証する。
- daemon が session の CLI/model/env/wiring と agent 同時実行上限を解決して Agent terminal を spawn する。
- #207 の request id / ACK を durable claim id に結び付け、PTY への初期 prompt write が完了してから claim を commit する。応答喪失時の再送でも、同じ request id の spawn / input を冪等に扱う。
- phase 開始前の exit、daemon/TUI crash、lease timeout、retry/backoff/dead-letter を扱う。
- daemon が作成した terminal metadata を TUI が発見して再 attach できるようにする。

## 受け入れ条件

- TUI 不在で delegate/queue された新規 sub session が daemon 所有 Agent として開始する。
- daemon/TUI/再起動の競合でも同一 request は一度だけ spawn する。
- CLI/model を異なる A/B request が claim 前後で競合しても、それぞれが記録した launch pair/generation を取り違えない。
- daemon 側 PTY write の成功を ACK してから claim を完了し、early exit・input failure・ACK 応答喪失で start intent を silent drop しない。
- 同じ request id の再送で terminal または初期 prompt を重複投入しない。
- concurrency 上限到達時は queue を消費せず、空き枠で再開する。
