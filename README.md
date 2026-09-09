# usagi

<div align="center">

<pre>
    (\(\&#160;&#160;&#160;&#160;&#160;&#160;&#160;&#160;&#160;&#160;&#160;&#160;&#160;&#160;&#160;&#160;&#160;&#160;&#160;&#160;&#160;&#160;&#160;
   (='-')     ╻ ╻ ┏━┓ ┏━┓ ┏━╸ ╻
  o(_(")(")   ┃ ┃ ┗━┓ ┣━┫ ┃╺┓ ┃
              ┗━┛ ┗━┛ ╹ ╹ ┗━┛ ╹
</pre>

**AI エージェントの並列開発を、session・worktree・terminal ごと束ねる TUI / CLI**

[![Test](https://github.com/KKyosuke/usagi/actions/workflows/test.yml/badge.svg)](https://github.com/KKyosuke/usagi/actions/workflows/test.yml)
[![Coverage](https://github.com/KKyosuke/usagi/actions/workflows/coverage.yml/badge.svg)](https://github.com/KKyosuke/usagi/actions/workflows/coverage.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-2024-orange.svg?logo=rust&logoColor=white)](https://rust-lang.org/)

</div>

`usagi` は、複数の AI エージェントや shell を隔離された Git worktree で動かし、
作業の開始から PR の確認までをひとつの画面で管理するツールです。

## 目次

- [何を解決したいのか](#何を解決したいのか)
- [インストール](#インストール)
- [はじめる](#はじめる)
- [基本概念](#基本概念)
- [ドキュメント](#ドキュメント)
- [開発](#開発)
- [ライセンス](#ライセンス)

## 何を解決したいのか

AI エージェントを並列に使うと、branch、terminal、作業状況、PR が分散しやすくなります。
`usagi` は作業単位を **session** にまとめ、次の問題を解消します。

| 困りごと | `usagi` での扱い |
|---|---|
| 複数の作業が同じ checkout で衝突する | session ごとに独立した worktree を作る |
| Agent と terminal が散らばり、状態を追いにくい | workspace をまたいで TUI から一覧・操作する |
| UI を閉じると長い処理まで止まる | daemon が process を所有し、再接続できる |
| 委譲先や PR までの流れが分断される | session、Agent、差分、PR、note を同じ作業単位で扱う |

対応する Agent は Claude、OpenAI Codex、Sakana AI です。通常の shell も同じ画面で利用できます。
実装範囲と入口面の全体像は [プロジェクト概要](document/01-overview.md) を参照してください。

## インストール

公開 release の installer を利用する方法が最短です。

```bash
curl -fsSL https://raw.githubusercontent.com/KKyosuke/usagi/main/scripts/install.sh | bash
```

```bash
usagi doctor  # 必要なツールと設定を確認
```

対応環境と必要なツール、ソースからのビルド、更新、shell 補完は
[インストールと更新](document/12-installation.md) を参照してください。

## はじめる

対象の Git リポジトリを開きます。引数を省略すると、現在のディレクトリを開きます。

```bash
usagi open /path/to/project
```

TUI が開いたら、次の順に進めます。

1. `+ new session` から作業名と base branch を選ぶ。
2. 作成した session で `agent` または `terminal` を実行する。
3. Diff と PR の状態を確認しながら作業する。

次回からは `usagi` を起動し、Open / Recent から workspace を選べます。
Session Garden では庭と右側の session 一覧から作業状況を確認し、各 Agent を開けます。
画面の詳細は [TUI](document/03-tui.md)、全キーボード操作は
[キーバインド](document/11-keybindings.md) を参照してください。

## 基本概念

| 用語 | 意味 |
|---|---|
| workspace | `usagi` で開く Git リポジトリ |
| session | ひとつの作業と、その worktree・Agent・terminal・差分・PR を束ねる単位 |
| daemon | session と process の状態を所有し、TUI を閉じた後も作業を継続する process |
| Director | root Agent から作業の分解や別 session への委譲を行う画面 |
| [Team](document/10-session-roles.md#catalog) | `none`（なし）/ `hierarchical`（階層型）/ `flat`（フラット）/ `pipeline`（パイプライン）から Agent の委譲構造を選ぶ設定 |

Agent は組み込みの MCP server を通じて session の作成・観測・委譲、issue、memory などを扱えます。
詳しい操作と authority の境界は [MCP サーバ](document/07-mcp.md) を参照してください。

## ドキュメント

| 知りたいこと | ドキュメント |
|---|---|
| 全体像、CLI command | [プロジェクト概要](document/01-overview.md) |
| 対応環境、インストール、更新、shell 補完 | [インストールと更新](document/12-installation.md) |
| 画面、設定、操作 | [TUI](document/03-tui.md) / [キーバインド](document/11-keybindings.md) |
| session と process の lifecycle | [daemon](document/05-daemon.md) |
| Agent 連携、委譲 | [MCP サーバ](document/07-mcp.md) / [session role](document/10-session-roles.md) |
| 環境変数、secret | [環境変数設定](document/09-env.md) |
| コード構成、依存関係 | [アーキテクチャ](document/02-architecture.md) |
| branch、commit、PR、品質基準 | [開発規約](document/06-conventions.md) |

すべての仕様文書は [ドキュメント目次](document/README.md) から参照できます。

## 開発

toolchain は `rust-toolchain.toml` に固定されています。環境構築、開発フロー、品質 gate、PR の手順は
[開発規約](document/06-conventions.md) を参照してください。

## ライセンス

[MIT](LICENSE)
