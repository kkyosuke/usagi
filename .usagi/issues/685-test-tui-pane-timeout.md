---
number: 685
title: test(tui): 生きた pane への入力が捨てられても打ち直さず timeout する
status: done
priority: high
labels: [v2, tui, test, ci]
dependson: []
related: [682, 684]
created_at: 2026-08-14T00:44:54.652099+00:00
updated_at: 2026-08-14T00:55:24.154520+00:00
---

## 問題・影響

`tests/cli_tui_pty.rs::real_pty_background_terminal_exit_closes_its_tab_through_scope_inventory` が
PR #1488 の **coverage job** で落ちた（同じ commit の full test job は success）。

- run: <https://github.com/kkyosuke/usagi/actions/runs/31757554114>（attempt 1 の `Rust coverage` job）
- `timed out waiting for generic-input:foreground-after-background-exit`
- binary は 86.56s（instrumentation 下）

**これは手元の CPU 競合ではなく CI で起きている。** coverage gate は全 PR 共有の required check なので、
無関係な PR を落とす。

## 原因

失敗時の画面が原因をそのまま出していた。

```text
[closeup] Ctrl-O: x/Ctrl-X close / … terminal is reconnecting; keystroke not delivered
```

TUI（`usagi_tui` の `usecase::application::terminal_session`）は、lane が reconnect 中・resync 中・
所有者不明などの状態で受けた keystroke を**捨て**、その事実を status bar に出す。テストは
`send()` してから echo（`generic-input:...`）を待つだけなので、**捨てられた keystroke を待ち続ける**。
落ちた入力は永久に現れないので、deadline をいくら伸ばしても直らない。

instrumentation は reconnect の窓を広げるだけで、原因ではない。したがって
「timeout を実時間ではなく進捗で測る」形にしても直らない（TUI は待っている間ずっと
うさぎの animation を描き続けるので、出力の停滞も検出できない）。**product 自身の
「届かなかった」報告を観測して打ち直す**のが唯一の正しい直し方である。

同じ形（生きた pane へ 1 行打って echo を待つ）は同 file に 9 か所ある。落ちたのは 1 か所だが、
露出は同じである。

## 診断の腐り

`wait_for_screen_since` は失敗時に input feedback を出すが、照合が固定文字列リストで、
product 側の現在の文言（`... keystroke not delivered`）を 1 つも含んでいなかった。そのため
**まさに keystroke が捨てられていた失敗が `feedback=[]` と表示され**、原因を取り違えさせた。

## 対象責務

- 生きた pane へ 1 行入力する 9 か所を、「TUI が捨てたと報告したら打ち直す」bounded loop に置き換える。
  単に遅いだけの run（instrumentation 下）では TUI は何も報告しないので、そのまま待つ。
  これで「遅い」と「落ちた」を product 自身の報告で切り分ける。
- 失敗時の feedback 照合を固定リストではなく語尾（`keystroke not delivered`）で行い、
  描画された行そのものを出す。文言が変わっても腐らない。

## 非対象

- deadline の延長、固定 sleep の追加。捨てられた入力はどちらでも直らない。
- 待ちの上限を「実時間」から「進捗の停滞」へ変える設計。この test では TUI が常に再描画しているため
  停滞を検出できず、上限が永久に来なくなる。
- coverage job から重い E2E を外す緩和。coverage 100% gate と整合しなくなるうえ、
  本件は instrumentation 固有ではなく「捨てられた入力を待つ」バグなので、外しても別環境で再発する。

## 受入条件

- [ ] 生きた pane への 1 行入力が、捨てられたら打ち直され、echo を観測して初めて次へ進む。
- [ ] `cargo llvm-cov --no-report -p usagi --test cli_tui_pty`（instrumentation 下＝CI coverage job と同条件）が複数回 green。
- [ ] 失敗時に、捨てられた keystroke の通知が feedback として表示される。
- [ ] deadline も固定 sleep も増やしていない。
