---
number: 178
title: docs: agent と開発者の検証規約をリスク比例へ更新する
status: done
priority: high
labels: [docs, test]
dependson: [179, 180]
related: []
parent: 177
created_at: 2026-07-10T23:35:22.578241+00:00
updated_at: 2026-07-11T02:31:02.335280+00:00
---

## 目的

`.agents/workflow.md` と `document/06-conventions.md` の「commit 前に常に全 cargo test」を、ローカル fast loop/commit/PR gate の段階規約へ更新する。実装済みの CI/推奨 script と一致した時点で変更し、予定を正本へ書かない。

## 規約内容

- 編集中: fmt check、check、変更 module/target と直接 consumer の test。
- commit 前: clippy all-targets と risk-based selected tests。全件条件を明示する。
- push/PR 前および CI: Rust 差分は full test + coverage 100%。coverage が test を兼ねるローカル経路では重複 test を避ける。
- agent の完了報告には「実行 command / 結果 / 未実行 gate と理由 / full test 必須条件への該当」を含める。
- docs-only は Rust gate を省略可能だが markdown link check を必須とする。

## 完了条件

workflow と conventions の SSoT が矛盾せず、具体的コマンド、全件条件、選択テストの非代替性が記載される。
