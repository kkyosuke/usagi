---
number: 500
title: fix(v1/process): child timeout 後の terminate・reap・drain を有界化する
status: in-progress
priority: medium
labels: [review, v1, process, resilience]
dependson: []
related: [171, 189]
parent: 453
created_at: 2026-07-20T12:07:09.009717+00:00
updated_at: 2026-07-20T23:00:39.746390+00:00
---

## 問題・影響

出荷中 v1 の `v1/src/presentation/mcp/child_io.rs::wait_with_timeout` は timeout/error 後 `kill` してから blocking `wait()` し、caller は pipe drain thread を join する。kill failure/無視や stdout を保持する grandchild で timeout 後も無期限停止する。同様の kill+wait が `v1/src/infrastructure/release.rs::fetch_tags` 等にも重複する。

## 成立条件 / 再現フロー

SIGTERM/kill を無視または reap 不能な fake child、stdout FD を継承する grandchild を起動して Ollama/env resolver/PR title/release fetch の timeout を発火する。deadline 後に blocking wait/join が戻らない。

## 対象責務と非対象

process group 単位の terminate→grace→force、bounded reap/drain、detached reaper、共通 primitive と全 4 call site 移行を対象とする。command-specific retry、Windows の全面 process supervisor は非対象だが platform差は明示する。

## 受入条件

- [ ] API の wall-clock deadline が kill/reap/drain を含む end-to-end bound になる。
- [ ] child process group と pipe を安全に閉じ、grandchild が FD を保持しても caller を永久 block しない。
- [ ] deadline 内に reap できない場合は detached cleanup/diagnostic を行い zombie を残さない。
- [ ] MCP Ollama、env resolver、PR title pool、release fetch が同じ primitive を使う。

## 必須回帰テスト

kill failure/never-exit fake、real child+grandchild stdout holder、large output、normal success、timeout の wall-clock bound と eventual no-zombie を platform別に検証する。

## docs / 移行影響

v1 process timeout/cleanup contract と platform limitation を開発 docs に記載する。data migration はない。
