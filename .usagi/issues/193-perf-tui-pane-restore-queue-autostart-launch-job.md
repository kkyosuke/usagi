---
number: 193
title: perf(tui): pane restore と queue autostart を非同期 launch job 化する
status: todo
priority: high
labels: [perf, tui, orchestration, ux, review]
dependson: []
related: [98, 136, 148]
created_at: 2026-07-11T01:30:34.197755+00:00
updated_at: 2026-07-11T02:45:02Z
---

## 背景

自動pane復旧とqueued prompt autostartは、初回描画前またはhome event loop上で次の処理を同期実行する。

- workspace env解決（`op read`、1 binding最大30秒）
- resumable session探索とagent provision
- daemon connect / Spawn / Attach handshake（各最大10秒）
- paneごとのspawn

手動pane追加は `PendingSpawn` でenv解決をoff-thread化しloadingを表示するが、restore/autostartはこの経路を通らない。1Passwordがlocked、daemonがwedged、履歴探索が遅い場合に、初回画面が出ないかTUI入力が止まり、進捗表示もない。

## 対象

- `src/presentation/tui/home/mod.rs` の `restore_open_panes`
- `src/presentation/tui/home/mod.rs` の `autostart_queued_prompts`
- startupとhome/Attached event loopのlaunch dispatch
- env resolver / daemon handshakeとの境界

## 方針

- 初回frameをpaintしてからrestore/autostart jobをdispatchする。
- workspace env解決はworkspace root単位で共有し、UI thread外で行う。
- paneごとに `queued / resolving-env / connecting / spawning / ready / failed` を表現する。
- 自動launchも手動pending-tabと同じ結果mailbox・error sink・cancel規則を利用する。
- 同時実行数はautostart reservation issueの枠管理と統合する。

## UX

- session/tabにloading chipを表示し、処理種別を短く示す。
- timeout・spawn failureは対象sessionと再試行可否を表示する。
- 他sessionの操作、quit、session create/removeを処理中も継続できる。

## 受け入れ条件

- 30秒停止するfake env resolverでも、初回frame・キー入力・animationが進む。
- 複数paneのrestoreでdaemon handshake待ちがUI threadに直列累積しない。
- 失敗paneだけをfailedにし、残りのrestore/autostartは継続する。
- queueはlaunch成功が確定するまで失われない。

## テスト

- delayed/failing resolver・daemon backendを注入したevent-loopテスト。
- startup first-paint順序テスト。
- 複数workspace rootのenv dedupe、cancel、timeout、partial failureテスト。
