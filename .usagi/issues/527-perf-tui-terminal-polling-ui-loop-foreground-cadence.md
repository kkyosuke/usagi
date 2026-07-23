---
number: 527
title: perf(tui): terminal pollingをUI loopから分離しforeground cadenceに制限する
status: todo
priority: medium
labels: [review, v2, tui, terminal, ipc, performance, scheduler]
dependson: [506, 521, 523, 525]
related: [197, 216, 344, 385, 388, 508, 521, 523]
created_at: 2026-07-22T11:44:32.841588+00:00
updated_at: 2026-07-23T00:09:07.139578+00:00
---

## 問題・影響

#506以前のshipping controllerは16ms tick/redrawごとに `WorkspaceUi.terminals` の全entryへ同期 `Resume` RPCを直列実行し、N paneで最大約62.5×N request/秒となっていた。#506は選択外/background terminalをdetachし、同期poll対象をselected foregroundの高々1本へ縮小したため、このN倍増幅は解消済みである。ただし、その1本の `Resume` は依然UI loop上でframeごとに同期実行される。foreground requestが遅延/hangするとrender、input、modal、quitが `read_key` 前で停止し、idle時にも最大約62.5 request/秒のdaemon loadを生む。

## 既存issueとの境界・前提

#344は同期client制約の下でloop tickごとのpollを導入し、#385はbackground exit検知のため全terminal sweepを接続した。#506がbackground detachを導入してall-pane sweepを除去した後も残るforeground同期loopを、本件はsteady-state corrective optimizationとして扱い、双方向IO/exit orderingを維持する。#197はdaemon内部の旧idle polling/session store rereadを削減するissueで、v2 TUI clientのforeground同期loopは対象外。

本issueは#506の全background tab detached intent、#521の実効request deadline/reconnect budget、#523のshared connection epoch/subscription再確立、#525のTUI不在/非attached terminalへ到達可能なfinal tombstone projectionを前提として消費し、これらを重複実装しない。#508のgeneration routingも尊重する。restore persistence/reconciliationは変更しない。

## exact observation primitive

- foregroundで明示的にattachedなterminalだけをinteractive `TerminalAction::Resume` polling対象にする。
- detached backgroundの観測primitiveはscope単位の `TerminalAction::Inventory` に固定する。#525がこのinventory projectionへexit/final-available tombstone metadataと明示reopen用locatorを提供する。
- background inventoryはexit/final-available **metadataだけ**をbounded cadenceで観測する。final replay bytesは取得せず、background terminalへ `Attach` / terminal-specific `Resume`を発行しない。
- final output bytesはtabをforeground化するか利用者が明示reopenした時だけ、#525が定義するfinal-replay取得primitiveで読む。その取得latencyはbackground bounded-exit保証に含めない。
- #525未完了時はbackground final-output/exit到達性を保証せず、旧subscriptionのResumeやfallback Attachで代替しない。本issueは#525依存のためreadyにならない。

## 対象責務

- foreground terminal output observationとbackground scope inventory observationをUI threadから分離し、per-ref/per-scopeで高々1件のin-flight jobを持つasync/coalesced schedulerへ移す。
- completionをfull scope/`TerminalRef` + #523 connection epoch + requested cursor/inventory revisionでfenceし、late/out-of-order resultをnew focus/ref/cursorへ適用しない。
- input/control/quit laneをpoll queueから分離する。owner unavailable、socket hang、#521 deadline/reconnect budget exhaustionを含むpoll失敗はworker結果として完了し、draw/input/modal/quitをblockしない。
- owner epochがavailableでrequestが#521 deadline内に応答する条件では、foreground outputをinteractive cadence、background exit/final-available metadataをinventory cadence + bounded queue delay + 1 request deadline以内に観測する。
- unavailable/hung中はUIを止めずbackoffし、available epoch確立時からmetadata観測上限を再適用する。
- foregroundのoutput burst/gap/resyncと、明示reopen時の#525 final replay→exit表示順を保持する。
- terminal数、pending jobs、per-frame completion drain、notification queueをboundedにし、cadence/drop/coalesce/deadline exhaustionをmetrics化する。

## 受入条件

- [ ] 1/10/100 idle paneでrequest rateとUI wakeupがpane数×60Hzに増えず、foreground latencyは目標内に保たれる。
- [ ] slow/hung/unavailable background inventory ownerとdeadline exhaustionがdraw/input/modal/quitをblockせず、focused laneを継続する。
- [ ] foreground per-ref Resumeとbackground per-scope Inventoryは各1件以下で、focus switch/#523 epoch change/resync後のlate resultを誤適用しない。
- [ ] detached background terminalへのAttach/Resume call countは0で、#525 scope Inventoryだけがexit/final-available metadataをavailable-owner時の公開上限内に一度だけ観測する。
- [ ] background中のfinal bytes取得latencyは保証せず、foreground化/明示reopen時だけ#525 primitiveで取得し、final replay→exit表示順を守る。
- [ ] #525未完了時にbounded final-output保証を主張せず、fallback attachせず、本issueはdependency未充足のままになる。
- [ ] unavailable期間はnon-blocking backoffし、available epoch確立後にbounded metadata観測を再開する。
- [ ] foreground化時だけ#506 intentに従ってattachし、#508 owner routing/partial failureを尊重する。

## 必須回帰テスト・計測

fake clock/barrierで1/10/100 refsと複数scope、slow/hung/deadline exhaustion/unavailable→available Inventory、late/out-of-order response、focus連打、cursor gap、#523 epoch change、#525 background tombstoneを検証する。detached backgroundのAttach/Resume call count 0、scope Inventory rate/in-flight count、frame/quit wall-clock、available化からexit metadata観測までの上限をassertする。foreground化/明示reopen時だけfinal replay fetchが1回走ることと表示順を実Unix socket E2Eでも確認する。

## docs

`document/03-tui.md` のredrawごと全poll記述をforeground Resume / background #525 scope Inventory schedulerへ更新し、backgroundのbounded保証がexit metadataだけであることを明記する。`document/04-ipc.md` のdeadline/connection epoch/backpressure契約から参照する。
