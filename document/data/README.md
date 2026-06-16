# データ永続化

`usagi` が永続化するデータの仕様をまとめたディレクトリです。データは、スコープの異なる
**2 層** に分かれています。全体像は [../01-overview.md](../01-overview.md) を参照してください。

## 目次

| # | 層 | ドキュメント | 保存場所 | 何を持つか |
|---|---|---|---|---|
| 1 | usagi 全体（グローバル） | [01-global.md](01-global.md) | `~/.usagi/`（`$USAGI_HOME` で上書き可） | 登録済みワークスペースの一覧、アプリ設定、エラーログ |
| 2 | workspace 毎（リポジトリ単位） | [02-workspace.md](02-workspace.md) | `<repo>/.usagi/` の `state.json` / `settings.json` / `history.json` | そのリポジトリの worktree / ブランチの状態、プロジェクト固有の設定上書き、コマンド実行履歴 |
| — | タスク issue（②の git 共有部分） | [03-issues.md](03-issues.md) | `<repo>/.usagi/issues/` | git で共有するタスク issue（frontmatter 付き markdown + `index.json`） |

①は「どのリポジトリを usagi で管理しているか」というマシン横断のインデックス、②は「その
リポジトリの中で各 worktree が今どういう状態か」というリポジトリ内のスナップショットです。②のうち
**タスク issue だけは git で共有**するため、保存フォーマットを [03-issues.md](03-issues.md) に分けています。
役割が重ならないよう、保存場所もファイルも分離しています。

## 共通の方針

両層で次の方針を共有します。

- **フォーマットは JSON**（`serde` + `serde_json`）。`serde_yaml` は現在メンテナンスされていないため採用していません。
- **`version` フィールドを必ず持つ**。将来スキーマを変更したときに移行判断ができるよう、各ファイルの先頭にフォーマットバージョン（現在 `1`）を埋め込みます。
- **アトミック書き込み**。一時ファイル（`*.tmp`）に書いてから `rename` で置き換えるため、書き込み途中にクラッシュしても壊れた JSON が残りません。
- **ファイルが無い場合は「空」として扱う**。読み込み時に存在しなければ、空リスト / デフォルト値 / `None` を返し、初回起動でもエラーになりません。
- 出力は `to_string_pretty`（整形済み・末尾改行付き）で、人間が読める / 差分が見やすい形にします。

## 関連モジュール一覧

| レイヤ | ファイル | 役割 |
|---|---|---|
| domain | `domain/workspace.rs` | グローバル登録エントリ `Workspace` |
| domain | `domain/settings.rs` | アプリ設定 `Settings` / `Theme` / `AgentCli`、ローカル設定 `LocalSettings`（`with_local` で上書き解決） |
| domain | `domain/workspace_state.rs` | リポジトリ状態 `WorkspaceState` / `WorktreeState` / `SessionRecord` / `BranchStatus` |
| domain | `domain/history.rs` | コマンド履歴の 1 件 `HistoryEntry` |
| infrastructure | `infrastructure/storage.rs` | グローバル `~/.usagi/` の load/save（`Storage`） |
| infrastructure | `infrastructure/error_log.rs` | グローバル `~/.usagi/logs/` への日次エラーログ追記・30 日より古いファイルの削除（`ErrorLog`） |
| infrastructure | `infrastructure/workspace_store.rs` | リポジトリ `<repo>/.usagi/` の `state.json` / `settings.json` の load/save（`WorkspaceStore`） |
| infrastructure | `infrastructure/history_store.rs` | リポジトリ `<repo>/.usagi/history.json` の load/append（`HistoryStore`） |
| infrastructure | `infrastructure/agent_state_store.rs` | グローバル `~/.usagi/agent-state/` の Agent phase の read/write/clear |
| infrastructure | `infrastructure/git.rs` | git CLI 経由の読み取り専用検査 |
| usecase | `usecase/workspace.rs` | グローバル登録の add/list/remove/touch |
| usecase | `usecase/settings.rs` | グローバル設定の load/更新、ローカル設定の load/save と実効設定の解決（`effective`） |
| usecase | `usecase/workspace_state.rs` | セッション worktree のステータス再計算（`inspect_worktree` / `sync` / `load`） |
| usecase | `usecase/session` | セッション作成・削除（再帰 worktree + コピー）と `state.json` の `SessionRecord` 追記／除去 |

---

関連: [設定ガイド](../05-settings.md) ｜ [オーケストレーション](../04-orchestration.md) ｜ [アーキテクチャ](../02-architecture.md)
