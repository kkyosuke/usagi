---
number: 480
title: fix(v1/store): Markdown rebuild を lock し freshness を堅牢化する
status: done
priority: medium
labels: [review, v1, persistence, concurrency]
dependson: []
related: [113, 117]
parent: 453
created_at: 2026-07-20T12:06:47.860251+00:00
updated_at: 2026-07-21T02:10:29.877091+00:00
---

## 問題・影響

出荷中 v1 の `v1/src/infrastructure/markdown_store.rs::{summaries,rebuild_derived,load_fresh_index}` は rebuild の source scan→derived write を store lock 外で行い、並行 writer 後に古い index/TOC を上書きできる。freshness も件数+mtime のため same-count/equal-mtime replacement を見逃す。

## 成立条件 / 再現フロー

rebuild が旧 snapshot を scan した地点で barrier を止め、別 process が create/update/remove してから rebuild を完了させる。新 source に対して旧 derived state が保存され、mtime条件次第で fresh と採用される。

## 対象責務と非対象

v1 rebuild の lock/version retry、source fingerprint、concurrent writer との収束を対象とする。source commit outcome は #479、root/v2 は #478、global issue allocator は #471。

## 受入条件

- [ ] scan→publish が shared store lock または revision compare/retry で 1 source revision に結び付く。
- [ ] key set/content identity を含む fingerprint で freshness を判定し、件数+mtimeだけに依存しない。
- [ ] writer と rebuild の順序にかかわらず derived state は最新 source SoT へ収束する。
- [ ] corrupt/legacy metadata は stale として安全に rebuild する。

## 必須回帰テスト

barrier 付き concurrent create/update/remove+rebuild、same-count rename、preserved/coarse mtime、process restart、legacy/corrupt index を検証する。

## docs / 移行影響

v1 cache schema/rebuild semantics を docs に記載する。legacy derived files は lock 下で再生成し、source Markdown は変更しない。
