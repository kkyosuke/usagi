---
number: 200
title: fix(tui): background git sync の freshness と error を表示する
status: done
priority: medium
labels: [fix, tui, git, ux, review]
dependson: []
related: [36, 84]
created_at: 2026-07-11T01:30:38.509508+00:00
updated_at: 2026-07-11T07:30:23.738747+00:00
---

## 症状

home起動時とembedded pane離脱後のgit status syncはbackground threadで実行される。完了までsidebarはsaved stateの `dirty / local / pushed / synced` を通常の確定表示と同じ色で描き、同期中であることを示さない。

syncが失敗した場合はlast-known statusを残すだけで、ユーザーは「まだ同期中」「失敗してstale」「現在値」を区別できない。sidebarの `Nm ago` はsession activityでありgit sync freshnessではない。

`#84` はUI thread上のgit fan-outを解消したが、非同期化によって生じるfreshness feedbackは対象外である。

## 方針

- workspace root単位に `GitSyncState { generation, started_at, finished_at, status, error }` を持つ。
- dispatch時にSyncingへ遷移し、sidebar/status headerへspinnerまたはdimmed stale markerを表示する。
- failure時はlast-known valueを維持しつつstale/errorを表示し、再試行操作を提供する。
- generationを比較し、遅い旧sync結果が新しい結果を上書きしない。
- unite modeではworkspaceごとに独立状態を持つ。
- loading色は通常の情報取得としてaccent/info系を使い、削除のdanger色と混同しない。

## 受け入れ条件

- startup/detach sync中にstatusが確定値として見えない。
- successでfresh表示へ戻り、failureでstale/errorが残る。
- overlapping syncは最新generationだけを適用する。
- cursor、session順序、PR badgeをbackground refreshが巻き戻さない。
- non-git rootでも意味のないspinnerを表示しない。

## テスト

- delayed/failing syncのUI snapshot。
- old/new generationの逆順completion。
- unite複数rootの独立状態。
- retry、quit中completion、session create/remove同時実行。
