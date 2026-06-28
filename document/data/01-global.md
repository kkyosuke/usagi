# 1. usagi 全体（グローバル）

> [データ永続化トップ](README.md) ｜ 次へ → [2. workspace 毎（リポジトリ単位）](02-workspace.md)

マシン横断で「どのリポジトリを usagi で管理しているか」と「アプリ全体の設定」を保持する層です。
`infrastructure/storage.rs` の `Storage` が管理します。

## 目次

- [保存場所](#保存場所)
- [`workspaces.json`](#workspacesjson)
- [`settings.json`](#settingsjson)
- [`agent-state/`](#agent-state)
- [`agent-prompts/`](#agent-prompts)
- [`open-panes/`（ペイン復旧スナップショット）](#open-panesペイン復旧スナップショット)
- [`resume-focus/`（復帰フォーカススナップショット）](#resume-focus復帰フォーカススナップショット)
- [`skills/`（Agent へ配布するスキル）](#skillsagent-へ配布するスキル)
- [`logs/`（エラーログ）](#logsエラーログ)
- [`logs/`（操作トレース）](#logs操作トレース)

## 保存場所

`infrastructure/storage.rs` の `data_dir()` が解決します。

1. 環境変数 `USAGI_HOME`（`DATA_DIR_ENV`）が設定されていればそれを使用
2. なければ `~/.usagi`（`$HOME/.usagi`）

```
~/.usagi/
├── workspaces.json   # 登録済みワークスペースの一覧
├── settings.json     # アプリ設定
├── agent-state/      # 起動中 Agent の ready/running/waiting/ended phase（worktree 別の一時キャッシュ）
├── agent-prompts/    # session_prompt がキューした、次回起動時に Agent へ渡すプロンプト（worktree 別）
├── open-panes/       # 各セッションの開いていたペイン構成（次回起動時に復旧する。worktree 別）
├── resume-focus/     # 終了時にいたセッションとエンゲージメント段階（次回起動時に復帰する。ワークスペース別）
├── skills/           # usagi がバイナリに同梱し Agent へ配布するスキル（起動時に展開。セッション worktree から symlink）
└── logs/             # 日次のエラーログ（error-YYYY-MM-DD.log）と操作トレース（trace-YYYY-MM-DD.jsonl）
```

環境変数 `USAGI_TRACE` で操作トレース（後述の [`logs/`（操作トレース）](#logs操作トレース)）の記録を有効化します（既定は無効）。

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
  "agent_cli": "claude",          // 起動する AI エージェント CLI（claude | codex | codex_fugu | gemini）
  "session_action_ui": "menu",    // 在席の右ペイン UI（menu | prompt、既定 menu）
  "sidebar": "full",              // 左サイドバーの初期状態（full | rail、既定 full。Ctrl-B で開閉）
  "mascot_animation_enabled": true, // サイドバーうさぎが操作に反応するか（既定 true。off で静止）
  "terminal_scrollback_lines": 2000, // 埋め込み端末ペインのスクロールバック行数（既定 2000、上限 50000）
  "local_llm": {                  // ローカル LLM 委譲（既定 off）
    "enabled": false,             // 有効時 agent 起動に usagi-llm MCP を追加
    "model": "qwen2.5-coder:7b"   // 委譲先 Ollama モデル
  }
}
```

- すべての列挙型は `snake_case`、先頭に必須の `version`（現在 `1`）を持ちます。
- 省略されたフィールドは既定値として扱われます（`local_llm` ブロックごと省略可）。
- **前方互換**: 新しい usagi が書いた**未知の列挙値**（例: 将来追加される `agent_cli` / `theme`）を古い usagi が読んでも、`settings.json` 全体の読み込みは失敗しません。未知値はそのフィールドだけを既定値に縮退させ、他の設定はそのまま読み込みます（`domain::serde_fallback`）。同じ縮退は `workspaces.json` の登録エントリにも `state.json` にも適用されます。
- `workspaces.json` への登録・削除・最終利用時刻の更新（`add` / `remove` / `touch`）は、複数の usagi プロセス（複数 TUI とセッションの `usagi mcp`）が同じファイルを read-modify-write しても**更新を取りこぼさない**よう、`.usagi/.lock` のプロセス間ロックで直列化されます（各 `save` の原子的書き込みに加えて、load→変更→save のシーケンス全体をロックで保護）。

対応するユースケース（`usecase/settings.rs`）: `load` / `save` / `set_theme` /
`set_default_workspace` / `set_notifications_enabled` / `set_agent_cli`（`session_action_ui` ほかは `save` で永続化）。
設定画面（Config）は `load` で読み込み、変更を `save` で永続化します。

## `agent-state/`

起動中の Agent が報告するライフサイクル phase を worktree ごとに保持する**一時キャッシュ**です。`claude` / `codex` の
ライフサイクルフックが `usagi agent-phase <phase>` を実行して書き込み、ホーム画面の監視スレッドが読み取って
`☾ ready` / `▶ running` / `◆ waiting` / `✓ done` を描画します（仕組みは [../04-orchestration.md#agent-フックによる状態報告](../04-orchestration.md#agent-フックによる状態報告) が正本）。

- ファイル名は worktree の正規化パスのハッシュ（16 桁 hex）。フックと監視スレッドが同じ規則で算出するため、
  ディレクトリ走査なしに対応付きます。内容にも worktree パスを持ち、ハッシュ衝突や別マシン由来の古いファイルは
  読み捨てます。
- `{ "worktree": "<path>", "phase": "ready" | "running" | "waiting" | "ended" }`。セッション起動時にリセットされ、永続的な
  状態ではないため `version` は持ちません。`infrastructure/agent_state_store.rs` が read/write/clear を担います。
- `session remove` でセッションを破棄すると、その worktree の phase ファイルも会話履歴（Claude の transcript / Codex の rollout / Gemini の chats）と
  あわせて削除されます（[../04-orchestration.md#セッションのライフサイクル](../04-orchestration.md) 参照）。

## `agent-prompts/`

MCP の [`session_prompt`](../03-commands/03-mcp.md#session_prompt-の挙動) がキューしたプロンプトを worktree
ごとに保持する**一時キャッシュ**です。`usagi mcp` プロセスは動作中の TUI を直接操作できないため、プロンプトを
ここへ置き、ホーム画面がそのセッションのエージェントペインを次にフレッシュ起動するときに取り出して、
エージェントの最初のメッセージとして渡します（[../03-commands/03-mcp.md#session_prompt-の挙動](../03-commands/03-mcp.md#session_prompt-の挙動) が正本）。

- ファイル名は worktree の正規化パスのハッシュ（16 桁 hex）。書き手（MCP）と読み手（TUI）が同じ規則で算出する
  ため、ディレクトリ走査なしに対応付きます。内容にも worktree パスを持ち、ハッシュ衝突や別マシン由来の古い
  ファイルは読み捨てます（`agent-state/` と同じ方式）。
- `{ "worktree": "<path>", "prompt": "<text>" }`。永続的な状態ではないため `version` は持ちません。取り出しは
  読み取りと同時に削除する**ワンショット**で、`infrastructure/agent_prompt_store.rs` が set/take を担います。

## `open-panes/`（ペイン復旧スナップショット）

各セッションが開いていたペイン構成を worktree ごとに保持し、次回起動時の**ペインの復旧**に使います（仕組みは
[../04-orchestration.md#ペインの復旧](../04-orchestration.md#ペインの復旧) が正本）。

- ファイル名は worktree の正規化パスのハッシュ（16 桁 hex）。内容にも worktree パスを持ち、ハッシュ衝突や
  別マシン由来の古いファイルは読み捨てます（`agent-state/` と同じ方式）。
- `{ "worktree": "<path>", "active": <usize>, "panes": [ { "kind": "agent" | "terminal", "cli": "claude" | null }, … ] }`。
  `panes` はタブ順、`active` は最後にアクティブだったタブの添字。`cli` は agent ペインのみ値を持ち（どの Agent CLI で
  復旧するか）、terminal ペインは `null`。永続的な状態ではないため `version` は持ちません。
- 書き込みはペインを開閉して制御が戻るたび。ペインが 1 つも無くなると消去され、`session remove` でも
  当該 worktree 分が消えます。`infrastructure/open_panes_store.rs` が save/load/clear を担います。

## `resume-focus/`（復帰フォーカススナップショット）

終了時にユーザーがいた**セッションとエンゲージメント段階**（切替 / 在席 / 没入）をワークスペースごとに保持し、
次回起動時の**復帰**に使います（仕組みは [../04-orchestration.md#ペインの復旧](../04-orchestration.md#ペインの復旧)
が正本）。ペイン復旧が「どのペインを開いていたか」を worktree 別に記録するのに対し、こちらは「どこにいたか」を
ワークスペース別に 1 件記録します。

- ファイル名はワークスペースルートの正規化パスのハッシュ（16 桁 hex）。内容にもワークスペースパスを持ち、
  ハッシュ衝突や別マシン由来の古いファイルは読み捨てます（`open-panes/` と同じ方式）。
- `{ "workspace": "<path>", "session": "<name>", "engagement": "switch" | "focus" | "attached" }`。
  `session` は終了時にカーソルがあったセッション（ルート行は `root`）、`engagement` はその深さ。
  永続的な状態ではないため `version` は持ちません。
- 書き込みは終了が確定した時（quit 確認モーダルの承認 / 即時 Ctrl-C / `:quit`）。`restore_panes_enabled` が
  OFF のときは書き込まれません。起動時に読み出してカーソル移動（切替）/ 在席 / 自動 attach（没入）を復元し、
  `session` が既に消えている場合は何も復元しません。`infrastructure/resume_focus_store.rs` が save/load を担います。

## `skills/`（Agent へ配布するスキル）

usagi がバイナリに同梱し、起動した Agent（Claude Code）へ配布する**スキル**の実体です。スキルは
`assets/skills/<name>/SKILL.md` としてビルド時にバイナリへ埋め込まれ、`infrastructure/skills.rs` が
TUI / MCP の起動時にここへ展開します（`materialize`）。仕組みは
[../04-orchestration.md#スキルの配布](../04-orchestration.md#スキルの配布) が正本です。

```
~/.usagi/skills/
└── usagi-session/
    └── SKILL.md
```

- **単一情報源**: ここがスキルの唯一の実体で、各セッション worktree の `.claude/skills/<name>` は**この
  ディレクトリ下の各スキルへの symlink**（スキルごと）です。バイナリを更新して再起動すると `materialize` が
  再展開し、すべての symlink が同時に新しい内容を指します（worktree ごとにコピーしない）。スキル単位で張る
  ため、プロジェクト独自の `.claude/skills/<別名>` と共存します。
- **冪等な上書き**: 起動のたびに埋め込み内容で上書きするため、古い内容が残りません。永続的な状態では
  ないため `version` は持ちません。
- ベストエフォート: 展開に失敗しても usagi の起動は止めません。

## `logs/`（エラーログ）

実行時エラーを記録するディレクトリです。`infrastructure/error_log.rs` の `ErrorLog` が管理し、
CLI / TUI / MCP のどの経路で発生したエラーでも横断的に書き出します。記録対象は次の 6 系統です。

- **`main` まで伝播したエラー**: CLI 各コマンドが返す `Err`。
- **TUI 内の操作失敗**: `main` に到達せず画面表示だけで消えてしまう失敗も書き出します。
  画面のコマンドログに**エラーとして出る操作失敗はそのままファイルにも残る**（「画面に出るエラー
  ＝ファイルに残るエラー」）のが原則で、対象はセッションの作成・削除・リネーム、エージェント /
  ターミナルの起動（PTY spawn を含む。起動がそもそも live なペインに至らなかった場合を含む）、
  `preview` のファイル読み込み失敗などです。これにより「操作が失敗した」事象を画面を閉じた後からでも
  追跡できます。
- **起動後のランタイム失敗**: 起動には成功したが、埋め込みシェル（とその先のエージェント CLI）が
  異常終了したケースも書き出します。子プロセスの終了コードを取得し、非ゼロ終了は
  `agent session in <worktree> exited with status <code>`、シグナル終了は
  `... terminated by signal <signal>` の形で記録します。正常終了（exit 0、`exit` やエージェントの
  完了、ユーザーが意図的に閉じたペイン）はノイズ防止のため記録しません。
- **バックグラウンドスレッドの異常終了**: 画面に何も出ないまま機能が静かに止まる失敗も書き出します。
  セッション作成・削除のワーカースレッドが panic した場合はその panic メッセージを**ファイルに**記録し
  （タスク行は失敗として確定します。画面に出すエラー文は raw な panic 文を載せず「{操作}が異常終了しました
  （{対象}）。詳細はログを確認してください」の定型文にして、診断用の生メッセージはログだけに残します）、
  常駐の埋め込みターミナル監視スレッドが共有状態の mutex poison で停止した場合も
  停止理由を記録します。いずれも痕跡を残さず止まると bell / phase バッジやセッション操作が機能停止する
  ため、原因を後から追えるようにしています。
- **MCP ツールの失敗**: `usagi mcp` はヘッドレスで動き、失敗は呼び出し元エージェントへ返るだけで
  画面にもログにも残りません。`session_create` の失敗はクライアントへ返すのに加えて
  `mcp session_create "<name>" failed: ...` の書式で記録し（TUI のセッション作成エラーと同じ書式）、
  MCP 経由の失敗も横断的に追跡できるようにします。
- **静かに回復・握りつぶされる本物の失敗**: 画面にも `main` にも出ないまま、フォールバックで処理が
  続いてしまう本物の失敗（I/O 障害・データ破損）も書き出します。派生キャッシュ（メモリ / issue の
  `index.json`）が破損していてマークダウンから再構築するケース（`<...> index <path> is corrupt;
  rebuilding from markdown: ...`）や、ホーム画面の再描画で記録済みセッションの読み込みに失敗したケース
  （通知チャネルがなく握りつぶされる経路。`failed to load recorded sessions from <path>: ...`）が
  該当します。回復はするものの本物の破損 / I/O 失敗なので、痕跡を残します。なお、想定内のフォールバック
  （非 git パスや、リモートも現在ブランチも無い新規リポでの既定ブランチ解決など）はノイズになるため
  記録しません。

TUI の**画面に出る操作失敗**（上の 2 つ目）は、**単一のエラーシンク**に集約して記録します。`Logger`
トレイト（`record(&str)`）を infrastructure に定義し、ホーム画面の状態へ注入する形で、画面表示と
ファイル永続化を 1 経路で扱います（本番は `FileLogger`、テストは何も書かない `NoopLogger`）。ノイズを
避けるため、**単なる入力ミス**（未知のコマンド・`usage: …` などのコマンド結果）は画面には赤字で出ますが
**ファイルには残しません**。記録するのは実際の操作失敗だけです。なお、起動後に埋め込みシェル /
エージェントが異常終了したケース（上の 3 つ目。PTY セッションの後始末で検出）、画面を持たない
バックグラウンドスレッドの異常終了（上の 4 つ目）、静かに回復・握りつぶされる本物の失敗（上の 6 つ目）は、
このシンクを経由せず `ErrorLog::record` で直接記録します。

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

## `logs/`（操作トレース）

ユーザー・エージェントの**操作**を分析できるように記録するログです。失敗だけを残すエラーログ（上）とは別系統で、
`infrastructure/trace_log.rs` の `TraceLog` が管理します。記録対象は次の 4 系統で、いずれも 1 操作 = 1 行の
JSON（JSONL）として追記します。

- **CLI コマンド**: `usagi <subcommand>` の実行と成否（`main` のディスパッチで記録。長時間常駐する
  `mcp` / `llm-mcp` は対象外）。
- **TUI のキー操作・画面遷移**: ホーム画面のイベントループが処理したキーと、その時点のモード
  （統括 / 切替 / 在席 / 没入）。
- **セッション操作**: セッションの作成・削除（`usecase/session` で成功時に記録）。
- **MCP ツール呼び出し**: `usagi mcp` が受けたツール名と成否（ツールディスパッチで横断的に記録）。

```
~/.usagi/logs/
├── trace-2026-06-24.jsonl
└── trace-2026-06-25.jsonl
```

```jsonc
{"recorded_at":"2026-06-25T01:23:45.678Z","category":"cli","action":"doctor","detail":"ok"}
{"recorded_at":"2026-06-25T01:24:01.012Z","category":"tui","action":"key","detail":"Overview Char('j')"}
{"recorded_at":"2026-06-25T01:24:30.345Z","category":"session","action":"create","detail":"feature-x"}
{"recorded_at":"2026-06-25T01:25:10.901Z","category":"mcp","action":"issue_create","detail":"ok"}
```

| フィールド | 型 | 意味 |
|---|---|---|
| `recorded_at` | RFC3339(UTC) | 操作を記録した日時 |
| `category` | string | 発生源（`cli` / `tui` / `session` / `mcp`） |
| `action` | string | カテゴリ内の操作名（コマンド名・`key`・`create`・MCP ツール名など） |
| `detail` | string? | 操作の詳細（成否・押されたキー・名前など）。無いときは省略 |

- **オプトイン**: 既定では記録しません。環境変数 `USAGI_TRACE`（`trace_log::TRACE_ENV`）に空でも `0` でもない値を
  設定したときだけ有効になります。キー入力や MCP 呼び出しといったホットパス上にあるため、無効時は環境変数を
  1 度読むだけで何も書きません。
- **追記専用 JSONL**: コマンド履歴（[02-workspace.md](02-workspace.md) の `history.jsonl`）と同じく、1 行 1 イベントを
  `O_APPEND` で書きます。複数プロセス（複数 TUI・セッションの `usagi mcp`）が同じファイルへ書いても各行が
  混ざりません。`jq` などで `category` / `action` 別に集計できます。
- **日次ローテーション**: ファイル名は `trace-YYYY-MM-DD.jsonl`。その日のイベントは同じファイルへ追記します。
- **30 日で削除**: 記録時にあわせて、`trace_log::RETENTION_DAYS`（30 日）より古い日次ファイルをプロセスごとに
  1 度だけ削除します。
- **ベストエフォート**: トレース出力自体の失敗は握りつぶし、記録対象の操作を妨げません。
