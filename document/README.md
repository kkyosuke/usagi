# usagi ドキュメント

> リポジトリの [README](../README.md)

仕様・規約の正本。**現在のビルドで動作する内容だけ**を記載する
（[06-conventions.md#記載実装済み](06-conventions.md#記載実装済み)）。

## 目次

| # | ドキュメント | 内容 |
|---|---|---|
| 1 | [01-overview.md](01-overview.md) | プロジェクト概要 |
| 2 | [02-architecture.md](02-architecture.md) | アーキテクチャ（workspace 構成・クレート責務・依存ルール） |
| 3 | [03-tui.md](03-tui.md) | TUI の画面遷移・live pane・resume data compatibility |
| 4 | [04-ipc.md](04-ipc.md) | daemon IPC の identity・wire protocol・Unix transport 契約 |
| 5 | [05-daemon.md](05-daemon.md) | daemon の session lifecycle・terminal ownership・generation 契約 |
| 6 | [06-conventions.md](06-conventions.md) | 開発規約（ブランチ・コミット・PR・品質チェック・CI・リリース） |
| 7 | [07-mcp.md](07-mcp.md) | MCP サーバ（agent 入口面）の JSON-RPC メソッド・tool 面・resource 面・orchestration ガイド |
| 8 | [08-coverage.md](08-coverage.md) | `coverage(off)` の symbol inventory・領域別返済順序 |
| 9 | [09-env.md](09-env.md) | 環境変数設定（global / workspace の 2 層・secret 解決・子プロセスへの注入） |
| 10 | [10-session-roles.md](10-session-roles.md) | session role（catalog・stable assignment・daemon 検証・prompt 合成） |
| — | [proposals/](proposals/README.md) | 設計提案（未実装の構成・機構の設計判断。spec とは分離して管理） |
