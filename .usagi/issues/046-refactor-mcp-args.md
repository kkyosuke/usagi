---
number: 46
title: refactor(mcp): Args 構造体の重複を解消し共通ヘルパを集約する
status: todo
priority: medium
labels: [refactor, mcp]
dependson: []
related: []
created_at: 2026-06-18T22:41:11.352837+00:00
updated_at: 2026-06-18T22:41:11.352837+00:00
---

## 背景

MCP サーバ層に「usecase 構造体と 1:1 で重複した Args 構造体」と「各モジュールに同一コピーされたヘルパ」が散在し、フィールド追加時の追従点が増えている。JSON-RPC framing の `McpService` 抽象自体は良い設計で、これは差分側の整理。

### 1. `*Args` が usecase 構造体とほぼ 1:1 重複（中）
usecase 側に `NewIssue`/`IssueChanges`/`IssueFilter`（memory も同様）が既にあるのに、`mcp/issue/mod.rs` で `CreateArgs`/`ListArgs`/`SearchArgs`/`UpdateArgs` を再定義し、手作業のフィールドコピー（`.filter()`/`.changes()`）を書いている。`ListArgs` と `SearchArgs` は `query` の有無以外ほぼ同一で `.filter()` も重複。`mcp/memory.rs` も同型。CLI 層（`cli/issue/mod.rs` の List/Search）でも同じ `IssueFilter` 組み立てを反復している。`SearchArgs::filter(&self)` と `ListArgs::filter(self)` の非対称（clone）も派生問題。

### 2. `parse_args` / `to_pretty` が 3 コピー（低）
`parse_args` が `mcp/issue/mod.rs`・`mcp/memory.rs`・`mcp/session.rs` に同一実装で 3 つ、`to_pretty` も `mcp/issue/json.rs`・`mcp/memory.rs`・`mcp/session.rs` に 3 つある。

### 3. 統合 `usagi` サーバの tool マージが 2 段の手動結合（低）
`usagi.rs`（issue+session）と `issue/mod.rs`（issue+memory）で `as_array_mut().expect(...).extend(...)` の配列ミューテーションが 2 段にネストし、ルーティングも `SESSION_TOOLS.contains` と `memory::tool_names().contains` で不揃いに振り分けている。

## 改善方針

- usecase 構造体（`IssueFilter`/`IssueChanges`/`NewIssue`・memory 版）に `#[derive(Deserialize)]` + `#[serde(default)]` を付け、MCP 層は直接デシリアライズする。少なくとも `SearchArgs { query, #[serde(flatten)] filter }` の形で `.filter()` 重複を 1 本化。CLI 側も `#[command(flatten)]` + `From<...> for IssueFilter` で 1 回に。
- `parse_args` / `to_pretty` を `presentation/mcp/mod.rs` に 1 本ずつ置き各サーバから使う。
- サブサーバを `Vec<Box<dyn McpService>>`（または `(tool_names, schemas, call)` レジストリ）にし、`UsagiMcpServer` が 1 段でループして schema を `flat_map`・call を「最初に名前を持つサーバへ委譲」する形にする。`tool_names()`/`TOOL_NAMES` の表現も統一。

## 確認方法

- MCP の各ツール（issue/memory/session）の入出力が従来どおりであること（MCP テスト）。
- `cargo fmt` / `cargo clippy --all-targets -- -D warnings` / `cargo test`（カバレッジ 100% 維持）。
