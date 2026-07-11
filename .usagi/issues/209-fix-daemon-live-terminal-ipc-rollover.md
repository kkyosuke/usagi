---
number: 209
title: fix(daemon): live terminal を保った IPC 世代 rollover を実装する
status: todo
priority: high
labels: [daemon, lifecycle, safety]
dependson: [208]
related: [205, 206, 207, 208]
parent: 159
created_at: 2026-07-11T12:13:49.117451+00:00
updated_at: 2026-07-11T12:24:02.000000+00:00
---

## 背景

`daemon::start` は PID 生存だけで旧 daemon を再利用する。新 TUI と pre-Hello/別 build daemon が不一致でも互換 daemon は起動せず、新規 autostart は local fallback、既存 owner は安全のため blocked になる。旧 daemon の stop は所有する生存 Agent を終了するため自動 restart できない。#207 の TUI 側 prompt 引き渡しと #208 の durable consumer / ACK を、新旧 daemon が併存する状況でも安全に使うには、terminal ownership を IPC 世代ごとに識別する必要がある。

## 方針

- executable identity ではなく明示 IPC protocol generation と capability を永続 record/handshake に持つ。
- 旧 generation が terminal を所有中は強制停止せず drain し、新規 terminal は互換 generation が所有する。
- open-pane terminal id に daemon generation/endpoint を結び付け、別世代で id を誤解釈・重複 spawn しない。
- start claim と input request は所有 generation へだけ送信し、別世代への誤配送・二重 ACK を防ぐ。
- `close_tab` / `close_active` / restore-disabled quit / remove など全 teardown 経路で `Killed` ACK を ownership 解放条件に統一し、ACK 不明の terminal id を snapshot/registry から先に失わない。pid reuse も generation と process identity で拒否する。
- terminal 0 の旧 daemon は自動停止し、使用中の旧世代は drain 完了後に冪等回収する。
- generation 数を有界化し、registry 不在/破損時は安全側へ escalation する。

## 受け入れ条件

- pre-Hello daemon と生存 terminal を維持したまま、新規 session は互換 daemon の durable terminal で開始する。
- 旧 pane は正しい世代へ再 attach するか、安全に blocked となり複製されない。
- terminal 0 の旧世代は自動回収される。
- crash/restart/registry corruption で既存 Agent を誤 kill しない。
- 明示 close の ACK timeout / disconnect 後も次回 fresh launch と重複せず、再接続後に kill 状態を確定または人へ escalation できる。
