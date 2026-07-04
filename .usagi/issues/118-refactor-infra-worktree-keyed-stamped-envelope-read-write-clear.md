---
number: 118
title: refactor(infra): worktree-keyed ストアの stamped-envelope read/write/clear を共通化する
status: todo
priority: high
labels: [refactor, infra, review]
dependson: []
related: []
created_at: 2026-07-04T23:14:15.402066+00:00
updated_at: 2026-07-04T23:14:15.402066+00:00
---

## 背景（なぜ問題か）

`worktree_keyed_store` はパス導出（`dir` / `file_name`（FNV hash）/ `key` / `path_for`）**だけ**を共有し、6 つのストアが「stamp して書く・衝突チェック付きで読む・clear する」という骨格を逐一コピペしている。

- ① stamp して書く: `write_atomic(&Struct{ worktree: key, .. })`
- ② 衝突チェック付き読み: `match read { Ok(Some(f)) if f.worktree==key => .., Ok(Some(_)) => absent, Ok(None) => absent, Err(_) => remove_file + absent }`
- ③ `clear`

特に `open_panes_store::load` と `resume_focus_store::load` はスタンプ欄名（`worktree` vs `workspace`）と payload 型以外ほぼ同一。`agent_live_prompt_store` は同一の衝突チェック arm を 1 ファイル内で 3 回書いている。`open_panes_store::clear` は `path_for` を使わず不整合。

## 対象箇所

- `src/infrastructure/worktree_keyed_store.rs`
- `agent_state_store` / `agent_prompt_store` / `agent_live_prompt_store` / `pr_link_store` / `open_panes_store` / `resume_focus_store` の `read_*` / `write` / `set` / `append` / `save` / `load` / `clear`

## やること

- `WorktreeStamped { fn stamped(&self) -> &str }` 相当のトレイト ＋ `read_ours<T>` / `write_stamped<T>` / `clear` の汎用ヘルパを `worktree_keyed_store` に追加し、各ストアをそれ経由に切り替える。
- **ロック取得の有無は設計差（RMW 中に保持する/しない）なので共通化せず呼び出し側に残す**。
- `open_panes_store::clear` の `path_for` 非使用の不整合も揃える。

## 受け入れ条件

- 6 ストアの衝突判定・破損ファイル削除・stamp 書き込みが 1 実装に集約され、各ストアのロック方針は現状維持。
- 既存テストが緑、カバレッジ 100% 維持。
