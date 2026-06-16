---
number: 10
title: usagi finish / submit（セッション統合・削除）
status: todo
priority: high
labels: [cli]
dependson: [3]
related: []
created_at: 2026-06-16T23:00:42.707462+00:00
updated_at: 2026-06-16T23:00:42.707462+00:00
---

# `usagi finish`（または `submit`）

## 概要

現在のセッションの変更をメインブランチへ統合し、不要になった worktree を削除する一連のフローを実装します。セッションのライフサイクル（作成 → 作業 → 統合 → 破棄）を完結させる重要コマンドです。

## やること

- 現在のセッションのコミットをメインブランチへ統合（merge）する。
- 統合完了後にセッション（worktree + ブランチ）を自動削除する。
- （オプション）GitHub CLI（`gh`）と連携して Pull Request を作成する。
- 未コミット変更やコンフリクトがある場合は中断して警告する。

## 完了条件

- `usagi finish` でアクティブセッションが main に統合され、worktree が削除される。
- `--pr` 等のオプションで PR 作成まで行える。
- 統合前の安全確認（未コミット変更・コンフリクト）が働く。
