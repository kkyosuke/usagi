# 1. usagi 全体（グローバル）

> [データ永続化トップ](README.md) ｜ 次へ → [2. workspace 毎（リポジトリ単位）](02-workspace.md)

マシン横断で「どのリポジトリを usagi で管理しているか」と「アプリ全体の設定」を保持する層です。
`infrastructure/storage.rs` の `Storage` が管理します。

## 目次

- [保存場所](#保存場所)
- [`workspaces.json`](#workspacesjson)
- [`settings.json`](#settingsjson)
- [`logs/`（エラーログ）](#logsエラーログ)

## 保存場所

`infrastructure/storage.rs` の `data_dir()` が解決します。

1. 環境変数 `USAGI_HOME`（`DATA_DIR_ENV`）が設定されていればそれを使用
2. なければ `~/.usagi`（`$HOME/.usagi`）

```
~/.usagi/
├── workspaces.json   # 登録済みワークスペースの一覧
├── settings.json     # アプリ設定
└── logs/             # 日次のエラーログ（error-YYYY-MM-DD.log）
```

## `workspaces.json`

usagi が管理対象として登録したワークスペースの一覧です。TUI のプロジェクト選択画面
（[../design/02-open.md](../design/02-open.md)）はここを読み取って候補を表示します。

```jsonc
{
  "version": 1,
  "workspaces": [
    {
      "name": "usagi",
      "path": "/Users/me/git/usagi",
      "created_at": "2026-06-13T05:01:18.659149Z",
      "updated_at": "2026-06-13T05:01:18.659149Z"
    }
  ]
}
```

| フィールド | 型 | 意味 |
|---|---|---|
| `name` | string | ワークスペースの一意な表示名 |
| `path` | path | ワークスペースディレクトリの絶対パス |
| `created_at` | RFC3339(UTC) | 登録日時 |
| `updated_at` | RFC3339(UTC) | 最終利用・更新日時（`touch` で更新） |

対応するユースケース（`usecase/workspace.rs`）: `add` / `list`（`updated_at` 降順）/ `remove` / `touch`。

## `settings.json`

ユーザーが設定可能なアプリ全体の設定です。本書は**保存フォーマット**を示します。各項目の意味・型・既定値・
選択肢・編集方法は [../05-settings.md#設定項目](../05-settings.md#設定項目) が正本です。

```jsonc
{
  "version": 1,
  "theme": "system",              // light | dark | system
  "default_workspace": "usagi",   // 既定で開くワークスペース名（未設定なら null）
  "workspace_root": "/home/me/git", // 新規プロジェクトのクローン先ベース（未設定なら null）
  "notifications_enabled": true,  // デスクトップ通知の ON/OFF（既定 true）
  "agent_cli": "claude",          // 起動する AI エージェント CLI（claude | gemini）
  "session_action_ui": "menu",    // 在席の右ペイン UI（menu | prompt、既定 menu）
  "local_llm": {                  // ローカル LLM 委譲（既定 off）
    "enabled": false,             // 有効時 agent 起動に usagi-llm MCP を追加
    "model": "qwen2.5-coder:7b"   // 委譲先 Ollama モデル
  }
}
```

- すべての列挙型は `snake_case`、先頭に必須の `version`（現在 `1`）を持ちます。
- 省略されたフィールドは既定値として扱われます（`local_llm` ブロックごと省略可）。

対応するユースケース（`usecase/settings.rs`）: `load` / `save` / `set_theme` /
`set_default_workspace` / `set_notifications_enabled` / `set_agent_cli`（`session_action_ui` ほかは `save` で永続化）。
設定画面（Config）は `load` で読み込み、変更を `save` で永続化します。

## `logs/`（エラーログ）

実行時に `main` まで伝播したエラーを記録するディレクトリです。`infrastructure/error_log.rs` の
`ErrorLog` が管理し、CLI / TUI のどのコマンドで発生したエラーでも横断的に書き出します。

```
~/.usagi/logs/
├── error-2026-06-15.log
└── error-2026-06-16.log
```

- **日次ローテーション**: ファイル名は `error-YYYY-MM-DD.log`。その日に発生したエラーは同じファイルへ追記します。
- **1 エラー = 1 エントリ**: `[YYYY-MM-DD HH:MM:SS] <メッセージ>` 形式。エラーチェーン（`anyhow` の `caused by`）は
  改行を字下げして同じエントリ内にまとめます。
- **30 日で削除**: エラー発生時にあわせて、`error_log::RETENTION_DAYS`（30 日）より古い日次ファイルを削除します。
- **ベストエフォート**: ログ出力自体の失敗は握りつぶし、元のエラーの stderr 出力を妨げません。書き込めなくても
  usagi の動作は止まりません。

> JSON ではなくプレーンテキストの追記ログです。`version` フィールドやアトミック書き込みは持ちません（上の
> [共通の方針](README.md#共通の方針) は JSON 永続化のための取り決めで、エラーログには適用されません）。
