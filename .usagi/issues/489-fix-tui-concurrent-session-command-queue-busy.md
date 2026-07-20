---
number: 489
title: fix(tui): concurrent session command を queue または Busy として完了させる
status: todo
priority: medium
labels: [review, v2, tui, session, concurrency]
dependson: [462]
related: [314, 315]
parent: 453
created_at: 2026-07-20T12:06:50.826920+00:00
updated_at: 2026-07-20T12:07:42.466913+00:00
---

## 問題・影響

root/v2 の `crates/tui/src/presentation/mod.rs::WorkspaceUi` は `session_commands: Option<Box<...>>` を worker に `take()` させ、`begin_session_command` は `None` なら silent return する。同時 2 件目の create/remove Effect が completion なしで消え、pending skeleton/overlay が永久に残る。

## 成立条件 / 再現フロー

遅い create worker が port を所有中に別 create または remove を発火する。2 件目は effect を受理した controller state を持つが backend action/completion がなく、利用者 feedback もない。

## 対象責務と非対象

#462 後の単一 production backend における session command admission、bounded queue または明示 Busy completion、worker lifecycle を対象とする。daemon 自体の operation idempotency、無制限並列実行は非対象。

## 受入条件

- [ ] 各 Effect は exactly one の実行または token/operation 対応 Busy/error completion を受ける。
- [ ] queue を採る場合は bound、順序、cancel、workspace switch policy を定義する。
- [ ] worker panic/channel close/out-of-order completion でも port と pending UI を回復する。
- [ ] silent return と永久 pending state を残さない。

## 必須回帰テスト

create+create、create+remove、remove+create、worker panic、channel close、out-of-order、workspace exit を barrier/fake worker で検証し、effect/completion 数が 1:1 であることを固定する。

## docs / 移行影響

`document/03-tui.md` に session operation の Busy/queue UX を記載する。wire/data migration はない。
