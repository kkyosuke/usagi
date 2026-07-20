---
number: 464
title: fix(v1/orchestrator): dependency PR head を worker worktree の基点にする
status: done
priority: high
labels: [review, v1, orchestration, git]
dependson: []
related: [182, 186]
parent: 453
created_at: 2026-07-20T12:06:21.033487+00:00
updated_at: 2026-07-20T21:14:33.238420+00:00
---

## 問題・影響

出荷中 v1 の `v1/src/usecase/orchestrator.rs::work_ready` は dependency PR の head を `Base` として算出するが、`delegate_worker` は prompt に書くだけで `session::create_with_agent` の worktree 基点へ渡さない。worker は dependency 実装を含まない既定 branch から開始し、誤った設計・重複修正を行う。

## 成立条件 / 再現フロー

dependency issue の PR head にだけ存在する sentinel commit を作り、dependent issue を ready にして worker を dispatch する。prompt は head を示す一方、worker worktree の `HEAD` と file tree は既定 branch のままになる。

## 対象責務と非対象

orchestrator の dependency base resolution と session creation input、checked-out commit の検証を対象とする。複数 dependency の merge strategy 変更、PR 自動 merge、v2 supervisor は非対象。

## 受入条件

- [ ] resolved dependency PR head を session/worktree creation の実基点として渡す。
- [ ] spawn 前に実 worktree `HEAD` が resolved immutable commit と一致することを確認する。
- [ ] missing/moved/unfetchable head は worker を起動せず typed blocked/error にする。
- [ ] prompt の provenance と実 checkout が同じ commit を参照する。

## 必須回帰テスト

実 Git repo と dependency PR fake で sentinel commit を用意し、worker worktree の `HEAD`/content、head 不在・移動・fetch failure、dependency なしの既定基点を検証する。

## docs / 移行影響

v1 orchestrator の dependency/base 契約を `v1/README.md` または既存 orchestration docs に追記する。既存 session の rebase/migration は行わず、新規 dispatch から適用する。
