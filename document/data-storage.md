# usagi — データ保持の方法

`usagi` が永続化するデータは、スコープの異なる **2 層** に分かれています。

| 層 | 保存場所 | 何を持つか | 管理モジュール |
|---|---|---|---|
| **① usagi 全体（グローバル）** | `~/.usagi/`（`$USAGI_HOME` で上書き可） | 登録済みワークスペースの一覧、アプリ設定 | `infrastructure/storage.rs` (`Storage`) |
| **② workspace 毎（リポジトリ単位）** | `<repo>/.usagi/` の `state.json` / `settings.json` / `history.json` | そのリポジトリの worktree / ブランチの状態、プロジェクト固有の設定上書き、コマンド実行履歴 | `infrastructure/workspace_store.rs` (`WorkspaceStore`) / `infrastructure/history_store.rs` (`HistoryStore`) |

①は「どのリポジトリを usagi で管理しているか」というマシン横断のインデックス、②は「そのリポジトリの中で各 worktree が今どういう状態か」というリポジトリ内のスナップショットです。役割が重ならないよう、保存場所もファイルも分離しています。

---

## 共通の方針

両層で次の方針を共有します。

- **フォーマットは JSON**（`serde` + `serde_json`）。`serde_yaml` は現在メンテナンスされていないため採用していません。
- **`version` フィールドを必ず持つ**。将来スキーマを変更したときに移行判断ができるよう、各ファイルの先頭にフォーマットバージョン（現在 `1`）を埋め込みます。
- **アトミック書き込み**。一時ファイル（`*.tmp`）に書いてから `rename` で置き換えるため、書き込み途中にクラッシュしても壊れた JSON が残りません。
- **ファイルが無い場合は「空」として扱う**。読み込み時に存在しなければ、空リスト / デフォルト値 / `None` を返し、初回起動でもエラーになりません。
- 出力は `to_string_pretty`（整形済み・末尾改行付き）で、人間が読める / 差分が見やすい形にします。

---

## ① usagi 全体（グローバル）

### 保存場所

`infrastructure/storage.rs` の `data_dir()` が解決します。

1. 環境変数 `USAGI_HOME`（`DATA_DIR_ENV`）が設定されていればそれを使用
2. なければ `~/.usagi`（`$HOME/.usagi`）

```
~/.usagi/
├── workspaces.json   # 登録済みワークスペースの一覧
└── settings.json     # アプリ設定
```

### `workspaces.json`

usagi が管理対象として登録したワークスペースの一覧です。TUI のプロジェクト選択画面はここを読み取って候補を表示します。

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

### `settings.json`

ユーザーが設定可能なアプリ全体の設定です。

```jsonc
{
  "version": 1,
  "theme": "system",              // light | dark | system
  "default_workspace": "usagi",   // 既定で開くワークスペース名（未設定なら null）
  "workspace_root": "/home/me/git", // 新規プロジェクトのクローン先ベース（未設定なら null）
  "notifications_enabled": true,  // デスクトップ通知の ON/OFF（既定 true）
  "agent_cli": "claude"           // 起動する AI エージェント CLI（claude | gemini）
}
```

| フィールド | 型 | 意味 |
|---|---|---|
| `theme` | enum | UI のカラーテーマ（`light` / `dark` / `system`、既定 `system`） |
| `default_workspace` | string?\| | 既定で開くワークスペース名。無ければ `null` |
| `workspace_root` | string?\| | 新規プロジェクトのクローン先ベースディレクトリ。未設定時は `~/git` にフォールバック |
| `notifications_enabled` | bool | デスクトップ通知（`hop` 時など）を表示するか。既定 `true` |
| `agent_cli` | enum | usagi が起動する AI エージェント CLI（`claude` / `gemini`、既定 `claude`） |

対応するユースケース（`usecase/settings.rs`）: `load` / `save` / `set_theme` /
`set_default_workspace` / `set_notifications_enabled` / `set_agent_cli`。設定画面（Config）は
`load` で読み込み、変更を `save` で永続化します。

---

## ② workspace 毎（リポジトリ単位）

### 保存場所

各リポジトリの **プライマリ（main）worktree のルート直下** に `.usagi/` を作り、その中に保存します。

```
<repo>/.usagi/
├── state.json      # worktree / ブランチの状態スナップショット
├── settings.json   # プロジェクト固有の設定上書き（ローカル設定）
└── history.json    # ワークスペース画面で実行したコマンドの履歴
```

- どの worktree からコマンドを実行しても、`git worktree list` の先頭（＝プライマリ worktree）を基準に保存先を解決します（`infrastructure/git.rs` の `primary_worktree`）。これによりリポジトリ内で 1 つの `.usagi/` に集約されます。
- `.usagi/` は `.gitignore` 済みのため、これらのファイルは **コミットされずローカルにのみ保持** されます（マシンごとのローカルな状態・設定という位置づけ）。

#### セッションの worktree 配置（#003）

`session new <name>` で作られる worktree は、ワークスペースルート直下の **`.usagi/worktree/<name>/`** に集約します（`.gitignore` 済み）。これによりセッションの所在が一意に定まり、一覧・削除・クリーンアップが扱いやすくなります。

ワークスペースのルートは git リポジトリである必要はありません。セッション作成時にルートを**再帰的に走査**し、

- **git リポジトリのディレクトリ** → その `git worktree` を `.usagi/worktree/<name>/<相対パス>/` に作成
- **git 管理外のファイル・ディレクトリ** → 同じ相対パスへコピー

として処理します。これにより、ルートが git でない複数リポジトリ構成（`/root` 直下に `app-a`・`app-b`、`be/be1` がそれぞれ git など）でも、各リポジトリごとに worktree が作られます。各 worktree の状態は引き続き下記 `WorktreeState` の配列として `state.json` に記録されます（`path` が `.usagi/worktree/<name>/...` を指す）。

### `state.json`

リポジトリ全体と、その全 worktree の状態です。

```jsonc
{
  "version": 1,
  "default_branch": "main",
  "worktrees": [
    {
      "branch": "main",
      "path": "/Users/me/git/usagi",
      "head": "76e906f",
      "primary": true,
      "upstream": "origin/main",
      "status": "pushed",
      "updated_at": "2026-06-13T05:01:18.659149Z"
    },
    {
      "branch": "feat/login",
      "path": "/Users/me/git/usagi/.usagi/worktree/login",
      "head": "aaf5459",
      "primary": false,
      "upstream": null,
      "status": "local",
      "updated_at": "2026-06-13T05:01:18.659149Z"
    }
  ],
  "sessions": [
    {
      "name": "feature-x",
      "created_at": "2026-06-14T01:02:03.456789Z",
      "root": "/Users/me/git/usagi/.usagi/worktree/feature-x",
      "repos": [
        {
          "relative": "",
          "path": "/Users/me/git/usagi/.usagi/worktree/feature-x",
          "branch": "feature-x"
        }
      ]
    }
  ],
  "updated_at": "2026-06-13T05:01:18.659149Z"
}
```

#### トップレベル（`WorkspaceState`）

| フィールド | 型 | 意味 |
|---|---|---|
| `default_branch` | string | リポジトリの既定ブランチ（例 `main`） |
| `worktrees` | array | 各 worktree の状態（プライマリが先頭） |
| `sessions` | array | `session new` で作成したセッションの一覧（省略時は空。下記 `Session`） |
| `updated_at` | RFC3339(UTC) | この状態を git から同期した日時 |

`sessions` は git 検査では復元できないため、`usagi status`（`sync`）が worktree を git から作り直す際も**既存のセッションはそのまま引き継ぎ**ます（`usecase/workspace_state.rs` の `sync`）。古い形式（`sessions` を持たない state.json）も `#[serde(default)]` により空リストとして読み込めます。

#### worktree ごと（`WorktreeState`）

| フィールド | 型 | 意味 |
|---|---|---|
| `branch` | string?\| | チェックアウト中のブランチ名。detached HEAD なら `null` |
| `path` | path | worktree ディレクトリの絶対パス |
| `head` | string | チェックアウト中コミットの短縮ハッシュ（7 桁） |
| `primary` | bool | リポジトリのプライマリ（main）worktree なら `true` |
| `upstream` | string?\| | 上流追跡ブランチ（例 `origin/feat/login`）。無ければ `null` |
| `status` | enum | ブランチのライフサイクル状態（下記） |
| `updated_at` | RFC3339(UTC) | この worktree の状態を更新した日時 |

#### セッションごと（`Session` / `SessionRepo`）

`session new <name>` は、ワークスペースルートを再帰的に走査し `.usagi/worktree/<name>/` 配下に同じディレクトリ構造を再現します（git リポジトリは新ブランチ `<name>` の worktree、それ以外はコピー）。`repos` には作成した **git worktree** のみを記録します（コピーしたファイルは含めない）。

| フィールド | 型 | 意味 |
|---|---|---|
| `name` | string | セッション名（新ブランチ名・worktree ディレクトリ名を兼ねる） |
| `created_at` | RFC3339(UTC) | 作成日時 |
| `root` | path | セッションルート（`.usagi/worktree/<name>`）の絶対パス |
| `repos` | array | 作成した各 worktree（`SessionRepo`） |
| `repos[].relative` | path | ワークスペースルートからの相対パス（ルート自身が git の場合は空文字） |
| `repos[].path` | path | 作成した worktree の絶対パス |
| `repos[].branch` | string | worktree にチェックアウトした新ブランチ名 |

対応するユースケース（`usecase/session.rs`）: `create`（セッション作成）/ `list`（一覧）。ワークスペース画面（`usagi hop`）の `session new <name>` / `session list` から呼ばれ、作成された worktree はサイドバーの worktree 一覧にも反映されます。

### `status`: ブランチのライフサイクル状態

`local` → `pushed` → `merged` の 3 状態で、ブランチがリモート・既定ブランチに対してどの段階にあるかを表します。

| 値 | 意味 |
|---|---|
| `local` | ローカルにのみ存在。上流追跡ブランチが無い（未 push） |
| `pushed` | 上流追跡ブランチがある（push 済み） |
| `merged` | 既定ブランチにマージ済み（既定ブランチの ancestor） |

#### 判定ロジック（`usecase/workspace_state.rs` の `classify`）

優先度は **merged > pushed > local**。

1. **merged**: そのブランチの先端が既定ブランチの ancestor（`git merge-base --is-ancestor`）であればマージ済みとみなす。リモートの既定ブランチ（`origin/<default>`）を優先的に基準にするため、ローカル fetch 前でも「リモート main に取り込まれたか」を反映できる。
   - ただしプライマリ worktree と既定ブランチ自身は、自分自身に対する merged 判定から除外する（統合先ブランチなので `local` / `pushed` のみ）。
2. **pushed**: 上流追跡ブランチ（`<branch>@{upstream}`）があれば push 済み。
3. **local**: 上記いずれにも当てはまらない。

### 同期と参照

`usecase/workspace_state.rs` が git 検査 → status 分類 → 保存をまとめます。

| 関数 | 役割 |
|---|---|
| `inspect(cwd)` | git を検査して `WorkspaceState` を組み立てる（保存しない） |
| `sync(cwd)` | `inspect` の結果を `<repo>/.usagi/state.json` に保存して返す |
| `load(cwd)` | 保存済みの状態を読み込む（無ければ `None`） |

CLI からは `usagi status` で `sync` が走り、最新状態を保存しつつ一覧表示します。

```
$ usagi status
default branch: main  (updated 2026-06-13 05:01 UTC)

* pushed   main                     76e906f → origin/main
    /Users/me/git/usagi
  local    feat/login               aaf5459
    /Users/me/git/usagi/.usagi/worktree/login
```

### git 検査の方針（`infrastructure/git.rs`）

- `git2` などのライブラリに依存せず、システムの `git` コマンドを読み取り専用で呼び出す（`doctor` と同じ方針。ユーザーの git 設定をそのまま尊重できる）。
- すべての呼び出しで `-C <repo>` を渡し、対象リポジトリを明示する。
- git hook 実行中に環境へ注入される `GIT_DIR` / `GIT_WORK_TREE` / `GIT_INDEX_FILE` などの **repo-scoping 環境変数を除去** してから git を呼ぶ。これにより `-C <repo>` が常に優先され、hook 経由で実行されても別リポジトリを誤って操作しない。

### `settings.json`: プロジェクト固有の設定上書き（ローカル設定）

グローバル設定（`~/.usagi/settings.json`）のうち、**プロジェクトごとに変えたい項目だけ**を上書きするローカル設定です。`.usagi/` 配下にあるためコミットされず、マシンごとに保持されます。

```jsonc
{
  "version": 1,
  "agent_cli": "gemini",          // 任意。未設定ならグローバル値を使う
  "notifications_enabled": false  // 任意。未設定ならグローバル値を使う
}
```

| フィールド | 型 | 意味 |
|---|---|---|
| `agent_cli` | enum?\| | このプロジェクトで起動する AI エージェント CLI（`claude` / `gemini`）。`null`（未設定）ならグローバル設定にフォールバック |
| `notifications_enabled` | bool?\| | このプロジェクトでのデスクトップ通知 ON/OFF。`null`（未設定）ならグローバル設定にフォールバック |

- 全フィールドが任意（`Option`）で、`null` は「グローバル設定に従う」を意味します。`light/dark` テーマやクローン先（`workspace_root`）のようにプロジェクト単位で変える意味の薄い項目は対象外です。
- **実効設定 = グローバル設定にローカルの上書きを適用した結果**。解決は `domain/settings.rs` の `Settings::with_local`、ユースケースは `usecase/settings.rs` の `effective(storage, repo_root)` が担います。

対応するユースケース（`usecase/settings.rs`）: `load_local` / `save_local` / `effective` /
`set_local_agent_cli` / `set_local_notifications_enabled`。

### `history.json`

ワークスペース画面（`usagi hop` 後の操作画面）のコマンドモードで実行したコマンドの履歴です。実行のたびに 1 件ずつ追記され、次回以降の画面起動時に読み込まれて `history` コマンドや `↑`/`↓` での履歴遡りに使われます。

```jsonc
{
  "version": 1,
  "entries": [
    { "command": "man",    "executed_at": "2026-06-14T01:02:03.456789Z" },
    { "command": "doctor", "executed_at": "2026-06-14T01:02:30.123456Z" }
  ]
}
```

| フィールド | 型 | 意味 |
|---|---|---|
| `entries` | array | 実行されたコマンドの並び（古い順） |
| `entries[].command` | string | 入力されたコマンド行（トリム済み） |
| `entries[].executed_at` | RFC3339(UTC) | コマンドを実行した日時 |

- 保存先は `state.json` と同じ `.usagi/` ディレクトリ（`<repo>/.usagi/history.json`）。
- `HistoryStore::append` は「読み込み → 1 件追加 → アトミック書き込み」を行う。読み込み時にファイルが無ければ空の履歴として扱う。
- ワークスペース画面での永続化は **ベストエフォート**。書き込みに失敗しても画面の操作は止めない（履歴が残らないだけ）。
- 対応するドメイン型は `domain/history.rs` の `HistoryEntry`。

---

## 関連モジュール一覧

| レイヤ | ファイル | 役割 |
|---|---|---|
| domain | `domain/workspace.rs` | グローバル登録エントリ `Workspace` |
| domain | `domain/settings.rs` | アプリ設定 `Settings` / `Theme` / `AgentCli`、ローカル設定 `LocalSettings`（`with_local` で上書き解決） |
| domain | `domain/workspace_state.rs` | リポジトリ状態 `WorkspaceState` / `WorktreeState` / `BranchStatus` |
| domain | `domain/session.rs` | セッション `Session` / `SessionRepo` |
| domain | `domain/history.rs` | コマンド履歴の 1 件 `HistoryEntry` |
| infrastructure | `infrastructure/storage.rs` | グローバル `~/.usagi/` の load/save（`Storage`） |
| infrastructure | `infrastructure/workspace_store.rs` | リポジトリ `<repo>/.usagi/` の `state.json` / `settings.json` の load/save（`WorkspaceStore`） |
| infrastructure | `infrastructure/history_store.rs` | リポジトリ `<repo>/.usagi/history.json` の load/append（`HistoryStore`） |
| infrastructure | `infrastructure/git.rs` | git CLI 経由の検査・worktree 作成（`add_worktree` / `is_repository_root` など） |
| infrastructure | `infrastructure/terminal.rs` | アクティブ worktree での対話シェル起動（`terminal`） |
| usecase | `usecase/workspace.rs` | グローバル登録の add/list/remove/touch |
| usecase | `usecase/settings.rs` | グローバル設定の load/更新、ローカル設定の load/save と実効設定の解決（`effective`） |
| usecase | `usecase/workspace_state.rs` | リポジトリ状態の inspect/sync/load（sync はセッションを保持） |
| usecase | `usecase/session.rs` | セッションの create/list（再帰的に worktree 構築） |
