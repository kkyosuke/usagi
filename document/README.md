# usagi ドキュメント

> リポジトリの [README](../README.md)

仕様・規約の正本。**現在のビルドで動作する内容だけ**を記載する
（[06-conventions.md#記載実装済み](06-conventions.md#記載実装済み)）。該当領域が実装されたときに
欠番を埋めていく。

## 目次

| # | ドキュメント | 内容 |
|---|---|---|
| 1 | [01-overview.md](01-overview.md) | プロジェクト概要 |
| 2 | [02-architecture.md](02-architecture.md) | アーキテクチャ（workspace 構成・クレート責務・依存ルール） |
| 3 | [03-tui.md](03-tui.md) | TUI の画面遷移・live pane・resume data compatibility |
| 4 | [04-ipc.md](04-ipc.md) | daemon IPC の identity・wire protocol・Unix transport 契約 |
| 5 | [05-daemon.md](05-daemon.md) | daemon の session lifecycle・terminal ownership・generation 契約 |
| 6 | [06-conventions.md](06-conventions.md) | 開発規約（ブランチ・コミット・PR・品質チェック・CI・リリース） |
| — | [proposals/](proposals/README.md) | 設計提案（未実装の構成・機構の設計判断。spec とは分離して管理） |
