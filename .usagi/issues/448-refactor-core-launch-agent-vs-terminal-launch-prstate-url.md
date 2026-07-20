---
number: 448
title: refactor(core): launch 語彙（agent vs terminal_launch）と PrState/URL 正規化の二重を統合する
status: todo
priority: medium
labels: [refactor, core, review]
dependson: []
related: []
created_at: 2026-07-20T12:04:12.239547+00:00
updated_at: 2026-07-20T12:04:12.239547+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。

## 根拠（検証済み）

**launch 語彙の重複**（`crates/core/src/domain/`）:

- profile ID 検証規則が同一実装で二重: `terminal_launch.rs:35-39` と `agent/mod.rs:126-130`（`is_ascii_lowercase || digit || '-'`、64 byte 上限）。
- scope: `TerminalLaunchScope`（terminal_launch.rs:56）vs `LaunchScope`（agent/mod.rs:233）。
- plan 検証: `TerminalLaunchPlan`（terminal_launch.rs:124-131）vs `LaunchPlan::new`（agent/mod.rs:319-342）— 空 program / 空 working dir の拒否が同型。
- 同型エラー enum: `TerminalLaunchValidationError`（:177）vs `LaunchValidationError`（agent/mod.rs:389）— InvalidProfileId / InvalidProgram / InvalidWorkingDirectory / InvalidEnvironment を両方が列挙。

**PrState / URL 正規化の重複**:

- `domain/pullrequest/mod.rs:171` と `domain/pr_inventory.rs:135` に**別物の `PrState`** が 2 つ。
- URL 正規化が二重: `pr_inventory.rs:22` `canonicalize()`（scheme 除去 :27-28・github.com 検証・canonical URL 再構築）vs `pullrequest/mod.rs:126` `pr_key()`（`/files`・query・fragment 除去による dedup キー）。同じ dedup 目的の 2 実装。

## 問題

検証規則・状態語彙が二重管理で、片側だけの仕様変更（例: profile ID に大文字許可）が黙って面間差を生む。同名 `PrState` は import 事故の温床。

## 改善案（要検討）

- launch 語彙（ID 検証・scope・plan 検証・エラー）を共通モジュールに抽出し、agent/terminal が共有する。
- `PrState` は canonical 側へ一本化し、もう片方は legacy と明示して段階廃止。URL 正規化は `canonicalize` へ集約する。

## 受け入れ条件

- [ ] 検証規則・scope・エラー enum の重複が解消されている。
- [ ] `PrState` が 1 定義（または legacy 明示付きの移行計画）になり、URL 正規化が 1 実装になる。
- [ ] coverage 100% を維持する。
