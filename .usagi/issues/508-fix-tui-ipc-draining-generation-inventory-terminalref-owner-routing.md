---
number: 508
title: fix(tui/ipc): draining generation の inventory と TerminalRef owner routing を接続する
status: todo
priority: high
labels: [review, v2, tui, daemon, ipc, terminal, recovery]
dependson: [518]
related: [386, 388, 463, 492, 506, 507, 516]
parent: 505
created_at: 2026-07-21T21:20:50.849656+00:00
updated_at: 2026-07-22T12:06:33.952561+00:00
---

## 問題・影響

current client は常に current active endpoint を中心に接続し、`TerminalRef.daemon_generation` から owner endpoint を選ぶ production route を持たない。この状態で #507 が旧 daemon を draining として残す shipping rollover を先に有効化すると、旧 generation の `TerminalRef` を inventory / attach / input / resync できない。

その結果、old PTY process は生存していても TUI close / reopen や reconnect 後に到達不能となる。resource / lease / outbox が残って old generation を collectできず、generation上限や後続restartを永続的に圧迫し得る。new activeへ誤配送した場合は別terminalへのeffectにもなる。

intermediate mainを安全に保つため、本issueを#507の後続ではなくshipping enableのprerequisiteへ前倒しする。#518のregistry projection / owner shardをfixtureから読み、shipping restart commandを有効化する前にmulti-generation inventory / routing capabilityを完成させる。

## 対象責務

trusted generation registry / locator を正本に、client / IPC の routing を分離する。#516 / #518 のactive・draining fixture上で独立実装し、clientは`owner-generation-routing.v1` capabilityをadvertiseする。#507は全handoff participantがこのcapabilityとcompatible registry revisionを満たすまでshipping rolloverを開始しない。

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

- [ ] #507 の shipping rollover path / capability は本issue完了までdisabledである。routing capability無し・旧client・revision mismatchではauthority handoff前にtyped refusalとなり、old active/currentと全PTYを維持する。
- [ ] planned restart 後の inventory が active / draining 両 generation の scope 内 live runtime を列挙し、exact ref で一度だけ tab へ投影する。
- [ ] old-generation tab の attach / resync / input / resize / detach / exit は owner endpoint、新規 control / launch は active endpoint へ配送される。
- [ ] active locator の切替・TUI close / reopen・transport reconnect をまたいでも old subscription / output cursor が正しく再確立され、入力を重複送信しない。
- [ ] forged endpoint、unknown / retired generation、scope mismatch、stale credential / ref、partial inventory failure を fail-closed に扱い、別 terminal へ fallback しない。
- [ ] draining endpoint の一時不通 / partial inventory は last-known tab を reconnecting として保持し、verified retirement / authoritative non-live まで削除・interrupted 化しない。
- [ ] draining generation の最後の terminal exit が active projection へ一度だけ反映され、tab と generation endpoint が安全に回収される。

## 必須 routing E2E

shipping restartをまだ有効化せず、#516 / #518のintegration fixtureでactive / draining 2 daemon、別Unix socket、owner shard、Claude / Codex実PTYを構成する。

- current locatorがnew activeを指す状態でscope inventoryをmergeし、old tabをoriginal owner-generation refのまま一度だけ復元する。
- old tabのattach / resync / distinct inputはdraining endpoint、新規launchはactive endpointへ配送し、Agent PID / spawn countを変えない。
- client close / reopen、active locator切替、draining transport failure、wrong-generation request、old terminal exit、最後のold exitを順に発生させ、cursor / partial failure / collectionを確認する。
- routing capability無し・旧client・registry revision mismatchではhandoff enable判定がtyped refusalとなり、old active/currentとPTYへeffectを与えない。

unit / integration fixtureではmerged inventoryのdeterministic order / dedup、generation別connection cache、credential refresh、cursor gap / resync、late frame / exitを網羅する。実shipping `usagi daemon restart` を使うend-to-endとprovider resume argv未実行の証明は、全prerequisite完了後に#507が所有する。

## docs / migration

[IPC](../../document/04-ipc.md)、[daemon](../../document/05-daemon.md)、[TUI](../../document/03-tui.md) の endpoint discovery と terminal request routing を更新する。旧 client が multi-generation registry を理解しない場合は old-generation terminal を誤配送せず typed unavailable に縮退する。#507のshipping rolloverは本issueのrouting capability確認までdisabledとし、同じmain revisionで到達不能なdraining resourceを生成しない。
