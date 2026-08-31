---
number: 722
title: fix(architecture): 文書と実装の依存方向を一致させて全crateで検査する
status: done
priority: medium
labels: [v2, architecture, clean-architecture, test]
dependson: []
related: []
created_at: 2026-08-26T00:00:00+00:00
updated_at: 2026-08-26T00:00:00+00:00
---

## Finding

`document/02-architecture.md` と crate root は `usecase → domain ← infrastructure` を宣言していたが、
core usecase は concrete store / Git / path / wire contract を `core/infrastructure` から直接使う。
daemon の usecase も共有 IPC / store / Git contract を参照する。一方、既存 architecture test は
daemon usecase から presentation への import だけを検査し、この不一致を検出しない。

## 設計判断

`core/infrastructure` は外部 adapter 専用層ではなく、面をまたぐ wire contract、transactional store、
Git effect を所有する common technical boundary とする。core usecase はそれらの注入値を直接合成できる。
domain の外部非依存、面クレートの相互非依存、face usecase の同一 face adapter / presentation 非依存を
hard boundary とする。

## 受入条件

- [x] 実際の依存行列と、古典的4層から意図的に外す core technical boundary の理由が正本文書にある。
- [x] 全 face manifest の usagi crate dependency を検査し、face 間の直接依存を拒否する。
- [x] production Rust AST を全 crate で走査し、domain の外向き依存、usecase / infrastructure から
      presentation への逆流、face usecase から同一 face infrastructure への依存を拒否する。
- [x] コメントと test-only fake は production dependency と取り違えない。
