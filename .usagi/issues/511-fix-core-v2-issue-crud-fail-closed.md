---
number: 511
title: fix(core): v2 issue CRUD を重複番号で fail-closed にする
status: done
priority: high
labels: [review, v2, core, issue, persistence, safety]
dependson: []
related: [335, 471]
created_at: 2026-07-21T21:29:10.408119+00:00
updated_at: 2026-07-22T11:01:33+00:00
---

## 問題・影響

v2 の `IssueStore` は、同じ番号を持つ `NNN-*.md` が複数ある場合でも point CRUD の identity を一意に検証しない。`read_locked` は directory iteration 順の先頭を返し、`write_locked` は選ばれなかった sibling を stale filename として削除し、`remove_with_outcome` は同番号の全 sibling を削除する。現行 backlog には #323 と #390 の番号衝突が実在するため、任意読取と不可逆な誤更新・誤削除が現実に起こり得る。

初回修正 #1226 のマージ後も、point read の一意性判定より先に derived repair が走り得る lock 範囲、search が3回の source snapshot を混在させる競合、`session_delegate_issue` が typed ambiguity を失う error mapping、missing store の read が lock directory を作る副作用が残った。本 follow-up は #1226 マージ後の main を基点に、この4点を同じ受入条件の未達として完了させる。

## 対象責務

- v2 `crates/core/src/infrastructure/store/issue.rs` の point read/write/remove を、同番号 source が複数あるとき typed ambiguity error で fail-closed にする。
- ambiguity error は issue number と衝突した全 exact path を辞書順で保持し、get/update/delete と MCP adapter まで安全に伝播させる。
- list/search は repair のため source set を観測できる契約を保ち、point CRUD のように任意の sibling を選ばないことを明記する。
- v2 の正本 docs に番号 identity、point CRUD の fail-closed、明示 repair の境界を反映する。

## 受入条件

- [x] 同番号 sibling が2件以上ある場合、get/update/delete は同じ deterministic な ambiguity error を返す。
- [x] ambiguity 判定は dirty marker、target write、remove より前に行われ、失敗後も全 sibling が byte-for-byte 不変である。
- [x] 通常の0件/1件 CRUD、title rename、derived refresh/repair の既存契約を維持する。
- [x] list/search と MCP adapter の挙動・説明が fail-closed 契約と整合する。
- [x] store/usecase/adapter の必要範囲をテストし、v2 docs を更新する。

## 必須回帰テスト

- seeded duplicate に対する store read/write/remove の typed error、sorted exact paths、source/derived state 不変。
- core usecase get/update/delete の ambiguity 伝播と全 sibling byte-for-byte 不変。
- MCP `issue_get` / `issue_update` / `issue_delete` が実行エラーとして ambiguity を返し、source を変更しないこと。
- list/search が sibling を暗黙に collapse せず観測可能であること。

## スコープ外

現存する #323/#390 の renumber/delete や自動修復は行わない。履歴監査なしに正しい identity を推測しない。過去 cleanup の #335 と v1 専用修正 #471 は related として参照し、本 issue では v2 のみを修正する。
