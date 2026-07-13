# 1. プロジェクト概要

> [ドキュメント目次](README.md) ｜ 次へ → [2. アーキテクチャ](02-architecture.md)

## 目次

- [usagi とは](#usagi-とは)
- [設計](#設計)
- [現在の実装状態](#現在の実装状態)

## usagi とは

`usagi` はセッション・worktree オーケストレータである。リポジトリごとに隔離された
worktree（セッション）を作り、複数の AI エージェント・シェルを並行して走らせ、
issue の委譲から PR の作成・マージまでのループを回す。

## 設計

PTY は daemon が所有し、TUI は daemon が所有する端末へ attach するクライアントとして動作する。
コードの構成と責務は [2. アーキテクチャ](02-architecture.md) を正本とする。

## 現在の実装状態

workspace の構成と各実行面の対応は [2. アーキテクチャ](02-architecture.md) を、画面と操作の詳細は
[3. TUI](03-tui.md) を参照する。主なコマンドは次のとおりである。

| コマンド | 動作 |
|---|---|
| `usagi` | Welcome 画面を対話的に表示する |
| `usagi open [path]` | workspace を登録して開く |
| `usagi config` | Config 画面を表示する |
| `usagi doctor` | Doctor 画面を表示する |
| `usagi update` | GitHub Releases の最新バイナリを導入する |
| `usagi daemon start` | daemon をバックグラウンドで起動する |
| `usagi daemon stop` | daemon を停止する |
| `usagi daemon status` | daemon の状態を表示する |
| `usagi daemon restart` | daemon を再起動する |
| `usagi mcp` | MCP サーバを起動する |
