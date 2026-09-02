# usagi ドキュメント

> リポジトリの [README](../README.md)

仕様・規約の入口。番号付き文書には**現在のビルドで動作する内容だけ**を記載する
（[06-conventions.md#記載実装済み](06-conventions.md#記載実装済み)）。

## 目次

- [読み方と SSoT](#読み方と-ssot)
- [仕様文書](#仕様文書)

## 読み方と SSoT

利用者はリポジトリの [README](../README.md) から始め、全体像は [1. プロジェクト概要](01-overview.md)、
詳細は下表の担当文書を参照する。開発者は加えて [2. アーキテクチャ](02-architecture.md) と
[6. 開発規約](06-conventions.md) を読む。同じ事実を複数文書へ複製せず、概要文書は担当する正本へリンクする。

文書は次のクラスに分ける。矛盾した場合は current spec の担当文書を現在契約とし、history や backlog を仕様の根拠にしない。

| クラス | 所在 | 役割 |
|---|---|---|
| current spec | 本 README の番号付き文書 | 実装済みの外部挙動、architecture、運用規約のトピック別 SSoT |
| proposal history | [`proposals/`](proposals/README.md) | 採用・却下を含む設計判断の履歴。現在契約は番号付き文書へ畳み込み、proposal 自体は正本にしない |
| backlog | [`.usagi/issues/`](../.usagi/issues/) | 未完了作業、改善案、進捗状態。実装済みとみなさず、現在契約は current spec で確認する |
| agent runbook | [`.agents/`](../.agents/) | session、worktree、PR の作業手順。製品仕様は current spec を参照する |

## 仕様文書

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
| — | [proposals/](proposals/README.md) | 設計判断の履歴（採用・却下を含む。current spec とは分離して管理） |
