---
number: 664
title: backlog: origin/main 4224b7ae daemon / terminal UI/UX 追補レビュー
status: todo
priority: high
labels: [review, v2, backlog, epic, tui, daemon, terminal, uiux]
dependson: []
related: []
created_at: 2026-08-06T20:34:02.093604+00:00
updated_at: 2026-08-06T22:33:28.840112+00:00
---

## レビュー基点

- reviewed commit: `4224b7ae2260ed1812a03353d4540626109361f0`
- reviewed at: 2026-08-06
- 観点: daemon status の到達可能性・鮮度、daemon-owned terminal のスクロール安定性、打鍵から ACK / echo までの応答性、PTY output から表示までの反映遅延

## 前回 backlog から解消済み

`origin/main` には #655〜#663 の修正が入り、live VT parser bound、Agent readiness preflight、critical worker supervision、New workspace 非同期化、scrollback O(1) eviction、frame material cache、frame diff run 化などは解消済みである。本 backlog はそれらを重複起票せず、最新実装に残る interactive path の問題だけを扱う。

## Finding 対応表

| priority | issue | invariant |
|---|---:|---|
| high | #665 | terminal control RPC 中も input / draw / scroll / quit を止めない |
| high | #666 | foreground output の VT apply を 1 frame の byte/time budget 内へ分割する |
| high | #672 | Agent tab intent のfile lock / fsyncをinput/render threadから分離する |
| medium | #667 | live bottom を離れた viewport を新規出力から固定し、新着と復帰操作を表示する |
| medium | #668 | click / drag は retained history 全体でなく viewport snapshot から開始する |
| medium | #669 | terminal copy の clipboard subprocess を render thread から外し bounded にする |
| medium | #670 | pathological な長い wrapped logical line でも viewport projection を frame budget 内に保つ |
| medium | #671 | daemon status の全 runtime をscroll可能にし、開いている間も鮮度を明示して更新する |
| medium | #673 | ready completion/action のburstをper-frame budgetで分割しinputを飢餓させない |

## 共通 UX invariant

- daemon / PTY / OS helper が遅くても、TUI は 1 frame 予算内で input・scroll・modal・quit を再び処理できる。
- input は順序・effect-unknown fenceを維持し、local echoやblind retryで成功を捏造しない。
- live bottom を離れた viewport は、retention で失われるまで同じ論理行を保持し、新規出力を明示する。
- display projection と pointer hit-test は同じ bounded viewport material を参照する。
- daemon status は省略した行を操作で到達可能にし、古い観測を現在値として見せない。
- daemon-owned Agent pane の表示intent永続化が遅くても、tab操作・draw・quitを止めず、durable successを捏造しない。
- background laneが継続的にreadyでも、bounded/fairなdrainで毎frame inputへ戻る。

## 完了条件

- #665〜#673 の実装と failure / boundary test がすべて `done`。
- burst output・hung terminal request・hung clipboard helper・継続出力中のscroll・10,000行history上のclick・長いwrapped logical line・viewportを超えるdaemon runtime一覧と状態遷移・競合中のAgent tab intent lock・completion burstを同時に含む実PTY回帰で、input / draw / quitのwall-clock boundと表示鮮度契約を満たす。
- `document/03-tui.md` と `document/04-ipc.md` が実装済みのframe scheduler・input ACK・scroll anchor・daemon status鮮度・Agent tab intent commit契約を正本として記載する。
