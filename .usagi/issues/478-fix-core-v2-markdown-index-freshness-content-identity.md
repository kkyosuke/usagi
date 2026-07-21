---
number: 478
title: fix(core): v2 Markdown index freshness を content identity で判定する
status: in-progress
priority: medium
labels: [review, v2, core, persistence]
dependson: []
related: [113, 211]
parent: 453
created_at: 2026-07-20T12:06:47.205151+00:00
updated_at: 2026-07-21T12:27:07.765211+00:00
---

## 問題・影響

root/v2 の `MarkdownStore::load_fresh_index` は source entry 件数と mtime が index より新しいかだけで freshness を判断する。同件数の delete+add/rename、内容変更後の mtime 保存、粗い timestamp では stale index を fresh と採用し、issue/memory search/list が source Markdown と食い違う。

## 成立条件 / 再現フロー

同数の source を別 key に置換するか、内容を変えて mtime を index 以前へ戻して search/list する。件数+mtime guard を通り、旧 title/body/identity が返る。

## 対象責務と非対象

root/v2 derived index の reproducible source identity/fingerprint、freshness validation と corrupt metadata fallback を対象とする。source/derived outcome は #477、v1 concurrency は #480、全文検索機能追加は非対象。

## 受入条件

- [ ] key set と content identity/revision を含む deterministic fingerprint で source と index を照合する。
- [ ] rename、delete+add、preserved/coarse mtime、same-size content change を stale と判定する。
- [ ] corrupt/unknown fingerprint は source scan/rebuild へ fail safe し、stale result を返さない。
- [ ] fingerprint 計算の cost と cache contract を計測・文書化する。

## 必須回帰テスト

same-count rename、delete+add、content change+preserved mtime、coarse mtime、corrupt/legacy metadata、正常 fast path を issue/memory の双方で検証する。

## docs / 移行影響

index schema/version と rebuild migration を persistence docs に記載する。legacy index は source を変更せず一度 rebuild する。
