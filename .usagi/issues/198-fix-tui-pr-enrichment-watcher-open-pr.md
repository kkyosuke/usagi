---
number: 198
title: fix(tui): PR enrichment を watcher から分離して open PR を再照会する
status: done
priority: medium
labels: [fix, tui, pr, orchestration, review]
dependson: []
related: [128, 173, 175]
created_at: 2026-07-11T01:30:36.975434+00:00
updated_at: 2026-07-11T07:00:15.425986+00:00
---

## 症状

terminal outputから新しいPR URLを検出すると、単一のterminal-pool watcherが `gh pr view` を同期実行する。1 PRあたりtimeoutは10秒で、複数PRは直列である。その間、同じwatcherが担うlive prompt配送、次回phase/bell観測、notification、resource sampleが止まる。

またlookupは「harvestしたPR URL集合が変化した時」だけ実行される。初回にOPENを取得した後、または初回lookupが失敗した後は、同じPR URLしか出力されなければ再照会されない。そのためOPEN→MERGEDがTUIへ反映されず、auto reclaimも起動しない。

## 方針

- URL harvestとPR enrichmentを分離し、`gh` subprocessは専用bounded workerで実行する。
- watcherはjob enqueueとresult mailbox適用だけを行い、phase/live prompt処理を待たせない。
- auto-managed OPEN PRとfailed lookupに `last_checked / next_retry / attempts` を持たせる。
- exponential backoffと最大同時lookup数を設ける。
- dismissed/pinned/mergedは現行どおり不要な再照会を避ける。
- TUIにrefresh中・最後のlookup errorを過度に騒がしくない形で表示する。

## 受け入れ条件

- 10秒停止するfake runner中もphase badgeとlive prompt配送が進む。
- 新しいterminal outputがなくてもOPEN→MERGEDを期限内に反映する。
- 初回failure後にbackoff付きでretryする。
- 同じPRのlookupを複数pane/TUI tickから重複起動しない。
- auto reclaimが更新済みMERGED状態を利用できる。

## テスト

- fake clock + delayed/failing runner。
- URL集合不変のOPEN→MERGED。
- 複数PRのconcurrency cap/dedupe。
- watcher progressとresult applyの統合テスト。
