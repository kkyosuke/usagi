# GEMINI.md

`usagi` で作業する際は、`.agents/` 配下の手順書に従うこと。

## 手順書

@.agents/workflow.md
@document/06-conventions.md

## 要点

- **新規作業**: 隔離環境を用意 → 開発 → ドキュメント更新 → PR 作成。
  - usagi セッション内（`.usagi/sessions/<name>/`）で起動しているなら**すでに worktree 内なので新規作成しない**。`main` で直接作業するときだけ worktree を切る。
- **追加修正**: 開発 → ドキュメント更新 → PR タイトル・概要の更新。
- コミット・push 前に `cargo fmt` / `cargo clippy --all-targets -- -D warnings` / `cargo test` を通す。
- ブランチ名・コミットメッセージは Conventional Commits 形式。

詳細は上記の手順書を参照。
