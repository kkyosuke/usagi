---
number: 213
title: feat(ipc): v2 daemon-authoritative IPC／ID を段階実装する
status: todo
priority: high
labels: [epic, ipc, daemon]
dependson: [212]
related: [159, 209]
created_at: 2026-07-12T11:38:03.490662+00:00
updated_at: 2026-07-12T11:38:03.490662+00:00
---

## 目的

[v2 IPC／ID proposal](../../document/proposals/02-ipc-id.md) を、core の型・純粋ロジックから secure transport、daemon の terminal／session authority、client cutover まで段階実装する Epic。

v1 の `TerminalPool` と `DaemonIpcServer` を丸ごと移植せず、`usagi-core` の domain／共有 reducer／wire 型、daemon の usecase／PTY adapter／server presentation、TUI・CLI・MCP の client adapter に責務分解する。

## MVP の子 issue

1. typed ID と fencing invariant
2. IPC envelope／handshake／error／bounded codec
3. Unix transport／peer・workspace 検証／backpressure
4. SessionLifecycle reducer と durable operation persistence
5. terminal registry／PTY API
6. session/control／prompt／autostart API
7. generation rollover と orphan safety（既存 #209 を v2 向けに更新）
8. TUI／CLI／MCP cutover と socket／PTY E2E

完全な daemon crash 継続は MVP に含めず、PTY broker／FD handoff の将来調査へ分離する。

## 完了条件

- managed session／terminal の実行権威が daemon に一本化され、daemon 不在時に local PTY へ暗黙 fallback しない。
- TUI/client 切断後も daemon 所有 PTY が継続し、再接続は revision／cursor から resume または明示 resync する。
- remove→同名再作成、late worker、複数 Agent pane、daemon rollover、ACK loss で誤配送・二重 spawn・所有権消失が起きない。
- pure／fake IO／Unix socket／PTY process E2E の各 gate が子 issue ごとに追加される。
- 実装済み契約を正本へ畳み込み、本 proposal をリンク stub 化する。
