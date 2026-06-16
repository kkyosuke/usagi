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
- [CI（GitHub Actions）](#cigithub-actions)
- [リリース](#リリース)

## アーキテクチャ

クリーンアーキテクチャの 4 層構成（`presentation → usecase → domain ← infrastructure`）。
**層の責務・依存方向・モジュール構成・依存ルールは [2. アーキテクチャ](02-architecture.md) が正本**。開発時は次の 2 点だけ守ること。

- `domain/` は他層・外部クレートに依存しない。
- 依存方向を逆流させない（例: `domain` から `infrastructure` を参照しない）。

## 技術スタック

使用クレートと用途の一覧は [2. アーキテクチャ#技術スタック](02-architecture.md#技術スタック) を正本とする（Rust 2021 / clap / console + crossterm / portable-pty + vt100 / git CLI / serde）。

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

## CI（GitHub Actions）

`main` への push / PR をトリガーに自動チェックが走る。

| ファイル | トリガー | 役割 |
|---|---|---|
| `.github/workflows/test.yml` | `main` への push / PR | `cargo fmt --check` / `clippy` / `build` / `test` |
| `.github/workflows/coverage.yml` | PR | カバレッジ計測・PR コメント・100% 未満で失敗 |
| `.github/workflows/markdown-link-check.yml` | `.md` 変更を含む push / PR | Markdown のリンク切れ（相対リンク・アンカー・外部 URL）を [lychee](https://github.com/lycheeverse/lychee) で検証 |
| `.github/workflows/enforce-pr-base.yml` | PR | ベースブランチが `main` であることを強制 |

- リンクチェックの設定（リトライ・除外・アンカー検証）は `lychee.toml` に集約する。ファイル内の見出しアンカー（`#見出し`）も検証するため、目次リンク等が見出しと一致していないと失敗する。

## リリース

リリースは **`Cargo.toml` の `version` 変更を起点に自動化**されている。手動でタグを切る必要はない。

### 手順

1. リリースしたい変更を `main` にマージする。
2. `Cargo.toml` の `version` を上げる PR を作成し `main` にマージする（例: `0.1.0` → `0.2.0`）。
3. 以降は自動で進む:
   - `auto-release.yml` が `main` への `Cargo.toml` 変更 push を検知し、version が前コミットから変わっていれば `v<version>` タグを対象にリリースを起動する。
   - reusable な `release.yml` が呼ばれ、4 プラットフォーム（Linux / macOS amd64・arm64 / Windows）のバイナリをビルドし、`v<version>` タグと GitHub Release を作成して成果物を添付する。

> version が変わらない push、または同名タグが既に存在する場合はスキップされる。

### リリースノート

- リリースノートは **GitHub Models（AI）** が前回タグからのコミットログをもとに日本語で自動生成する（`release.yml` の `release-notes` ジョブ）。
- AI 生成に失敗した場合はコミットログをそのまま本文にフォールバックする。
- あわせて GitHub 標準の自動生成ノート（PR 一覧）も付与される。

### ワークフロー構成

| ファイル | トリガー | 役割 |
|---|---|---|
| `.github/workflows/auto-release.yml` | `main` への `Cargo.toml` 変更 push | version 変更を検知し `release.yml` を呼び出す |
| `.github/workflows/release.yml` | `v*` タグ push / `workflow_call` | リリースノート生成・ビルド・GitHub Release 作成 |

`release.yml` は `v*` タグの手動 push でも従来どおり動作する（`workflow_call` は追加のトリガー）。
