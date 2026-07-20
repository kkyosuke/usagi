---
number: 463
title: fix(tui): TerminalSession を reconnect し入力失敗を投影する
status: todo
priority: high
labels: [review, v2, tui, terminal, resilience]
dependson: []
related: [216, 265, 303, 365, 385, 388]
parent: 453
created_at: 2026-07-20T12:06:20.675238+00:00
updated_at: 2026-07-20T12:06:20.675238+00:00
---

## 問題・影響

root/v2 の `crates/tui/src/usecase/application/terminal_session.rs::TerminalSession::poll` は `Live` 以外で即 return し、一時的な `TerminalError::Unavailable` を永久 `Disconnected` にする。`send_input` は非 Live/subscribed 入力を無言で捨て、presentation の pane は Live 表示のままなので、利用者は入力が届いたと誤認する。

## 成立条件 / 再現フロー

live terminal の IPC socket を一時停止して poll/input を行い、daemon を復帰させる。session は再 attach/resync を試みず、同じ pane への入力は success 相当で消える。replacement terminal の自動 spawn は所有権を壊すため許可しない。

## 対象責務と非対象

`TerminalSession` の reconnect/backoff state machine、入力 `Result`、pane/notice projection を対象とする。daemon restart snapshot hydrate は #459、output retention は #472、terminal の replacement spawn は非対象。

## 受入条件

- [ ] `Unavailable` は capped exponential backoff 付き `Reconnecting` へ遷移し、同じ `TerminalRef` を attach/resync する。
- [ ] stale/orphaned/exited と一時 unavailable を区別し、terminal state を pane に投影する。
- [ ] 非 Live 入力は typed `Result` / feedback を返し、無言破棄や success 扱いをしない。
- [ ] reconnect 中に replacement spawn を行わず、detach/close で retry を停止する。

## 必須回帰テスト

fake clock で backoff 上限・reset・cancel を固定し、実 socket restart で reconnect→reattach/resync、入力 outcome、Disconnected/Reconnecting 表示、stale/exited 終端を検証する。

## docs / 移行影響

`document/03-tui.md` と `document/04-ipc.md` に terminal 接続状態、入力保証、retry UX を追記する。wire migration は不要。
