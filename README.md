# usagi

[![Test](https://github.com/KKyosuke/usagi/actions/workflows/test.yml/badge.svg)](https://github.com/KKyosuke/usagi/actions/workflows/test.yml)
[![Release](https://github.com/KKyosuke/usagi/actions/workflows/release.yml/badge.svg)](https://github.com/KKyosuke/usagi/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-2021-orange.svg?logo=rust)](https://www.rust-lang.org/)

AI Agent のワークフローを管理する TUI/CLI ツール。[usagi.ai](https://github.com/KKyosuke/usagi.ai) の設計を引き継いだ再構築プロジェクトです。

## Prerequisites

- Rust (Cargo)
- Git

## Installation

### One-liner (macOS / Linux)

ビルド済みバイナリを 1 行でダウンロードしてインストールできます:

```bash
curl -fsSL https://raw.githubusercontent.com/KKyosuke/usagi/main/scripts/install.sh | bash
```

`~/.usagi/bin` にインストールされます。表示される案内に従って PATH を通してください:

```bash
export PATH="$PATH:$HOME/.usagi/bin"
```

プラットフォームを指定してアーカイブから直接インストールすることもできます:

#### macOS (Apple Silicon)
```bash
curl -L https://github.com/KKyosuke/usagi/releases/latest/download/usagi-macos-arm64.tar.gz | tar -xz && ./install.sh && rm install.sh
```

#### macOS (Intel)
```bash
curl -L https://github.com/KKyosuke/usagi/releases/latest/download/usagi-macos-amd64.tar.gz | tar -xz && ./install.sh && rm install.sh
```

#### Linux (AMD64)
```bash
curl -L https://github.com/KKyosuke/usagi/releases/latest/download/usagi-linux-amd64.tar.gz | tar -xz && ./install.sh && rm install.sh
```

#### Windows (AMD64)
[Releases](https://github.com/KKyosuke/usagi/releases) ページから `usagi-windows-amd64.zip` をダウンロードして展開し、Git Bash で `install.sh` を実行するか、バイナリを手動で PATH に追加してください。

### From Source

```bash
cargo install --path .
```

## Quick Start

依存ツール・通知・設定ストレージの健全性確認:

```bash
cargo run -- doctor
```

`git` / `bash` の導入状況に加え、`usagi hop` のデスクトップ通知が利用可能か、設定ストレージが読めるかを `ok` / `warn` / `missing` で表示します。

### ワークスペースで開発を始める

```bash
cd <project>      # usagi init 済みのプロジェクト
cargo run -- hop  # TUI を起動
```

ワークスペースを開いたあと、コマンドモード（`:` で起動）から以下を実行できます。

```text
:session new feature-x   # .usagi/worktree/feature-x/ にセッション（worktree）を作成
:session list            # セッション一覧
:terminal                # 選択中の worktree でシェルを右ペインに埋め込み起動（Ctrl-O でデタッチ）
:agent                   # terminal を開いて設定中の Agent CLI（既定 claude）を起動（実質 terminal → claude）
```

作成した worktree は左ペインに表示されます。目的の worktree を選んで `terminal` を実行すると、左ペインの一覧を表示したまま、その worktree を作業ディレクトリとしたシェルが**右ペインにライブで埋め込まれます**。そこで `claude` などの AI エージェントを起動して開発できます。シェルを抜けるには `Ctrl-O`（デタッチ）か、シェル側で `exit` してください。`:agent` を使えば、この「`terminal` を開いて Agent CLI を起動する」操作を 1 コマンドで行えます（起動する Agent CLI は Config 画面・ローカル設定で選べます）。

### タスクを管理する

プロジェクトのタスクは `usagi issue` で管理できます（`<repo>/.usagi/issues/` に frontmatter 付き markdown で保存。git で共有されます）:

```bash
cargo run -- issue create --title "ログイン画面" --priority high --depends-on 1
cargo run -- issue list            # 着手可能(ready)/ブロック中を可視化
cargo run -- issue list --ready    # いま着手できる issue だけ
cargo run -- issue update 2 --status done
```

`list` / `search` は依存（`--depends-on`）がすべて `done` になった「着手可能」な issue を `ready` と表示し、ブロック中のものには未達の依存番号を併記します。詳細は [document/03-commands/01-cli.md](document/03-commands/01-cli.md#usagi-issue)。

## Project Structure

クリーンアーキテクチャを採用しています（domain → usecase → infrastructure ← presentation）。

```
src/
├── main.rs            # CLI エントリポイント (clap)
├── lib.rs             # モジュール宣言
├── domain/            # ビジネスエンティティ（外部依存なし）
├── usecase/           # ビジネスロジック
├── infrastructure/    # 永続化・Git 操作などの外部連携
└── presentation/      # CLI/TUI インターフェース
tests/                 # 統合テスト
document/              # プロジェクトドキュメント（仕様・規約。開発者 + AI 向け）
.agents/               # AI エージェント固有の作業手順（CLAUDE.md/GEMINI.md から参照）
```

> `document/` は開発者・AI の双方が読むプロジェクト仕様と[開発規約](document/06-conventions.md)、`.agents/` は AI に守らせる作業手順（worktree 運用・PR の進め方など）を置きます。仕様の目次は [document/README.md](document/README.md) を参照してください。

## Development

```bash
cargo build          # ビルド
cargo test           # テスト
cargo fmt            # フォーマット
cargo clippy         # Lint
```

### Git Hooks

[lefthook](https://lefthook.dev) で Git hooks を管理しています。クローン後に一度だけ実行してください:

```bash
brew install lefthook   # macOS 以外: npm i -g lefthook など
lefthook install
```

| フック | 内容 |
| --- | --- |
| pre-commit | ブランチ名チェック / staged な `.rs` を `cargo fmt` で自動フォーマット |
| commit-msg | [Conventional Commits](https://www.conventionalcommits.org/ja/) 形式のチェック |
| pre-push | `cargo clippy -- -D warnings` / `cargo test`（CI と同条件） |

- ブランチ名: `<type>/<説明>`（例: `feat/add-doctor-command`）
- コミットメッセージ: `<type>[(scope)][!]: <説明>`（例: `feat: doctor コマンドを追加`）
- type: `feat` `fix` `docs` `style` `refactor` `perf` `test` `build` `ci` `chore` `revert`
- 緊急時のスキップ: `LEFTHOOK=0 git commit ...` または `git commit --no-verify`

### Release

リリースは `Cargo.toml` の `version` 変更を起点に自動化されています。`version` を上げる変更を `main` にマージすると、`v<version>` タグと GitHub Release が自動作成され、各プラットフォーム向けバイナリが添付されます。リリースノートは GitHub Models（AI）がコミットログから自動生成します。詳細は [document/06-conventions.md#リリース](document/06-conventions.md#リリース) を参照してください。

## License

MIT
