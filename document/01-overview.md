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
最小の実行面を持つ。CLI が TUI の起動要求を返し、合成ルートが TUI の初期画面へ
変換するため、入口面と TUI 面のクレート間に直接依存は生じない。以下の表が
コマンドから起動面への対応の正本である。

| コマンド | 動作 |
|---|---|
| `usagi` / `usagi hop` | Welcome 画面を対話的に表示する（`↑↓` で選択移動、`q` / Esc / Ctrl-C で終了） |
| `usagi open [path]` | Workspace TUI を起動画面に選ぶ。`path` 省略時はカレントディレクトリを使う |
| `usagi config` | Config TUI を起動画面に選ぶ |
| `usagi doctor` | Doctor TUI を起動画面に選ぶ |
| `usagi version` / `usagi --version` | 配布 version を表示する |
| `usagi daemon` | daemon 面の ready 行（`usagi v<version> daemon ready`）を表示する |
| `usagi mcp` | 入口面（MCP）の ready 行（`usagi v<version> mcp ready`）を表示する |
| `usagi <不正な引数>` | clap の利用方法エラーを stderr に表示し、非 0 で終了する |

Welcome 画面は対話的に動く。合成ルートが端末を raw mode + 代替スクリーンにして、TUI 面の
純粋な制御ループ（`presentation::run_welcome`）へ注入した端末（`Terminal` ポート）で毎フレーム
描き直し、キー入力で選択を動かす。実端末の制御（crossterm による raw mode・キー読み取り）は
合成ルートだけが持ち、TUI 面は `Terminal` ポートに対して純粋に振る舞う。非対話環境（パイプ・
CI など）では対話ループの代わりに Welcome の 1 フレームを出力して終了する。

まだ対話ループを持たない起動画面（Workspace / Config / Doctor）は、選択された画面名と
Workspace の path をバナーとして示す `BannerScreenRunner` が表示する。初期画面の選択は TUI の
application 層が行う。
