# usagi

`usagi` をゼロから作り直す v2 の開発ツリー。リポジトリルートがそのまま v2 の
Cargo パッケージであり、旧実装（v1）は [v1/](v1/README.md) に独立したプロジェクト
として退避してある。

| 場所 | 内容 |
|---|---|
| `/`（ルート） | v2 の実装。CI（fmt / clippy / test / coverage 100%）の対象 |
| `v1/` | 退避した旧実装。仕様ドキュメント（`v1/document/`）ごと独立した Cargo プロジェクトで、ルートのビルド・CI の対象外 |

## 構成（v2）

クリーンアーキテクチャの 4 層構成（`presentation → usecase → domain ← infrastructure`）。

```
.
├── Cargo.toml          # edition 2024 / clippy (all + pedantic) を warn で有効化
└── src/
    ├── main.rs         # 合成ルート。実 IO をここで束ねる
    ├── lib.rs          # ライブラリクレートのモジュール定義
    ├── domain/         # ビジネスルール。他層・外部クレートに依存しない
    ├── usecase/        # domain を組み合わせた操作
    ├── infrastructure/ # 外部世界（git / FS / プロセス）との接続
    └── presentation/   # CLI / TUI の入出力表現。実 IO は注入で受け取る
```

## 開発

リポジトリルートで実行する。

| 目的 | コマンド |
|---|---|
| ビルド確認 | `cargo check --all-targets` |
| フォーマット確認 | `cargo fmt --all -- --check` |
| Lint | `cargo clippy --all-targets -- -D warnings` |
| テスト | `cargo test --quiet` |
| 実行 | `cargo run` |

## 方針

- 実 IO（標準入出力・サブプロセス・端末・PTY）は引数やジェネリックで注入し、
  本物の IO は `src/main.rs` で束ねる。ロジックはすべてユニットテスト可能に保つ
  （テストカバレッジ 100% を維持する）。
- 依存クレートは必要になった時点で追加する（v1 の依存を先回りで持ち込まない）。
- コミット・PR・品質チェックの規約はリポジトリ共通の
  [v1/document/06-conventions.md](v1/document/06-conventions.md) に従う（v1/v2 共通で有効）。

## v1 を使う・参照する

退避した v1 は `v1/` 配下でそのまま完結してビルドできる。

```bash
cd v1
cargo build --release
```

機能・画面・データ仕様のリファレンスは [v1/README.md](v1/README.md) と
[v1/document/](v1/document/README.md) を参照する。
