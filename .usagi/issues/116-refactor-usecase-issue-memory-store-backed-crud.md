---
number: 116
title: refactor(usecase): issue/memory の store-backed CRUD を共通化する
status: todo
priority: high
labels: [refactor, core, review]
dependson: [114]
related: []
parent: 113
created_at: 2026-07-04T23:13:42.321956+00:00
updated_at: 2026-07-04T23:13:42.321956+00:00
---

## 背景（なぜ問題か）

`usecase/issue/mod.rs` と `usecase/memory/mod.rs` は、store を挟んだ CRUD オーケストレーションが構造的にほぼ同一で、逐語コピーになっている。

- `get`: `Store::new(root).read(key)`
- `search`: 空クエリ→`list` へ短絡する複数行コメントまで逐語コピー → `scan_lenient` → `search::fold_query` → `matches_folded` → `map(summary)`
- `update`: `store.lock()` → `let Some(mut x)=store.read()? else { return Ok(None) }`（lost-update コメントも同文）→ フィールドごとの `if let Some(f)=changes.f { x.f=f }` → `updated_at=Utc::now()` → `write_locked`
- `delete`: 1 行

さらに `NewIssue`/`NewMemory`、`IssueChanges`/`MemoryChanges`（`is_empty()` が全 `is_none()` の AND）、`IssueFilter`/`MemoryFilter`（`matches()`）も同型。全文検索プリミティブ（`usecase/search.rs`）は既に共通化済みで、その外側のラッパだけが重複している。

## 対象箇所

- `src/usecase/issue/mod.rs`（`get` / `search` / `update` / `delete` / `IssueChanges` / `IssueFilter`）
- `src/usecase/memory/mod.rs`（`get` / `search` / `update` / `delete` / `MemoryChanges` / `MemoryFilter`）

## やること

- エンティティ・キー型（`u32` vs `&str`）・`Changes`・`Filter` をパラメタ化した store-backed CRUD 層（`Record`/`Filter` トレイト＋ジェネリック `get`/`update`/`delete`/`search`）を導入する。
- issue 固有の readiness 注釈（`annotate`）と memory 固有の `sort_newest_first` は後処理フックとして残す。

## 受け入れ条件

- issue/memory 双方の `get`/`update`/`delete`/`search` が共通実装に集約され、各モジュールにはフィールド適用・後処理・型定義だけが残る。
- 既存の全テストが緑、カバレッジ 100% 維持。

## 補足

親 #113。domain の封筒共通化（#114）の後に着手すると綺麗にはまるため #114 に dependson。
