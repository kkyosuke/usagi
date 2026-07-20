---
number: 479
title: fix(v1/store): Markdown source commit と derived failure の outcome を分離する
status: in-progress
priority: medium
labels: [review, v1, persistence, durability]
dependson: []
related: [113, 117]
parent: 453
created_at: 2026-07-20T12:06:47.530967+00:00
updated_at: 2026-07-20T23:26:31.802409+00:00
---

## 問題・影響

出荷中 v1 の `v1/src/infrastructure/markdown_store.rs` でも source Markdown write/remove 後の index/TOC rebuild failure を operation 全体の `Err` とする。出荷 CLI/MCP は既に適用済みの mutation を retry し、duplicate issue/memory や誤った利用者表示を起こせる。

## 成立条件 / 再現フロー

v1 issue/memory の source write 成功後に derived write/rename を失敗させる。command は失敗するが source は更新済みで、同じ command retry の意味が変わる。

## 対象責務と非対象

v1 store の source commit point、derived dirty outcome、retry idempotency を対象とする。rebuild lock/freshness は #480、root/v2 は #477/#478、format redesign は非対象。

## 受入条件

- [ ] source commit 済みと未適用を区別する outcome を v1 caller まで伝える。
- [ ] derived failure を rebuild-needed として自己修復し、source を rollback/重複適用しない。
- [ ] create/update/remove の retry が source identity に対して idempotent である。
- [ ] source write failure は mutation なしを保証する。

## 必須回帰テスト

v1 issue/memory の index/TOC failpoint で create/update/remove、CLI/MCP retry、process reopen self-heal、source failure を検証する。

## docs / 移行影響

v1 persistence/CLI docs に partial cache failure の利用者 outcome を記載する。derived file は起動時に再生成し、source migration はない。
