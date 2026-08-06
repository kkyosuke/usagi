---
number: 664
title: backlog: origin/main 4224b7ae terminal UI/UX 追補レビュー
status: todo
priority: high
labels: [review, v2, backlog, epic, tui, terminal, uiux]
dependson: []
related: []
created_at: 2026-08-06T20:34:02.093604+00:00
updated_at: 2026-08-06T20:49:32.790879+00:00
---

## レビュー基点

- reviewed commit: `4224b7ae2260ed1812a03353d4540626109361f0`
- reviewed at: 2026-08-06
- 観点: daemon-owned terminal のスクロール安定性、打鍵から ACK / echo までの応答性、PTY output から表示までの反映遅延

## 前回 backlog から解消済み

`origin/main` には #655〜#663 の修正が入り、live VT parser bound、Agent readiness preflight、critical worker supervision、New workspace 非同期化、scrollback O(1) eviction、frame material cache、frame diff run 化などは解消済みである。本 backlog はそれらを重複起票せず、最新実装に残る interactive path の問題だけを扱う。

## Finding 対応表

| priority | issue | invariant |
|---|---:|---|
| high | #665 | terminal control RPC 中も input / draw / scroll / quit を止めない |
| high | #666 | foreground output の VT apply を 1 frame の byte/time budget 内へ分割する |
| medium | #667 | live bottom を離れた viewport を新規出力から固定し、新着と復帰操作を表示する |
| medium | #668 | click / drag は retained history 全体でなく viewport snapshot から開始する |
| medium | #669 | terminal copy の clipboard subprocess を render thread から外し bounded にする |

## 共通 UX invariant

- daemon / PTY / OS helper が遅くても、TUI は 1 frame 予算内で input・scroll・modal・quit を再び処理できる。
- input は順序・effect-unknown fenceを維持し、local echoやblind retryで成功を捏造しない。
- live bottom を離れた viewport は、retention で失われるまで同じ論理行を保持し、新規出力を明示する。
- display projection と pointer hit-test は同じ bounded viewport material を参照する。

## 完了条件

- #665〜#669 の実装と failure / boundary test がすべて `done`。
- burst output・hung terminal request・hung clipboard helper・継続出力中のscroll・10,000行history上のclickを同時に含む実PTY回帰で、input / draw / quitのwall-clock boundを満たす。
- `document/03-tui.md` と `document/04-ipc.md` が実装済みのscheduler・input ACK・scroll anchor契約を正本として記載する。
