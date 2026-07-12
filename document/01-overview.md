# 1. プロジェクト概要

> [ドキュメント目次](README.md) ｜ 次へ → [2. アーキテクチャ](02-architecture.md)

## 目次

- [usagi とは](#usagi-とは)
- [v2 の位置づけ](#v2-の位置づけ)
- [v1 との関係](#v1-との関係)
- [現在の実装状態](#現在の実装状態)

## usagi とは

`usagi` はセッション・worktree オーケストレータである。リポジトリごとに隔離された
worktree（セッション）を作り、複数の AI エージェント・シェルを並行して走らせ、
issue の委譲から PR の作成・マージまでのループを回す。

## v2 の位置づけ

v2 は usagi のフルリライトである。v1 で決定した「PTY 所有を daemon に移し、TUI は
daemon が所有する端末に attach するクライアントになる」設計
（[v1/document/proposals/02-daemon.md](../v1/document/proposals/02-daemon.md)）を
最初から前提にした構造で作り直す。コードの構成は
[2. アーキテクチャ](02-architecture.md) を正本とする。

## v1 との関係

| 場所 | 内容 |
|---|---|
| `/`（ルート） | v2 の実装。ビルド・CI（fmt / clippy / test / coverage 100%）の対象 |
| `v1/` | 退避した旧実装。仕様ドキュメント（`v1/document/`）ごと独立した Cargo プロジェクトで、ルートの workspace から exclude されている |

- 配布 version はルート `Cargo.toml` が v1 の version を引き継ぎ、v2 として最初に
  リリースするときに bump する（[6. 開発規約#リリース](06-conventions.md#リリース)）。
- v1 は `v1/` 配下で従来どおり単体ビルドできる。

## 現在の実装状態

v2 は workspace の骨組み（[2. アーキテクチャ](02-architecture.md)）と、それを検証する
最小の実行面だけを持つ。

| コマンド | 動作 |
|---|---|
| `usagi` | 起動バナー（`usagi v<version>`）を表示する |
| `usagi daemon` | daemon 面の ready 行（`usagi v<version> daemon ready`）を表示する |

機能は v1 から順次再実装する。実装タスクは issue ストア（`.usagi/issues/`）で追跡する。
