---
number: 520
title: fix(tui): pane launch workerからlive terminal stream portを分離する
status: todo
priority: high
labels: [review, v2, tui, terminal, agent, concurrency, resilience]
dependson: []
related: [463, 489, 506, 508, 521, 523]
created_at: 2026-07-22T11:38:16.425358+00:00
updated_at: 2026-07-22T11:44:45.691094+00:00
---

## 問題・影響

shipping `WorkspaceUi` は Agent/generic Terminal pane launch をbackground workerへ移す際、launch commandだけでなく全live `TerminalSession`が共有する `AgentCommandPort` を `Option::take` してworkerへ渡す。slow/hung request中はportが `None` になり、既存paneのpoll/resizeが停止し、focused入力は `terminal is busy; keystroke not delivered` で失われる。worker panic/channel lossではportが永久に戻らない。

## 既存 issueとの境界

#489 はsession command admissionの同型bugを修正済みだがpane launch/terminal streamは対象外。#506 のactive writerはasync restore・saved Agent tab intent・foreground-only attachを所有するため変更しない。本issueはlaunch command ownershipと常駐stream ownershipの分離だけを扱う。connection epochは#523、request deadlineとhung pendingの有界終了は#521の別責務とする。

## 対象責務

- `PaneLaunchCommandPort` と常駐するterminal stream port/handleを分離し、launch workerが既存subscription、poll、input、resize、detachを奪わない構成にする。
- Agent launch/resume/generic terminal launchのslow/hung/panic時も既存pane IOとTUI input/quitを継続する。hung request自体のtimeoutは#521を利用し、本issueで独自deadlineを実装しない。
- launch admissionはbounded queueまたはtyped Busy completionで全pending operationをexactly once完了させる。
- completionはoperation/interaction fenceを維持し、late/panic/channel-closeで別pending paneへport/resultを返さない。
- #506 のrestore job/intent store、#508 のgeneration routing、daemon launch idempotencyは再実装しない。

## 受入条件

- [ ] launchが停止中でも既存Agent/Terminal paneのpoll・input・resize・detachが成功し、busyによるkeystroke lossがない。
- [ ] hung worker中もstream portは生存する。worker panic/channel closeはpending paneをsafe failureへ収束させ、hung pendingの有界timeoutは#521のcontractへ委譲する。
- [ ] concurrent pane launchはbounded admissionと1 request : 1 completionを保つ。
- [ ] Agent/generic、root/session、foreground/backgroundで同じownership規則になる。
- [ ] #506 branchとのrebase後もrestore専用portとlive streamを二重所有しない。

## 必須回帰テスト

barrierでlaunch workerを停止し、2つの既存paneにpoll/input/resize/detachを実行する。worker panic、completion receiver drop、launch queue overflow、workspace exit、late completionをdeterministic fakeで検証し、各operation/completion数とstream callをassertする。

## docs

`document/03-tui.md` のpane launch非同期処理を、command workerと常駐streamの分離契約へ更新する。
