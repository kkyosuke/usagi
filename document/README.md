# usagi v2 ドキュメント

> リポジトリの [README](../README.md) ｜ v1 の仕様は [v1/document/](../v1/document/README.md)

v2（フルリライト）の仕様・規約の正本。**現在のビルドで動作する内容だけ**を記載する
（[06-conventions.md#記載実装済み](06-conventions.md#記載実装済み)）。ファイル番号は v1 の
`document/` と同じ体系を使い、該当領域が v2 で実装されたときに欠番を埋めていく。

## 目次

| # | ドキュメント | 内容 |
|---|---|---|
| 1 | [01-overview.md](01-overview.md) | プロジェクト概要（v2 の位置づけ・v1 との関係） |
| 2 | [02-architecture.md](02-architecture.md) | アーキテクチャ（workspace 構成・クレート責務・依存ルール） |
| 6 | [06-conventions.md](06-conventions.md) | 開発規約（ブランチ・コミット・PR・品質チェック・CI・リリース） |
| — | [proposals/](proposals/README.md) | 設計提案（未実装の構成・機構の設計判断。spec とは分離して管理） |

## v1 ドキュメントとの関係

v1 時点の仕様（コマンド・画面・データ構造・orchestration・設計提案）は退避版
[v1/document/](../v1/document/README.md) にある。退避版は v1 実装のスナップショットとして更新しない。
配布中のバイナリ（v1 実装）の挙動や、v2 が引き継ぐ設計提案（例:
[daemon 化](../v1/document/proposals/02-daemon.md)）を参照するときに読む。
