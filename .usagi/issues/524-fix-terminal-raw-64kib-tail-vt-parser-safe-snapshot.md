---
number: 524
title: fix(terminal): raw 64KiB tailをVT parser-safe snapshotへ置き換える
status: todo
priority: high
labels: [review, v2, daemon, tui, terminal, vt, replay, correctness, p1]
dependson: []
related: [199, 251, 265, 472, 473]
created_at: 2026-07-22T11:41:58.106442+00:00
updated_at: 2026-07-22T12:08:12.322807+00:00
---

## P1 correctness classification

PTY process自体は生存し続ける一方、trim後のattach/resyncで利用者が見るscreen、cursor、style、copy対象historyを破壊し得るためP1 correctnessとする。availabilityではなくauthoritative terminal state corruptionである。

## 問題・影響

#472 はterminal replayを最大64KiBのraw byte tailへbounded化した。shipping TUIの `TerminalSession::replace` はresync/attach時にblank `TerminalScreen` を作り、そのtailを先頭からVT parserへ流す。しかし任意byte境界のtailはUTF-8/CSI/OSC sequence途中から始まり、過去に設定されたcursor、SGR、scroll region、alternate buffer、消去/折返し状態も含まない。raw tail単独は現在screen stateを再構成できず、trim後のattach/reconnectで文字化け・escape漏れ・cursor/画面/copy history破損を起こす。

## 既存issueとの境界

#199 はdaemonをterminal grid/scrollbackの唯一の権威とし、clientへviewport snapshot・cursor・attrsを送り、backlog eviction/alt-screen後も正しく復帰する契約を実装済みとした。しかし現shipping v2はclientのblank parserへraw tailを渡す構成を再導入しており、本件は#199のshipping regressionである。done issueを再利用せずcorrective issueとしてrelatedにする。

#472 はend-to-end byte/frame boundを所有し、UI scrollbackとsemantic VT checkpointは明示的に対象外。#473 はexited PTY transport/FD回収とbounded final replayの分離。本issueはbounded windowを維持したまま#199の単一grid authority / VT semanticsを復元し、buffer上限やFD lifecycleを再実装しない。

## 対象責務

- attach/resync snapshotを「blank parserに任意raw tail」の契約から、versioned semantic screen checkpoint + checkpoint以後のcontiguous suffix、または同等にparser-safeで完全な再構築表現へ変更する。
- checkpointはprimary/alternate buffer、**primary saved buffer**、全rows/cells、cursor/saved cursor、wrap、scroll region、tab stops、SGR属性、decoder/UTF-8境界、必要なscrollbackを明示的かつboundedに表す。
- visible viewportだけでなく `cells_with_scrollback` とselection/copyが参照するprimary historyを保存し、alternate screenから戻った後のprimary bufferも同一に復元する。
- daemonとTUIでVT parser authorityを二重実装しない層/serialization責務を決める。raw PTY bytesとrendered screenを混同しない。
- legacy raw snapshotはcapability/revision negotiationでfail closedまたは安全な限定表示にし、途中escapeを文字として露出しない。
- hostileなrows/cols/cell count/scrollback length/attribute tableをdecode前にchecked arithmeticとaggregate allocation budgetで検証する。
- offset/cursor continuity、64KiB/frame/memory bounds、Agent/generic共有経路を維持し、snapshot restoreを理由にPTYをrespawnしない。

## 受入条件

- [ ] retention先頭がUTF-8、CSI/OSC、SGR、alternate-screen sequenceの途中でもreconnect前後のvisible cells/cursor/styleが一致する。
- [ ] primary/alternate/saved primary buffer、`cells_with_scrollback`、selection/copy historyがuntrimmed referenceと一致する。
- [ ] tail以前に開始したcursor movement、clear、scroll region、alternate bufferの状態がsnapshot後も保持される。
- [ ] resizeがcheckpoint直前、checkpoint生成とsuffixの間、restore直後にinterleaveしてもgeometry/revision fenceでold/new stateを混在させない。
- [ ] malformed/unknown snapshot revisionとhostile dimensions/countsはescape injection、panic、integer overflow、unbounded allocation、blank parser corruptionを起こさずtyped fail closedになる。
- [ ] checkpoint+suffixは既定IPC frameとper-terminal/aggregate cell/scrollback memory bound内に収まる。
- [ ] old/new client-daemon capability/revisionの全組合せがnegotiated semantic snapshot、明示legacy限定表示、typed incompatibleのいずれかへ決定的に収束する。
- [ ] Agent/generic、resize、resync、exit final snapshotで同一contractを使い、reattach前後でchild PIDとspawn countが不変である。

## 必須回帰テスト

1. 実daemon + 実PTY + fresh client/TUI E2Eで64KiB超のunique output、long-running SGR、alternate screen、cursor save/restore、primary scrollback/copy markerを生成する。client disconnect→reattach/resync後にchild PID/spawn count不変、visible cells/cursor/style、primary saved buffer、`cells_with_scrollback`/copy historyがbefore/referenceと一致することをassertする。
2. 64KiB超出力でUTF-8、CSI/OSC、SGR、alternate-screen、combining/CJK、malformed bytesの全split位置を生成し、untrimmed reference parserとcheckpoint+suffix restoreをproperty/fixture比較する。
3. old client/new client × old daemon/new daemon × capability present/absent × supported/unknown revisionのcompatibility matrixを固定し、途中escapeをlegacy raw parserへ渡さないことをassertする。
4. rows/cols 0・最大値、乗算overflow、巨大cell/attribute/scrollback count、aggregate budget超過、compression bomb相当payloadをfuzz/property testし、decode前のbounded allocationとtyped rejectionを測る。
5. resizeをcheckpoint直前、capture中、suffix適用前後へbarrierでinterleaveし、geometry/revision mismatchがsnapshot retryまたはtyped resyncになり、state混在しないことを検証する。
6. 実IPC frame size、per-terminal/aggregate allocation peak、Agent/generic共通fixture、exit final snapshotをassertする。

## docs / migration

`document/04-ipc.md` をsnapshot schema/capability/revision/geometry/offsetのSSoT、`document/03-tui.md` をvisible + primary/copy-history restore behaviorのSSoTとして更新する。wire revision、old/new compatibility matrix、hostile allocation limitを定義する。
