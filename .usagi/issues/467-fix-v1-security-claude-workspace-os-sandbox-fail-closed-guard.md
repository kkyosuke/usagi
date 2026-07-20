---
number: 467
title: fix(v1/security): Claude workspace を OS sandbox と fail-closed guard で隔離する
status: done
priority: high
labels: [review, v1, security, claude, sandbox]
dependson: []
related: [105, 107, 108, 149]
parent: 453
created_at: 2026-07-20T12:06:22.042681+00:00
updated_at: 2026-07-20T13:44:59.581344+00:00
---

## 問題・影響

出荷中 v1 の `v1/src/presentation/cli/guard_workspace.rs::{run,deny_reason}` と `v1/src/usecase/workspace_guard.rs::{normalize,command_mutates_repo}` は malformed input を許可し、lexical path と限定的な `git` token だけを見る。Bash、redirection、`sed`/`rm`、wrapper、absolute git、symlink で workspace 外を変更でき、`v1/src/infrastructure/agent/claude.rs` の headless 経路は `--dangerously-skip-permissions` も使う。

## 成立条件 / 再現フロー

Claude hook に欠落/不正 JSON、`/usr/bin/git`、`sh -c`、redirect、`sed -i`、`rm`、workspace 内 symlink→外部 sentinel を与える。guard が fail open または lexical in-root と判定し、外部変更を止められない。

## 対象責務と非対象

interactive/headless の OS sandbox を主境界にし、canonical/symlink-safe allow roots と fail-closed hook を防御層として定義する。Claude の一般 permission UX、Codex sandbox、root/v2 Agent adapter は非対象。

## 受入条件

- [ ] interactive/headless の全 Claude launch が platform OS sandbox を通り、workspace/session allow root 外を書けない。
- [ ] canonicalization failure、symlink escape、malformed/unknown mutating payload は fail closed にする。
- [ ] hook は Bash/wrapper/absolute git/redirection/mutating utilities を漏れなく deny または sandbox に委ね、hook 単独を hard boundary と称さない。
- [ ] sandbox unavailable の degraded mode は無保護起動せず明示的に失敗する。

## 必須回帰テスト

実 symlink/external sentinel と adversarial command table/fuzz で Bash、redirect、sed/rm、wrapper、absolute git、malformed input、headless を拒否し、in-root write と root read-only の許可を検証する。

## docs / 移行影響

v1 security/Agent docs を OS sandbox 主境界へ修正し、対応 platform と fail-closed degraded mode を明記する。既存 hook 設定は再生成が必要になり得る。
