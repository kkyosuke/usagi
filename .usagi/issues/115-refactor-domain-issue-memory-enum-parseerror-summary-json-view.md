---
number: 115
title: refactor(domain): issue/memory の enum 文字列トリオ・ParseError・Summary・JSON view の重複を集約する
status: todo
priority: medium
labels: [refactor, core, review]
dependson: []
related: []
parent: 113
created_at: 2026-07-04T23:13:25.912374+00:00
updated_at: 2026-07-04T23:13:25.912374+00:00
---

## 背景（なぜ問題か）

issue/memory の周辺に、型名以外がほぼバイト一致の雑多なスキャフォールドが並行している。

1. **enum 文字列トリオ**: `IssueStatus` / `IssuePriority` / `MemoryType`（および `usecase/issue/stats.rs` の `GroupBy`）が `as_str` の match ＋ `Display`（`write_str(as_str())`）＋ `FromStr`（`as_str` を鏡写しにした match ＋ `invalid <x>: {other:?}`）というトリオを 4 回反復している。
2. **ParseError**: `ParseIssueError` と `ParseMemoryError` は型名以外バイト単位で同一（newtype + `Display` + `Error` + `From<ParseFrontmatterError>`、各 ~16 行）。
3. **Summary**: `IssueSummary` / `MemorySummary`（body を除き `file` を足した serde 構造）と `summary()` コンストラクタが同型。
4. **JSON view**: `IssueView`/`MemoryView`（body 込み）と `ListedIssueView`/`MemorySummaryView`（`file` 込み）が、いずれも「借用フィールド＋`created_at`/`updated_at` を `to_rfc3339()` で所有 String 化した `#[derive(Serialize)]` 構造＋`From<&Entity>`」という同型。モジュール doc コメントも逐語コピー。差分はフィールド集合と memory の `#[serde(rename = "type")]` のみ。

## 対象箇所

- `src/domain/issue/mod.rs`（`ParseIssueError` / `IssueStatus` / `IssuePriority` / `IssueSummary` / `Issue::summary`）
- `src/domain/memory/mod.rs`（`ParseMemoryError` / `MemoryType` / `MemorySummary` / `Memory::summary`）
- `src/usecase/issue/stats.rs`（`GroupBy`）
- `src/usecase/issue/view.rs`・`src/usecase/memory/view.rs`

## やること

- `as_str` / `Display` / `FromStr` を variant→token とエラー型でパラメタ化した宣言的マクロに集約する。
- 2 つの同一 ParseError を 1 つの共通 newtype（またはマクロ生成）に統合する。
- JSON view の `to_rfc3339` タイムスタンプ String 化と `From<&Entity>` の規約を共通化する（構造体自体はフィールド差のため各エンティティに残してよい）。

## 受け入れ条件

- enum の文字列マッピングトリオがマクロ 1 定義から生成され、ParseError が 1 型に統合され、view のタイムスタンプ変換が 1 か所に集約される。
- 各 enum の parse/round-trip テスト、`--json`/MCP 出力の既存テストが緑。カバレッジ 100% 維持。
