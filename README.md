# usagi

AI Agent のワークフローを管理する TUI/CLI ツール。[usagi.ai](https://github.com/KKyosuke/usagi.ai) の設計を引き継いだ再構築プロジェクトです。

## Prerequisites

- Rust (Cargo)
- Git

## Installation

### From Source

```bash
cargo install --path .
```

## Quick Start

依存ツールの確認:

```bash
cargo run -- doctor
```

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
document/              # プロジェクトドキュメント
```

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

## License

MIT
