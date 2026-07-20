---
number: 487
title: test(tui): controller・presentation・production wiring を coverage 対象へ戻す
status: todo
priority: medium
labels: [review, v2, tui, coverage]
dependson: [484]
related: [356, 360, 380]
parent: 453
created_at: 2026-07-20T12:06:50.164691+00:00
updated_at: 2026-07-20T12:07:42.169284+00:00
---

## 問題・影響

root/v2 TUI は controller、presentation、`src/runtime/tui.rs`、`DaemonBackend` の business routing まで大量に `#[coverage(off)]` とし、通常起動だけ unavailable stub を使う回帰 (#462) や入力/completion 欠落を 100% gate で検出できない。

## 成立条件 / 再現フロー

production screen graph の port 注入、controller Effect routing、key ordering、completion/error branch を未実行のまま coverage を測る。function 全体が除外されるため green になる。

## 対象責務と非対象

TUI controller/presentation/application runtime と production selection/wiring の decision logic を coverage 対象へ戻す。raw terminal draw/read syscall の薄い adapter は #484 policy と integration test で扱う。core/daemon は #485/#486。

## 受入条件

- [ ] controller reducer、Effect executor、screen entry selection、completion、input classifier/error projection の規約外 exclusion を除く。
- [ ] direct/Welcome/Open/Recent production composition を同じ harness で測る。
- [ ] real terminal IO から pure mapping/state machine を分離し、残る exclusion を理由付き allowlist 化する。
- [ ] workspace 100% gate と #484 lint を維持する。

## 必須回帰テスト

全 Effect route、全 entry path、success/failure completion、Ctrl-O/global chord、terminal reconnect state、settings injection を deterministic harness で実行し coverage 対象を検査する。

## docs / 移行影響

TUI production harness と exclusion policy を開発 docs に記載する。利用者データ migration はない。
