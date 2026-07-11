---
number: 194
title: fix(tui): pending agent spawn 失敗時に queued prompt を再queueする
status: todo
priority: high
labels: [fix, tui, orchestration, ux, review]
dependson: []
related: [98, 141, 148]
created_at: 2026-07-11T01:30:34.596597+00:00
updated_at: 2026-07-11T02:45:02Z
---

## 症状

queued promptがあるsessionでagent paneを起動し、env解決後のPTY/daemon spawnが失敗すると、promptがstoreから消える。UIはspawn成功前に「queued prompt delivered」と記録し、実際のspawn errorを `PendingPoll::Gone` に潰してloadingを消すだけである。

env resolver worker自体がpanicした場合は結果slotが埋まらず、pending tabが永久にResolvingのまま残り得る。

## 根本原因

- `start_pending_spawn` がpromptを先に `take` する。
- prompt本文とdelivery状態を `PendingSpawn` のtransactionとして保持していない。
- `poll_pending_spawn` が `add_pane_selected` のerrorを捨てる。
- worker completionが `Result` / panicを表現しない。

## 方針

- promptはspawn成功までpending launchが所有し、成功時だけdeliveryをcommitする。
- spawn failure、resolver failure/panic、ユーザーcancelでは元のqueueへ順序を保って戻す。
- `PendingPoll::Failed { error, retryable }` を導入し、on-screen errorとdaily logへ一度だけ記録する。
- 「delivered」ログはpane/agentへの引き渡し成功後にのみ出す。
- autostartとmanual launchで同じtransaction helperを使う。

## 受け入れ条件

- spawn failure後もpromptが次回launchで受け取れる。
- UIに対象sessionと失敗理由が表示され、loadingが静かに消えない。
- resolver panicで永久loadingにならない。
- cancelと同時appendがpromptを欠落・逆順化しない。

## テスト

- fake spawner failureとretry成功。
- resolver error/panic。
- pending中のconcurrent appendとcancel。
- 成功前にdelivered logが出ないこと。
