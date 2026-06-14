# 6. 開発規約

> [ドキュメント目次](README.md) ｜ ← 前へ [5. 設定](05-settings.md)

`usagi` の開発で守るべき規約。**開発者・AI エージェントの双方**が従う。
プロジェクト全体像は [1. プロジェクト概要](01-overview.md) を参照。

## 目次

- [アーキテクチャ](#アーキテクチャ)
- [技術スタック](#技術スタック)
- [ブランチ名](#ブランチ名)
- [コミットメッセージ](#コミットメッセージ)
- [プルリクエスト](#プルリクエスト)
- [品質チェック（コミット・push 前に必須）](#品質チェックコミットpush-前に必須)
- [Git Hooks（lefthook）](#git-hookslefthook)

## アーキテクチャ

クリーンアーキテクチャの 4 層構成。依存は矢印の方向にのみ許可される。
詳細・モジュール構成は [2. アーキテクチャ](02-architecture.md) を参照。

```
presentation ──> usecase ──> domain
      │              │          ▲
      └──────────────┴──> infrastructure
```

| 層 | 責務 |
|---|---|
| `domain/` | 外部依存のない純粋なエンティティ |
| `usecase/` | ビジネスロジック |
| `infrastructure/` | Git 操作・永続化などの外部連携 |
| `presentation/` | CLI ルーティング・TUI 描画・TUI 内コマンド |

- `domain/` は他層・外部クレートに依存しない。
- 依存方向を逆流させない（例: `domain` から `infrastructure` を参照しない）。

## 技術スタック

- 言語: Rust (edition 2021)
- CLI: clap / TUI: ratatui + crossterm
- 疑似ターミナル: portable-pty + vt100 / Git 操作: git2
- 非同期: tokio / シリアライズ: serde・serde_json

## ブランチ名

`main` または `<type>/<説明>`。

- type: `feat|fix|docs|refactor|perf|test|build|ci|chore`
- 例: `feat/add-doctor-command`
- pre-commit フックで命名規則がチェックされる。

## コミットメッセージ

[Conventional Commits](https://www.conventionalcommits.org/ja/) 形式。`<type>[(scope)][!]: <説明>`。

- type: `feat|fix|docs|style|refactor|perf|test|build|ci|chore|revert`
- 例: `feat: doctor コマンドを追加` / `fix(cli): 引数解析のエラーを修正`
- commit-msg フックでチェックされる。

## プルリクエスト

- タイトルは Conventional Commits 形式に合わせる。
- 本文には「目的 / 変更内容 / テスト・確認方法」を含める。

## 品質チェック（コミット・push 前に必須）

```bash
cargo fmt                                  # フォーマット
cargo clippy --all-targets -- -D warnings  # Lint（警告はエラー扱い）
cargo test                                 # テスト
```

- テストカバレッジ 100% を維持する（CI / lefthook でチェック）。
- 緊急時のフックスキップ: `LEFTHOOK=0 git commit ...` または `--no-verify`（原則使わない）。

## Git Hooks（lefthook）

| フック | 内容 |
|---|---|
| pre-commit | ブランチ名チェック / staged な `.rs` を `cargo fmt` |
| commit-msg | Conventional Commits 形式チェック |
| pre-push | `cargo clippy -- -D warnings` / `cargo test` |
