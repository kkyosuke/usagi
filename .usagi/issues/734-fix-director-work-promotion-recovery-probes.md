---
number: 734
title: "fix(daemon): Director Work の promotion 競合と診断ノイズを解消する"
status: done
priority: high
labels: [v2, daemon, supervisor, tui, ipc, correctness]
dependson: []
related: [527, 665, 700, 719]
created_at: 2026-09-05T09:30:00+00:00
updated_at: 2026-09-05T10:12:19+00:00
---

## 問題

Director Work の Agent promotion を durable に予約してから Agent admission が確定するまでに、1秒周期の
Supervisor tick が未割り当て `Ready` task を即座に escalate する。admission failure は通常の `Cancel` を
適用するため `TerminalRun` で拒否され、stale reservation が毎秒 reconciliation error を残し続ける。

同じ運用ログには、product 自身の raw socket readiness probe が handshake 前に切断されることで生じる
`peer process identity unavailable`、明示的に開いた non-Git tenant を Git orphan cleanup に渡す周期エラー、
低い `RLIMIT_NOFILE` 下での established client capacity 枯渇も記録されている。

## 受入条件

- [x] Goal root と delegated task の promotion 予約中は通常の unassigned-Ready escalation の対象外である。
- [x] admission success は予約印を消費し、admission failure は旧版が作った matching escalation からも終端へ収束する。
- [x] reserve → tick → success/failure と、既存 escalated snapshot の回帰テストがある。
- [x] daemon の内部 readiness probe は必須 hello を送り、protocol error 応答も endpoint 到達として扱う。
- [x] 明示的に開いた non-Git tenant の orphan cleanup は Git 候補なしとして成功する。
- [x] daemon は許可された hard limit まで descriptor soft limit を引き上げ、失敗時は従来上限へ安全に fallback する。
- [x] 周期 Supervisor reconciliation の同一エラーは状態が変わるまで一度だけ記録する。
- [x] daemon / IPC / TUI の仕様と回帰テストを更新し、risk-based gate が green になり、PR CI を追跡できる。
