---
number: 621
title: refactor(ipc): capability 語彙と negotiation policy を SSoT 化する
status: todo
priority: medium
labels: [refactor, ipc, core, daemon, architecture, ssot, review]
dependson: []
related: [622]
created_at: 2026-08-02T22:56:10.970379+00:00
updated_at: 2026-08-02T22:56:46.640173+00:00
---

## 問題 / 責務違反

IPC capability の wire 名と接続 policy が、同じ protocol contract でありながら複数層へ分散している。

- `crates/core/src/usecase/client.rs::IpcClient::connect_with` は `request.correlation.v1` / `pr.snapshot.v1` / `build.artifact.v1` / `daemon.owner-identity.v1` を required capability として文字列で直書きする。
- `crates/daemon/src/presentation/ipc.rs::server_protocol` は同じ名前を server advertisement の別リストへ直書きする。
- `crates/daemon/src/usecase/authority/standby.rs::BUILD_ARTIFACT_CAPABILITY` は `build.artifact.v1` をさらに独自定数として持つ。
- `usagi_core::infrastructure::ipc` に定数があるのは terminal checkpoint / input operation / workspace fence / owner-generation routing だけで、同じ capability vocabulary に二つの管理方式が混在する。

`document/02-architecture.md#依存ルール` は IPC protocol vocabulary を `usagi-core` に置くと定め、`document/06-conventions.md#ドキュメント規約` は同じ mapping / 判定規則を一つの正本へ集約することを要求する。現在は client requirement、server advertisement、handoff verification がコンパイル時に結び付かない。

## 発生条件 / 影響

片側だけで capability の rename、typo、追加・削除を行うと発生する。

- client required 名だけが変わる、または server advertisement から必要名が落ちると、`negotiate` が全接続を拒否し、TUI / CLI / MCP が daemon を利用できない。
- safety capability を server が advertise しても client policy が required に追加し忘れると、接続は成立する一方で client が安全要件を保証したことにならない。
- `BUILD_ARTIFACT_CAPABILITY` の独自定数と client/server literal が drift すると、通常接続と standby readiness が同じ build identity 能力を別名で判定する。

履歴上、owner identity、build artifact、workspace fence、owner-generation routing は別 PR で順次追加されており、現在の分散は機能追加のたびに片側更新を要求する。

## 修正方針

- `usagi_core::infrastructure::ipc` に capability の closed vocabulary（typed enum または一元的な constants/descriptors）を置く。
- 各 descriptor に wire 名と用途を一度だけ定義し、client required policy、server advertised policy、standby/handoff verification はそこから導出する。
- connection context により条件付きとなる owner identity / workspace fence と、client が advertise する owner-generation routing は policy 関数で明示し、単一の巨大な無条件リストにはしない。
- protocol の外部 wire 名と negotiation behavior は維持する。

## 必要な回帰テスト

- 全 production client-required capability が production server advertisement に含まれることを table-driven に検証する。
- expected-owner / workspace-bound / unbound の各 context で required policy の差分を固定する。
- active / standby の advertised policy 差分（generation handoff は standby のみ）を固定する。
- build artifact capability が通常 handshake と standby readiness で同じ descriptor を参照することを固定する。
- unknown required capability は従来どおり effect 前に拒否されることを維持する。
- 静的検索または architecture test で production code の capability wire literal が正本外に増えないことを検知する。

## 受入条件

- production code に同じ capability wire 名の client/server/handoff 重複定義がない。
- capability の追加・rename が正本一か所の変更と policy test の更新で完結する。
- 既存の handshake、owner binding、workspace fence、rollover readiness の挙動が維持される。
