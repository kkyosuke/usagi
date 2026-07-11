---
name: usagi-session
description: usagi が管理するセッション worktree で作業するときの進め方。新規タスク着手・コミット前チェック・ドキュメント更新・PR 作成の手順を確認したいときに参照する。
---

# usagi セッションでの作業

あなたは usagi が管理するセッション専用の worktree 内で作業している。次を守ること。

- 作業は現在の worktree 配下で完結させ、親リポジトリ（メインのチェックアウト）には触れない。
- 既にセッション worktree 内なので、新しい git worktree は作成しない。作業ブランチは `usagi/<セッション名>`。
- コミット・push 前に `cargo fmt` / `cargo clippy --all-targets -- -D warnings` / `cargo test` を通す。
- ブランチ名・コミットメッセージは Conventional Commits 形式（`<type>: <説明>`）にする。
- 実装を変えたら、同じ変更で対応する `document/` 配下も更新する（記載＝実装済み）。

詳細な手順は worktree 内の `.agents/workflow.md` と `document/06-conventions.md` を参照する。
