---
number: 49
title: refactor(session): セッション操作の二重実装を解消する（破棄・inspect・dirty 判定）
status: done
priority: medium
labels: [refactor, session]
dependson: []
related: []
created_at: 2026-06-18T22:40:11.804658+00:00
updated_at: 2026-07-04T00:15:30.541142+00:00
---

## 背景

コードレビューで、セッション/worktree オーケストレーションのフローに「同一の責務が 2 実装に分裂している」箇所が複数見つかった。いずれも `usecase/session/` と周辺に散らばっており、片方を直すともう片方が取り残されやすい。

### 1. セッション物理破棄が 2 か所に重複（高）
`session/mod.rs` の `remove` と `reconcile.rs` の `prune_stray` が、ほぼ同一手順（`remove_worktree` → `delete_branch` → `remove_dir_all`）を別実装している。違いは worktree の特定方法（記録済み path か list_worktrees 由来か）と force フラグの有無だけ。

### 2. worktree の inspect が record と sync で別実装（中）
`session/mod.rs` の `record` は worktree ごとに `git::default_branch()` を呼ぶ（worktree 数ぶん git プロセス）。一方 `workspace_state.rs` の `sync` は「リポジトリ単位の性質なので root で一度だけ」解決して使い回す最適化済み。同じ「worktree 群を inspect して `Vec<WorktreeState>` を作る」処理が、片方だけ最適化された 2 実装になっている。

### 3. dirty 判定が 2 系統（中）
`remove` は `git::has_uncommitted_changes()`、`inspect_worktree` は `git::worktree_status().dirty` で同じ「dirty か」を別々の git 呼び出しで判定している。`remove` は記録済み `state.json` に既に算出済みの `status` があるのにそれを使わず再計算する。

### 4. reconcile が create/remove のたびに全ツリー再走査（低）
`create`/`remove` の冒頭で無条件に `reconcile()` を呼び、stray が無い通常ケースでも `source_repos`（全ツリー再帰走査）＋全リポの `list_worktrees` を毎回実行している。

## 改善方針

- 「1 セッション（root + branch + 対象リポ群）を物理破棄する」関数を 1 本化し、`remove`（force 可変）と `prune_stray`（常に force）から呼ぶ。worktree の特定も `list_worktrees` 経由に揃える。
- 「worktree のリストを inspect して `Vec<WorktreeState>` を返す」共通ヘルパを `workspace_state` に切り出し、`record` と `sync` の両方から呼ぶ。`default_branch` はリポジトリ単位で 1 回解決にする。
- dirty 判定の入口を `worktree_status().dirty` に統一する。
- `reconcile` 冒頭で「記録済みセット」と「`.usagi/sessions/` 直下の実ディレクトリ名」を突き合わせ、差分が空なら全走査をスキップして早期 return する。

## 確認方法

- セッション作成・削除の挙動が従来どおりであること（E2E / ユニット）。
- マルチリポでの git プロセス呼び出し回数が削減されていること。
- `cargo fmt` / `cargo clippy --all-targets -- -D warnings` / `cargo test`（カバレッジ 100% 維持）。
