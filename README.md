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

## License

MIT
