# 1. usagi 全体（グローバル）

> [データ永続化トップ](README.md) ｜ 次へ → [2. workspace 毎（リポジトリ単位）](02-workspace.md)

マシン横断で「どのリポジトリを usagi で管理しているか」と「アプリ全体の設定」を保持する層です。
`infrastructure/storage.rs` の `Storage` が管理します。

## 目次

- [保存場所](#保存場所)
- [`workspaces.json`](#workspacesjson)
- [`settings.json`](#settingsjson)

## 保存場所

`infrastructure/storage.rs` の `data_dir()` が解決します。

1. 環境変数 `USAGI_HOME`（`DATA_DIR_ENV`）が設定されていればそれを使用
2. なければ `~/.usagi`（`$HOME/.usagi`）

```
~/.usagi/
├── workspaces.json   # 登録済みワークスペースの一覧
└── settings.json     # アプリ設定
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

ユーザーが設定可能なアプリ全体の設定です。各項目の意味・既定値・編集方法（Config 画面）は
[../05-settings.md](../05-settings.md) にまとめています。本書は保存フォーマットを示します。

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

| フィールド | 型 | 意味 |
|---|---|---|
| `theme` | enum | UI のカラーテーマ（`light` / `dark` / `system`、既定 `system`） |
| `default_workspace` | string?\| | 既定で開くワークスペース名。無ければ `null` |
| `workspace_root` | string?\| | 新規プロジェクトのクローン先ベースディレクトリ。未設定時は `~/git` にフォールバック |
| `notifications_enabled` | bool | デスクトップ通知（`hop` 時など）を表示するか。既定 `true` |
| `agent_cli` | enum | usagi が起動する AI エージェント CLI（`claude` / `gemini`、既定 `claude`） |
| `session_action_ui` | enum | ホーム画面の在席で右ペインに出すアクション UI（`menu` / `prompt`、既定 `menu`） |
| `local_llm.enabled` | bool | ローカル LLM 委譲を有効化するか。既定 `false`（オプトイン） |
| `local_llm.model` | string | 委譲先 Ollama モデル名。既定 `qwen2.5-coder:7b` |

対応するユースケース（`usecase/settings.rs`）: `load` / `save` / `set_theme` /
`set_default_workspace` / `set_notifications_enabled` / `set_agent_cli` / `set_session_action_ui`。設定画面（Config）は
`load` で読み込み、変更を `save` で永続化します。
