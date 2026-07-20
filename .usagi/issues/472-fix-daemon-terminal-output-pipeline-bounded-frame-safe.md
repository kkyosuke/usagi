---
number: 472
title: fix(daemon): terminal output pipeline を bounded かつ frame-safe にする
status: todo
priority: high
labels: [review, v2, daemon, terminal, ipc]
dependson: []
related: [216, 218, 248, 251, 264, 271]
parent: 453
created_at: 2026-07-20T12:06:23.708368+00:00
updated_at: 2026-07-20T12:06:23.708368+00:00
---

## 問題・影響

root/v2 の `crates/daemon/src/usecase/terminal.rs::Entry` は bounded journal と別に `replay: Vec<u8>` を無制限に伸ばし、snapshot で全 clone する。`src/runtime/daemon.rs::{AgentPty::new,DaemonPty::new}` も unbounded channel を使う。IPC の `DEFAULT_MAX_FRAME_BYTES` 1 MiB を超えると attach/resync snapshot 自体が送れず、同じ oversized resync を繰り返し、memory も上限なく増える。

## 成立条件 / 再現フロー

高速 PTY producer と遅い/no subscriber で 1 MiB 超の output を発生させ、attach/resume と process memory を観測する。journal が bounded でも replay/channel/frame のいずれかが無制限で end-to-end bound が成立しない。

## 対象責務と非対象

PTY reader→channel→registry journal/replay→IPC snapshot/chunk の全段 backpressure、retention、cursor/resync protocol を対象とする。UI scrollback 表示量、PTY map/FD cleanup は #473、restart snapshot hydrate は #459。

## 受入条件

- [ ] 全 queue/buffer に byte/item 上限と overflow policy があり、producer が無制限 allocation を起こさない。
- [ ] attach/resync reply は常に frame 上限内で、truncated window の base offset と `ResyncRequired` recovery が一意に定義される。
- [ ] slow subscriber が他 terminal/daemon を飢餓にせず、最終 output→exit ordering を維持する。
- [ ] metrics で dropped/coalesced/backpressured bytes を観測でき、secret/output を log しない。

## 必須回帰テスト

1 MiB 超の高速 producer、slow/absent subscriber、複数 terminal、attach/resync loop、exit 直前 output を実 IPC で流し、memory/frame bound、cursor continuity、最終 ordering を検証する。

## docs / 移行影響

`document/04-ipc.md` と `document/05-daemon.md` に retention window、offset、overflow/backpressure、resync 契約を記載する。wire schema を変える場合は version/互換 error を定義する。
