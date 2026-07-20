---
number: 481
title: fix(v2/cli): daemon request failure を非 0 exit で返す
status: done
priority: medium
labels: [review, v2, cli, daemon]
dependson: []
related: [275]
parent: 453
created_at: 2026-07-20T12:06:48.193493+00:00
updated_at: 2026-07-20T23:10:40.135770+00:00
---

## 問題・影響

root/v2 の `src/runtime/cli.rs::dispatch` は `RunOutcome::DaemonRequest` の client 作成/request failure を stderr に表示した後 `Ok(())` を返す。shell/CI は daemon 不在、protocol rejection、application failure を exit 0 と誤認し、automation が次工程を実行する。

## 成立条件 / 再現フロー

daemon を停止するか invalid/stale request を返す fake daemon へ v2 CLI command を送って `$?` を確認する。error text が出ても process status は 0 になる。

## 対象責務と非対象

CLI runtime の typed command outcome→`ExitCode` mapping、safe stderr と success/accepted 判定を対象とする。daemon error taxonomy の全面再設計、TUI notice、v1 CLI は非対象。

## 受入条件

- [ ] daemon connection、transport、protocol、application rejection は非 0 exit を返す。
- [ ] `Ok` と契約上の `Accepted` だけが 0 になり、stderr と exit status が矛盾しない。
- [ ] library/usecase 内で `process::exit` せず、composition root が typed outcome を map する。
- [ ] safe user message と diagnostic/error code を保持する。

## 必須回帰テスト

実 CLI process test で daemon 不在、socket failure、protocol rejection、stale/invalid request、accepted、success の stdout/stderr/exit code を table 化する。

## docs / 移行影響

CLI docs に exit status contract を記載する。従来 error 時も 0 を期待した automation は修正が必要だが、wire/data migration はない。
