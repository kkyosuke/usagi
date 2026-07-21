---
number: 508
title: fix(tui/ipc): draining generation の inventory と TerminalRef owner routing を接続する
status: todo
priority: high
labels: [review, v2, tui, daemon, ipc, terminal, recovery]
dependson: [507]
related: [386, 388, 463, 492, 506]
parent: 505
created_at: 2026-07-21T21:20:50.849656+00:00
updated_at: 2026-07-21T21:31:36.212861+00:00
---

## 問題・影響

#507 で旧 daemon を draining として残しても、client が常に current active endpoint だけへ接続するなら、旧 generation の `TerminalRef` を inventory / attach / input / resync できない。現在の [TUI runtime](../../src/runtime/tui.rs) は current daemon endpoint を解決する persistent client が中心で、`TerminalRef.daemon_generation` から owner endpoint を選ぶ production route がない。

その結果、planned restart 後の control plane handoff に成功しても、TUI close / reopen や reconnect で旧 Claude / Codex tab が消えるか、new active へ誤配送される。

## 対象責務

trusted generation registry / locator を正本に、client / IPC の routing を分離する。

| request | routing |
|---|---|
| workspace / session / issue / new Agent launch 等の control operation | current active generation |
| scope inventory | active と trusted draining generation の inventory を取得し、完全な `TerminalRef` で merge / dedup |
| attach / resume-stream / resync / input / resize / detach / terminal kill | request の完全な `TerminalRef.daemon_generation` が示す owner endpoint |
| stale / unknown generation | typed stale / unavailable。current endpoint や同名 terminal へ fallback しない |

generation endpoint の解決は daemon が書く trusted record だけを使い、IPC client が任意 socket path を指定できないようにする。endpoint 再接続、credential / protocol negotiation、output cursor は generation ごとに独立して保持し、active locator の変更で draining subscription を破棄しない。

TUI restore は merged live inventory から旧 owner の exact ref を復元し、#506 の saved intent が利用可能なら順序・選択・dismissal も reconcile する。#506 未導入の inventory-only client でも owner routing は機能する。選択 tab だけを owner endpoint へ attach し、background tab は detached のまま保持する。

draining endpoint の timeout / transport failure は authoritative absence ではない。last-known tab intent を `reconnecting` / `owner unavailable` として保持し、verified generation retirement または owner からの authoritative non-live / exit だけで tab と endpoint record を回収する。partial inventory response を cold-restart interrupted projection へ変換しない。

## 受入条件

- [ ] planned restart 後の inventory が active / draining 両 generation の scope 内 live runtime を列挙し、exact ref で一度だけ tab へ投影する。
- [ ] old-generation tab の attach / resync / input / resize / detach / exit は owner endpoint、新規 control / launch は active endpoint へ配送される。
- [ ] active locator の切替・TUI close / reopen・transport reconnect をまたいでも old subscription / output cursor が正しく再確立され、入力を重複送信しない。
- [ ] forged endpoint、unknown / retired generation、scope mismatch、stale credential / ref、partial inventory failure を fail-closed に扱い、別 terminal へ fallback しない。
- [ ] draining endpoint の一時不通 / partial inventory は last-known tab を reconnecting として保持し、verified retirement / authoritative non-live まで削除・interrupted 化しない。
- [ ] draining generation の最後の terminal exit が active projection へ一度だけ反映され、tab と generation endpoint が安全に回収される。

## 必須 product E2E

shipping binary で Claude / Codex fixture を同時起動し、両方の `TerminalRef`、Agent PID、spawn count を記録して実際の `usagi daemon restart` を呼ぶ。

- reopen 後に両 tab が original owner-generation ref のまま一度だけ復元され、retained output と distinct input echo を確認できる。
- Agent PID / spawn count が不変で、provider resume option が呼ばれない。
- restart 後に起動した 3 個目の Agent は new active generation に属する。
- wrong-generation request、片方の endpoint failure、old terminal exit、最後の old terminal exit を順に発生させ、routing / partial failure / collection を確認する。

unit / integration fixture では merged inventory の deterministic order / dedup、generation 別 connection cache、credential refresh、cursor gap / resync、late frame / exit を網羅する。

## docs / migration

[IPC](../../document/04-ipc.md)、[daemon](../../document/05-daemon.md)、[TUI](../../document/03-tui.md) の endpoint discovery と terminal request routing を更新する。旧 client が multi-generation registry を理解しない場合は old-generation terminal を誤配送せず typed unavailable に縮退する。
