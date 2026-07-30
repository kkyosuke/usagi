---
number: 586
title: docs/refactor(core): workspace_guard の実IOと "pure" 記述の不整合を解消する
status: in-progress
priority: medium
labels: [core, architecture, docs]
dependson: []
related: []
created_at: 2026-07-30T10:47:32.234143+00:00
updated_at: 2026-07-30T23:12:24.439973+00:00
---

## 背景

`document/02-architecture.md`（L813-814）は次のように明記している。「判定の純粋ロジックは usagi-core の usecase::workspace_guard にあり、cli/hooks/guard_workspace はその薄い stdin → stdout シム（実 stdin は合成ルートが束ねる）」。

しかし `crates/core/src/usecase/workspace_guard.rs` の `path_escapes_root`（L204-228）は実際には:

- `std::fs::canonicalize(root)` / `std::fs::canonicalize(cwd)`（L206）
- `ancestor.exists()` によるファイルシステム探索（L225）
- `std::fs::canonicalize(existing)`（L226）

を直接呼び出しており、`usagi-core::usecase` 層が実ファイルシステム syscall を直接実行している。呼び出し元 `crates/cli/src/cli/hooks/guard_workspace.rs:135` もこれをそのまま呼ぶだけで、実 IO をポート越しに注入していない。

`document/06-conventions.md` の「テストできないからとロジックを計測対象外に逃がさない。実 IO は引数やジェネリックで注入し、本物の IO は合成ルートで束ねる」という原則、および「`usagi-core` の `domain/` は... git・PTY・端末・ファイル IO 等の重い外部クレートは持ち込まない」に類する usecase 層への期待と、ドキュメントの「pure」という主張が実装と一致していない。

## 対象

いずれかの方針で解消する（実装判断は担当者に委ねる）。

- (A) `path_escapes_root` の `canonicalize` / `exists` をポート（trait）越しに注入し、ドキュメントの「pure」記述と実装を一致させる。
- (B) `path_escapes_root` が意図的にファイルシステム照会を必要とする理由（symlink 解決の正しさに必要、かつ副作用を持たない read-only 照会であるため usecase に留めてよい、等）を `document/02-architecture.md` に明記し、「pure」という表現をより正確な記述に修正する。

いずれの場合も、`document/02-architecture.md` の記述と実装が一致する状態にする（SSoT 原則: ドキュメントに書くのは実装済みの事実のみ）。

## 受入条件

- [ ] `workspace_guard.rs` の実 IO 呼び出しについて、方針 (A) か (B) のいずれかが実施されている。
- [ ] `document/02-architecture.md` の該当記述が実装と一致している。
- [ ] `cargo test -p usagi-core` および `cargo test -p usagi-cli`（guard-workspace hook 経路）が green。
