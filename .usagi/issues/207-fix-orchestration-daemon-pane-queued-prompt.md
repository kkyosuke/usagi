---
number: 207
title: fix(orchestration): daemon 永続 pane の queued prompt 自動着手を復旧する
status: done
priority: high
labels: [bug, orchestration, daemon]
dependson: []
related: [136, 205, 206, 208, 209]
created_at: 2026-07-11T12:02:15.323503+00:00
updated_at: 2026-07-11T15:28:05.406226+00:00
---

## 症状

daemon が保持した Agent pane に TUI が再 attach した後、TUI 不在中に `session_prompt(auto)` が launch queue へ積んだ prompt が消費されず、sub session が自動着手しない。terminal-only pane がある場合も Agent autostart が誤って抑止される。

## 原因

- live marker は TUI PID に結び付くため、TUI 不在中の `session_prompt(auto)` は launch queue を選ぶ。
- 再 attach 後の autostart は live pane を一律 skip し、watcher は live queue しか drain しない。
- `has_live_pane` が Agent と terminal を区別しない。
- 同時実行上限の早期 return が既存 Agent への配送まで止める。

## 方針

- launch queue に「`auto` のフォールバックを既存 Agent へ引き渡せるか」を保存する。明示 `mode=queue` と旧形式は既存 Agent へ渡さず、次の fresh launch 用という従来契約を保つ。
- live Agent pane がある場合は、autostart が watcher へ冪等な配送要求を登録する。watcher は引き渡し可能な prompt だけを live queue より先に配送する。retry 待ちの prompt は順序を保ち、terminal 状態の dead-letter と fresh-only prompt は後続 live work を永久に塞がない。
- pane 無し fresh spawn の claim は launch/live origin と retry state を別々に保持する。live-only failure は live queue の metadata で bounded backoff/dead-letter し、古い launch dead-letter を継承しない。retry の先頭 batch 境界も保存し、backoff 中の append が旧 attempt 履歴を継承しない。既存 Agent が復帰した live delivery はこの spawn retry を迂回して回復できる。
- TUI の input handle への送信が失敗した場合は prompt と既存の retry 情報を launch queue へ復元し、上限付き backoff を継続する。daemon terminal への durable prompt input は request id 付き `Keys` と PTY write 後の `InputResult` ACK を使い、missing terminal・write failure・ACK timeout も呼び出し側へ返す。応答喪失後の再送を exactly-once にする durable claim / deduplication は #208 で扱う。
- terminal-only pane は新規 Agent autostart を妨げない。
- 同時実行上限は新規 spawn のみに適用し、既存 Agent への配送は枠を使わない。
- daemon autospawn は register/socket ready まで待ち、unattended launch は daemon 不通時に local PTY へ縮退しない。
- attach は `Missing` と `Adopted` / transport failure を型で分け、前者だけ fresh fallback する。daemon registry に生存 terminal が残る worktree は bounded retry とし、snapshot に無い孤児 Agent を複製しない。
- background spawn 後の terminal id を open-pane snapshot へ即時保存し、人が一度も attach しなくても次回 TUI が同じ daemon pane を発見する。
- 全 CLI の interactive command 終了を `exited` phase として記録し、consumer marker と配送直前の双方で終了済み Agent を除外する。worktree に複数 Agent pane があり終了元を特定できない場合は自動 kill しない。

## 受け入れ条件

- TUI 不在中に `auto` が選んだ queue が、daemon Agent への再 attach 後、監視上 `running` / `waiting` でない tick に自動配送される。phase を報告しない Agent は通常の live prompt と同じ best-effort 配送になる。
- 明示 `mode=queue` の prompt は既存 Agent へ配送されず、指定した CLI / model とともに次の fresh launch まで残る。保存済みの起動設定は、`auto` prompt を既存 Agent へ渡した場合も稼働中プロセスへ遡及適用しない。
- 配送要求は重複せず、retry 待ち・dead-letter・input send failure で prompt と retry 順序を失わない。launch の dead record が newer live work を永久に塞がず、live spawn failure は独立して上限 5 回で停止する。daemon 側は request id と ACK を相関し、PTY write の失敗を prompt 復元へ返す。ACK 応答喪失時は at-least-once retry となり得るため、再送の exactly-once 化は #208 に残す。
- terminal-only pane では Agent が自動起動する。
- 上限到達中も既存 Agent への配送は進み、新規 spawn は待つ。
- retry/backoff/dead-letter と build mismatch の既存安全策を壊さない。
- daemon start 直後の queue、TUI 再起動、adopted/missing attach、ACK 不明、terminal-only、Agent process exit、複数 Agent pane、未 attach の background snapshot を回帰テストする。
