# CLAUDE.md

`usagi` で作業する際は、`.agents/` 配下の手順書に従うこと。

## 手順書

@.agents/workflow.md
@.agents/conventions.md

## 要点

- **新規作業**: worktree 作成 → 開発 → ドキュメント更新 → PR 作成。
- **追加修正**: 開発 → ドキュメント更新 → PR タイトル・概要の更新。
- コミット・push 前に `cargo fmt` / `cargo clippy --all-targets -- -D warnings` / `cargo test` を通す。
- ブランチ名・コミットメッセージは Conventional Commits 形式。

詳細は上記の手順書を参照。
