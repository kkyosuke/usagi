---
number: 192
title: fix(orchestration): queued prompt autostart に dispatch 予約を導入する
status: done
priority: high
labels: [fix, orchestration, tui, review]
dependson: []
related: [185]
created_at: 2026-07-11T01:30:33.641601+00:00
updated_at: 2026-07-11T06:12:19.956117+00:00
---

## 背景

`autostart_queued_prompt_limit` は monitor snapshot の `running` / `waiting` だけを占有枠として数える。ところが agent pane の spawn 直後は古い phase を消去し、次の phase hook を watcher が観測するまで占有情報が存在しない。

home 起動時の autostart とイベントループの次の pass、または 110ms の UI tick と 200ms の watcher tick の間で、別の queued session が追加起動される。このため設定値が 1 でも、phase 観測前に複数 batch が起動できる。

既存の `#182 Limit queued prompt autostart concurrency` は done だが、tick 間の予約を扱っておらず受け入れ条件を満たしていない。なお issue store には別内容の `number: 182` も存在するため、本 issue ではタイトルで区別する。

## 対象

- `src/presentation/tui/home/mod.rs` の `autostart_queued_prompts`
- `src/presentation/tui/home/terminal/pool.rs` の slot 集計・phase watcher
- 起動直後と Attached 中の autostart 呼び出し

## 方針

- queued prompt を dispatch した瞬間に worktree 単位の in-flight reservation を作る。
- reservation は phase / pane liveness の権威状態へ引き継ぐまで占有枠として数える。
- spawn failure、pane exit、timeout、phase 非対応CLIでも予約が永久残留しない解放規則を定義する。
- 同じ worktree・同じqueue generationを二重dispatchしない。

## 受け入れ条件

- limit=1でqueued sessionが複数あっても、phase観測前に2件目を起動しない。
- home起動時passと最初のevent-loop passを連続実行しても上限を超えない。
- phase報告が遅いCLI・報告しないCLIでも、定義したfallback規則で安全に進行する。
- 上限到達中のpromptは消費せず、枠解放後に一度だけ起動する。

## テスト

- fake clock / delayed phaseで複数tickを進める統合テスト。
- startup pass直後のevent-loop passの回帰テスト。
- spawn failure、phaseなし、pane exit時のreservation解放テスト。
