# usagi

[![Test](https://github.com/KKyosuke/usagi/actions/workflows/test.yml/badge.svg)](https://github.com/KKyosuke/usagi/actions/workflows/test.yml)
[![Coverage](https://github.com/KKyosuke/usagi/actions/workflows/coverage.yml/badge.svg)](https://github.com/KKyosuke/usagi/actions/workflows/coverage.yml)
[![Release](https://img.shields.io/github/v/release/KKyosuke/usagi?display_name=tag)](https://github.com/KKyosuke/usagi/releases)
[![License](https://img.shields.io/github/license/KKyosuke/usagi)](LICENSE)

AI エージェントのセッションと Git worktree を管理する、Rust 製の TUI / CLI です。隔離された作業環境で
複数のエージェントやシェルを並行して動かし、issue から PR までの作業を支援します。

## Install

macOS または Linux では、最新リリースを次の one-liner でインストールできます。

```sh
curl -fsSL https://raw.githubusercontent.com/KKyosuke/usagi/main/scripts/install.sh | bash
```

バイナリは `~/.usagi/bin/usagi` に配置されます。まだ PATH に含まれていない場合は、シェル設定へ追加してください。

```sh
export PATH="$PATH:$HOME/.usagi/bin"
```

## Quick start

```sh
usagi
usagi open .
```

引数なしで Welcome 画面を開きます。`usagi open .` はカレントディレクトリを workspace として開きます。

| コマンド | 用途 |
|---|---|
| `usagi` | Welcome TUI を開く |
| `usagi open [path]` | workspace を登録して開く |
| `usagi config` | 設定画面を開く |
| `usagi doctor` | 必要ツールの導入状況を確認する |
| `usagi daemon start` | バックグラウンド daemon を起動する |
| `usagi update` | 最新リリースへ更新する |
| `usagi --help` | すべてのコマンドを表示する |

## Development

```sh
cargo check --workspace --all-targets
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --quiet
```

設計と開発規約は [document/README.md](document/README.md) を参照してください。
