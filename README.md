# usagi

`usagi` をゼロから作り直す v2 の開発ツリー。リポジトリルートがそのまま v2 の
Cargo パッケージであり、旧実装（v1）は [v1/](v1/README.md) に独立したプロジェクト
として退避してある。

| 場所 | 内容 |
|---|---|
| `/`（ルート） | v2 の実装。CI（fmt / clippy / test / coverage 100%）の対象 |
| `v1/` | 退避した旧実装。仕様ドキュメント（`v1/document/`）ごと独立した Cargo プロジェクトで、ルートのビルド・CI の対象外 |

## 構成（v2）

「TUI 面 / daemon 面 / 入口面（CLI・MCP）/ 共通（common）」の 4 クレート＋合成ルートの
Cargo workspace。各クレート内はクリーンアーキテクチャの依存方向を守る（正本は
[document/02-architecture.md](document/02-architecture.md)）。

```
.
├── Cargo.toml          # workspace ルート ＋ 配布バイナリ usagi（bin）のパッケージ
├── src/
│   └── main.rs         # 合成ルート。実 IO をここで束ね、各面へ dispatch する
└── crates/
    ├── core/           # usagi-core: 共通の domain / usecase / 共有 infrastructure
    ├── cli/            # usagi-cli: 入口面。CLI サブコマンドと MCP サーバ（usagi-core にのみ依存）
    ├── daemon/         # usagi-daemon: daemon 面（usagi-core にのみ依存）
    └── tui/            # usagi-tui: TUI 面（usagi-core にのみ依存）
```

## 開発

リポジトリルートで実行する。

| 目的 | コマンド |
|---|---|
| ビルド確認 | `cargo check --workspace --all-targets` |
| フォーマット確認 | `cargo fmt --all -- --check` |
| Lint | `cargo clippy --workspace --all-targets -- -D warnings` |
| テスト | `cargo test --workspace --quiet` |
| 実行 | `cargo run -- [args]` |

CLI から選ぶ TUI 起動画面を含む現在のコマンド動作は
[v2 の実装状態](document/01-overview.md#現在の実装状態)を参照する。

## 方針

- 実 IO（標準入出力・サブプロセス・端末・PTY）は引数やジェネリックで注入し、
  本物の IO は `src/main.rs` で束ねる。ロジックはすべてユニットテスト可能に保つ
  （テストカバレッジ 100% を維持する）。
- 依存クレートは必要になった時点で追加する（v1 の依存を先回りで持ち込まない）。
- コミット・PR・品質チェックの規約は [document/06-conventions.md](document/06-conventions.md) に従う。

## v1 を使う・参照する

退避した v1 は `v1/` 配下でそのまま完結してビルドできる。

```bash
cd v1
cargo build --release
```

機能・画面・データ仕様のリファレンスは [v1/README.md](v1/README.md) と
[v1/document/](v1/document/README.md) を参照する。
