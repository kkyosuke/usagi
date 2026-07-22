---
number: 523
title: fix(tui): shared terminal connection epochで全pane subscriptionを再確立する
status: todo
priority: high
labels: [review, v2, tui, terminal, ipc, reconnect, safety]
dependson: []
related: [216, 463, 508, 517, 519]
created_at: 2026-07-22T11:41:36.789232+00:00
updated_at: 2026-07-22T11:45:23.051060+00:00
---

## 問題・影響

shipping TUIは全 `TerminalSession` が1本のpersistent `IpcClient` を共有する。1 paneのrequestがprotocol/transport errorになるとadapterは種類を問わずsocketをdropし、daemonはそのconnectionの全attachmentを解除する。しかし別paneは新socket上のattachment不要な `Resume` が成功するため `Live` とold subscriptionを保持し、次inputが `NotAttached` でeffect-zero rejectionされ最初のkeystrokeを失う。そのerrorが新socketを再びdropし、他paneのfresh subscriptionまで壊すcascadeになる。

resize errorもsessionをLiveのまま残すため同じ経路を作る。same connectionのResyncで同じsubscriptionを再attachした場合、無条件にinput_seqを0へ戻すとdaemon ledgerとのgapになる。new attach後にold subscriptionをdetachすると、epochを区別しない現adapterはnew socketをdropし得る。

## 既存 issueとの境界

#463 は単一TerminalSessionのreconnectを実装済みだがshared production connectionをモデル化していない。#508 はplanned rollover時のgeneration owner routing/cacheを所有し、本issueは現行same-generation connection内のcross-pane epoch invalidationだけを扱う。#517のACK意味、#521のdeadline、#520のworker ownership、poll schedulerは別責務。

## 対象責務

- production terminal transportにlocal connection epochを持ち、subscriptionを `{wire id, epoch}` としてsessionへ関連付ける。
- fully received protocol errorとtransport破断を区別し、pane-local Resync/Staleがhealthy shared socketを不要にdropしない。
- socket replacement時は全old-epoch subscriptionをinvalidにし、各paneが `Resume/Input` を送る前にfresh Attachする。
- old-epoch detachはlocal no-opとし、new attachを破壊しない。
- same epoch/same subscriptionのresyncはinput_seqを保持し、new epoch/subscriptionだけresetする。
- Agent/generic Terminalの共有ownerで同じ規則を使い、replacement spawnは行わない。

## 受入条件

- [ ] 2つ以上のpaneで1 paneのResync/protocol errorがpeer subscriptionを無効化せず、transport EOF時は全paneがfresh attachしてからResume/Inputする。
- [ ] recovery後の最初のkeyが一度だけwriteされ、NotAttached/cascade socket dropがない。
- [ ] Attach(new)後のDetach(old)がnew connection/subscriptionを変更しない。
- [ ] same-socket resync後の次input sequenceが継続し、new connectionではfresh client ledgerと整合してresetする。
- [ ] resize/list/inventory/detach起因のtransport replacementも同じepoch invalidationを通る。
- [ ] #508のgeneration別routingがこのepoch primitiveを再利用できる。

## 必須回帰テスト

Agent+genericの2〜3 refsをC1でattachし、pane Aのprotocol Resync、resize EOF、partial response EOFを順に発生させる。各epochで各refのAttachがResume/Inputに先行すること、Bのfirst input write count 1、Aのfresh subscription保持、old detach no-op、same-sub seq継続をproduction adapter seamとWorkspaceUi multi-pane testで固定する。

## docs

`document/03-tui.md` と `document/04-ipc.md` にconnection epoch、subscription invalidation、reattach orderingを記載する。
