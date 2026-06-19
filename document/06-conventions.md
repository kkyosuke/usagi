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
- [ドキュメント規約](#ドキュメント規約)
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
- **例外**: usagi のセッション worktree（`.usagi/sessions/<name>/`）はブランチ名がセッション名（`<name>`）になる。セッション名は `/` を含められない（[04-orchestration.md](04-orchestration.md)）ため `<type>/<説明>` を満たせず、pre-commit フックはこの worktree 内のコミットを命名規則チェックの対象外にする。

## コミットメッセージ

[Conventional Commits](https://www.conventionalcommits.org/ja/) 形式。`<type>[(scope)][!]: <説明>`。

- type: `feat|fix|docs|style|refactor|perf|test|build|ci|chore|revert`
- 例: `feat: doctor コマンドを追加` / `fix(cli): 引数解析のエラーを修正`
- commit-msg フックでチェックされる。

## プルリクエスト

- タイトルは Conventional Commits 形式に合わせる。
- 本文には「目的 / 変更内容 / テスト・確認方法」を含める。

## ドキュメント規約

`document/` 配下・`README.md`・`.agents/` を書くときのルール。**実装を変えたら同じ PR で対応ドキュメントも更新する**
（[ワークフロー](../.agents/workflow.md) ステップ 3）のが大前提で、その上で次を守る。

### 記載＝実装済み

- **現在のビルドで動作する仕様だけを書く**。未実装・予定の機能、「coming soon」「移植予定」、`✅` / `🚧`
  などの実装状況マーカーは置かない（あると「どこまで本当か」を読者が判断できなくなる）。
- 記述は**現在形・断定形**で書く（「実装します」「移植していきます」ではなく「〜する」「〜である」）。
- ロードマップを残したい場合は、本仕様ドキュメントと混ぜず別管理にする（issue ストア `.usagi/issues/`）。

### SSoT（単一情報源）

- **1 つの事実は 1 か所だけに書く**。重複しそうな内容は**正本**を 1 つ決め、他のドキュメントはそこへリンクする。
  正本側には「ここが正本」と明記する。
- 主な正本の所在:

  | 内容 | 正本 |
  |---|---|
  | 技術スタック・アーキテクチャ（層・依存・モジュール） | [02-architecture.md](02-architecture.md) |
  | コマンドの構文・役割 | [03-commands/](03-commands/README.md) |
  | セッション・worktree のライフサイクル概念 | [04-orchestration.md](04-orchestration.md) |
  | 設定項目の意味・既定値・変更方法 | [05-settings.md](05-settings.md) |
  | 開発規約 | 本書（06-conventions.md） |
  | 画面の見た目・モード・キー操作 | [design/](design/README.md) |
  | 永続化ファイルの保存フォーマット | [data/](data/README.md) |

- **層をまたいで書かない**。例: `data/` は保存フォーマット（バイト列）だけを書き、設定の意味は `05-settings.md`、
  画面 UI は `design/` に書く。

### 構造

- **1 ファイル = 1 トピック**。番号付きファイル（`01-` …）＋系統ごとのサブディレクトリ（`design/` / `data/` /
  `03-commands/`）で構成し、各ディレクトリに目次となる `README.md` を置く。
- ファイルが長くなりすぎたら分割する（目安: 1 ファイル 300 行を超えたら要検討）。実装の内部詳細（コード構造・
  拡張点）は仕様ドキュメントに書かず、`02-architecture.md` か該当コードへのポインタにとどめる。

### ナビゲーション

- 各ファイルの先頭に `> [目次] ｜ ← 前へ […] ｜ 次へ → […]` のパンくずを置く。
- 章の冒頭に**目次**（`##` 見出しへのアンカーリンク）を置く。

### 可読性

- **列挙・対照は散文でなく表**で、**フロー・階層は ASCII 図**で示す。
- **テーブルのセルに段落を詰め込まない**。コマンドごとの詳細な挙動は、表の下に per-command の節を設けて書く。
- 型表記は `string?`（Optional）のように統一する。

### リンク

- ディレクトリ内・ディレクトリ間とも**相対リンク**を使う。リンク切れと**見出しアンカー**（`#見出し`）は
  CI（[markdown-link-check](#cigithub-actions)、lychee）で検証されるため、目次・アンカーは見出しと一致させる
  （不一致は CI 失敗）。
- ソースコードは `path:line` で固定参照せず、該当する仕様ドキュメントへリンクする（行番号は陳腐化しやすい）。

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
