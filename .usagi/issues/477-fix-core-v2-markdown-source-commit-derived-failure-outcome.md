---
number: 477
title: fix(core): v2 Markdown source commit と derived failure の outcome を分離する
status: done
priority: medium
labels: [review, v2, core, persistence]
dependson: []
related: [113, 211]
parent: 453
created_at: 2026-07-20T12:06:46.908907+00:00
updated_at: 2026-07-20T22:33:06.399814+00:00
---

## 問題・影響

root/v2 の `crates/core/src/infrastructure/persistence/markdown_store.rs` を使う `IssueStore` / `MemoryStore` は source Markdown の atomic write/remove 後、derived index または `MEMORY.md` rebuild が失敗すると全 operation を `Err` として返す。source は既に commit 済みなので caller retry が duplicate create や誤った failure 表示を起こす。

## 成立条件 / 再現フロー

source file write は成功し derived index/TOC の rename/write だけを failpoint で失敗させる。API は error を返すが source read は新状態を示し、同じ create retry は別番号/重複を作り得る。

## 対象責務と非対象

v2 store の source SoT commit point、derived cache failure outcome、self-heal scheduling を対象とする。freshness 強化は #478、v1 実装は #479/#480、Markdown format 変更は非対象。

## 受入条件

- [ ] source commit と derived refresh を別 phase/outcome として表現し、commit 済み mutation を未適用 error に見せない。
- [ ] derived failure は dirty/rebuild-needed として記録し、次 read/startup で source から自己修復する。
- [ ] create/update/remove retry は source identity を確認し duplicate/二重削除を起こさない。
- [ ] source failure は従来どおり mutation なしの error を返す。

## 必須回帰テスト

index/TOC write・rename failpoint を create/update/remove に入れ、source committed outcome、retry idempotency、次回 read/reopen の self-heal、source failure rollback を検証する。

## docs / 移行影響

persistence docs に Markdown SoT と cache dirty outcome を記載する。derived file は破棄・再生成可能で、durable source migration はない。
