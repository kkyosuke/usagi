---
number: 210
title: feat(core): v1 の domain（workspace/issue/memory）を usagi-core へ移行する
status: done
priority: high
labels: [core]
dependson: []
related: []
created_at: 2026-07-12T01:15:03.465323+00:00
updated_at: 2026-07-12T01:15:10.216729+00:00
---

## 目的

v2 の `usagi-core` はこれまで `domain/` に `AppInfo` しか持っていなかった。v2 の実装を進める土台として、
v1 の中核エンティティを v2 の domain 層へ移行する。移行対象は本 issue のタイトルどおり **workspace / issue / memory** の 3 エンティティ。

## 対象（v1 → v2）

| v1 | v2 |
|---|---|
| `v1/src/domain/frontmatter.rs` | `crates/core/src/domain/frontmatter.rs` |
| `v1/src/domain/workspace.rs` | `crates/core/src/domain/workspace.rs` |
| `v1/src/domain/issue/{mod,markdown,tests}.rs` | `crates/core/src/domain/issue/{mod,markdown,tests}.rs` |
| `v1/src/domain/memory/{mod,markdown,tests}.rs` | `crates/core/src/domain/memory/{mod,markdown,tests}.rs` |

`frontmatter` は issue / memory の共有基盤（`---` 封筒・リストのエスケープ・timestamp・改行の無害化・`str_enum!` マクロ）なので併せて移行する。

## 方針

- `Issue` / `Memory` の frontmatter 仕様・round-trip 挙動は v1 と等価に保つ（テストごと移行）。
- domain エンティティが使う基盤語彙として `chrono`（時刻）・`serde`（JSON インデックス表現の derive）を
  `[workspace.dependencies]` に追加する。`serde_json` はテスト専用（dev-dependency）。
- v2 の `clippy::pedantic`（`-D warnings`）を満たすため `#[must_use]` と `# Errors` doc を補う。
- `Workspace` は v1 に単体テストが無かったため、coverage 100% を保つテストを新設する。
- 依存ルールの明確化（domain は他 usagi クレート・他層に依存せず、外部は chrono/serde の基盤語彙に限る）を
  `document/02-architecture.md`・`document/06-conventions.md` に反映する。

## 完了条件

- `cargo run`（引数なし / daemon / mcp / 未知コマンド）が従来どおり動く。
- fmt / clippy（pedantic, `-D warnings`）/ full test / coverage 100%（lines・functions）/ Markdown link check が通る。
