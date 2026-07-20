---
number: 476
title: fix(daemon): full TerminalRef 検証後にだけ resize effect を実行する
status: done
priority: high
labels: [review, v2, daemon, terminal, security]
dependson: []
related: [214, 218, 264, 309]
parent: 453
created_at: 2026-07-20T12:06:25.014656+00:00
updated_at: 2026-07-20T21:40:10.557199+00:00
---

## 問題・影響

root/v2 の `crates/daemon/src/usecase/terminal_ipc.rs::GenericTerminalRuntime::request` は Resize で `pty.resize` を先に実行し、その後 `coordinator.resize` で full `TerminalRef` を検証する。production `DaemonPty::resize` は terminal ID だけで lookup するため、workspace/session/worktree/generation を偽装した stale ref が他 owner の実 PTY geometry を変更してから拒否される。

## 成立条件 / 再現フロー

有効な terminal ID に別 generation/workspace/session/worktree を組み合わせた `TerminalRef` で Resize を送る。response は stale でも PTY adapter の resize effect が 1 回発生する。

## 対象責務と非対象

full ref の read-only preflight validation、effect 後 commit 順序、generic/Agent resize の共通 contract を対象とする。resize geometry validation 自体や PTY library bug は非対象。

## 受入条件

- [ ] workspace/session/worktree/terminal/generation/ownership の全 fence を effect 前に検証する。
- [ ] forged/stale ref は PTY effect 0 で typed error を返す。
- [ ] PTY resize failure 時は coordinator の committed geometry を更新しない。
- [ ] validate と effect の間の exit/replacement race を generation token または lock で fence する。

## 必須回帰テスト

各 `TerminalRef` field を 1 つずつ偽装した table test、PTY failure、validate 後 exit/replacement race、valid resize を fake effect counter と production adapter で検証する。

## docs / 移行影響

`document/04-ipc.md` に terminal command の validate-before-effect invariant を追記する。migration はない。
