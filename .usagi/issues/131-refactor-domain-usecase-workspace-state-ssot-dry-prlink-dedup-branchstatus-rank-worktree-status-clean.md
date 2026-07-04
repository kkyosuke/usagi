---
number: 131
title: refactor(domain/usecase): workspace_state 周辺の SSoT/DRY 小整理（PrLink dedup 一本化・BranchStatus 文字列/rank 導出・worktree_status clean フォールバック集約）
status: todo
priority: low
labels: [refactor, core, review]
dependson: []
related: []
created_at: 2026-07-04T23:17:34.080364+00:00
updated_at: 2026-07-04T23:17:34.080364+00:00
---

## 背景（なぜ問題か）

`workspace_state` 周辺に、小粒だが放置するとドリフトする SSoT/DRY の綻びが 3 つある。

1. **PrLink の URL dedup が二重定義**: `domain/workspace_state.rs` の `PrLink::aggregate` が「同一 `url` が既にあれば push しない」dedup を実装しているのに、`usecase/workspace_state.rs` の `fold_pr_links` が同じ `!x.iter().any(|p| p.url == pr.url)` を再実装している。
2. **BranchStatus のワイヤ文字列・rank 序数が二重管理**: `BranchStatus` は `#[serde(rename_all = "snake_case")]` で `"new"`/`"dirty"`/`"local"`/`"pushed"`/`"synced"` を生成する一方、手書きの `as_str` が同じ 5 文字列を別途定義。variant 名を変えると serde 表現と `as_str` が黙って乖離しうる。`rank` の 0–4 も enum 宣言順を手書き序数で写している。
3. **clean な WorktreeStatus フォールバック重複**: `usecase/update.rs` の `update_default` と `propagate` の双方が `git::worktree_status(x).unwrap_or(git::WorktreeStatus { head: String::new(), branch: None, upstream: None, dirty: false })`（非 git パスは clean 扱い）を逐語コピーしている。

## 対象箇所

- `src/domain/workspace_state.rs`（`PrLink::aggregate`、`BranchStatus::as_str`/`rank`）
- `src/usecase/workspace_state.rs`（`fold_pr_links`）
- `src/usecase/update.rs`（`update_default`/`propagate`）

## やること

- `fold_pr_links` を `PrLink::aggregate`（または domain 側の dedup ヘルパ）経由に統一する。
- `BranchStatus::as_str` を serde 表現と機械的に一致させる（あるいは片方から導出する／乖離検出テストを足す）。`rank` は宣言順から導出できないか検討する。
- clean フォールバックを小ヘルパ（例 `status_or_clean(path)`）に抽出する。

## 受け入れ条件

- URL dedup と WorktreeStatus clean フォールバックの実装がそれぞれ 1 か所になる。BranchStatus の文字列表現の正本が 1 つになり乖離を検出できる。
- 既存テストが緑、カバレッジ 100% 維持。
