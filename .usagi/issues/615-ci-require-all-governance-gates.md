---
number: 615
title: ci: main ruleset で full-test coverage Markdown gate を必須化する
status: done
priority: high
labels: [review, ci, governance, security, test]
dependson: []
related: [203, 601]
created_at: 2026-07-31T15:00:00+09:00
updated_at: 2026-08-01T00:00:00+09:00
---

## Finding（P1 CI/governance）

2026-07-31 に GitHub API で active ruleset `17627257` を確認すると、required contexts は `test` と `enforce-base-main` だけである。`document/06-conventions.md` が最終 gate とする `full-test`、`coverage`（coverage-off lint を含む）、Markdown link check は通常 merge / bypass で green を強制されない。Markdown workflow は path filter で context 自体が出ない変更があるため、現状のまま required にすると merge が永久 pending になる。

## 最小修正方針

各 workflow を常時起動し、対象外 path でも同名 aggregate context を success report する設計に統一する。job 名/context を stable にした後 ruleset 17627257 の required status checksへ追加し、bypass policyと監査手順も文書化する。

## テストと受け入れ条件

- Rust、Markdown-only、無関係 path の fixture PR すべてで required context set が必ず現れる。
- Rust 差分は full-test/coverage/coverage-off lint failure、Markdown 差分は broken link failure が merge をblockする。
- API readback で ruleset に `test`、`enforce-base-main`、`full-test`、`coverage`、always-reporting Markdown aggregate が登録される。
- workflow rename 時に ruleset drift を検知する自動 test/audit がある。
