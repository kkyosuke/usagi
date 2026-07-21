---
number: 486
title: test(daemon): reducer・reconcile・error path を coverage 対象へ戻す
status: done
priority: medium
labels: [review, v2, daemon, coverage]
dependson: [484]
related: [356, 360, 380]
parent: 453
created_at: 2026-07-20T12:06:49.822445+00:00
updated_at: 2026-07-21T13:06:59.998378+00:00
---

## 問題・影響

root/v2 daemon の runtime reducer、Agent/terminal admission、generation/reconcile/error mapping に広い `#[coverage(off)]` があり、test 済み usecase と production wiring の断絶や effect ordering failure を coverage gate が見逃す。

## 成立条件 / 再現フロー

`src/runtime/daemon.rs`、`crates/daemon/src/usecase/agent_ipc.rs`、terminal/generation/reconcile path の excluded branch を未実行にしても 100% gate が成功する。

## 対象責務と非対象

daemon reducer、routing、reconcile、error/effect ordering の decision logic を coverage 対象へ戻す。socket accept、OS signal、実 PTY syscall の薄い adapter は #484 の理由付き allowlist と production integration test で扱う。core/TUI は #485/#487。

## 受入条件

- [ ] admission/replay/restart/fencing/resize/output/error routing の規約外 exclusion を除く。
- [ ] production composition を injectable port/failpoint で通し、test-only constructor だけを測らない。
- [ ] 残る real IO exclusion は理由と integration coverage を持つ。
- [ ] workspace 100% gate と #484 lint を維持する。

## 必須回帰テスト

store failpoint、restart hydrate、stale ref、partial effect、unknown request、observer exit を含む branch tests と production composition test を追加し、coverage report の対象 symbol を検査する。

## docs / 移行影響

daemon test seam/production harness の開発 docs を必要に応じ更新する。runtime/data migration はない。
