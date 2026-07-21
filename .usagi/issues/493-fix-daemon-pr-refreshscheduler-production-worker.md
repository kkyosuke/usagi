---
number: 493
title: fix(daemon): PR RefreshScheduler を production worker に配線する
status: done
priority: medium
labels: [review, v2, daemon, pullrequest]
dependson: []
related: [346]
parent: 453
created_at: 2026-07-20T12:06:52.141783+00:00
updated_at: 2026-07-21T02:19:31.427226+00:00
---

## 問題・影響

root/v2 の `crates/daemon/src/usecase/pr_inventory.rs::RefreshScheduler` は test にしか生成されず、production daemon に refresh worker/tick がない。PR inventory は scheduler の dedupe/backoff/freshness 契約を通らず、TUI/MCP が stale snapshot を読み続けるか別実装へ依存する。

## 成立条件 / 再現フロー

production daemon で refresh 対象を登録し、期限到来・remote failure・再試行を観測する。`RefreshScheduler::new/default` の production caller がなく、tested task selection が実行されない。

## 対象責務と非対象

daemon composition の scheduler instance、clock/tick worker、PR provider、snapshot publish、shutdown を対象とする。PR UI overlay (#317)、browser、provider feature追加は非対象。

## 受入条件

- [ ] production daemon が scheduler を生成し、bounded worker/tick で due refresh を実行する。
- [ ] duplicate request を coalesce し、failure backoff、success freshness、shutdown/cancel を scheduler SSoT で扱う。
- [ ] ad hoc/no-op refresh path を削除し、snapshot consumer に safe stale/error metadata を返す。
- [ ] daemon restart 後の schedule rebuild を deterministic にする。

## 必須回帰テスト

fake clock/provider と production composition で due/not-due、dedupe、failure backoff、success publish、restart、shutdown、slow provider の worker bound を検証する。

## docs / 移行影響

PR refresh/freshness 契約を daemon/TUI docs に追記する。snapshot schema を変える場合だけ version migration を定義する。
