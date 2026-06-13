# usagi — データ保持の方法

`usagi` が永続化するデータは、スコープの異なる **2 層** に分かれています。

| 層 | 保存場所 | 何を持つか | 管理モジュール |
|---|---|---|---|
| **① usagi 全体（グローバル）** | `~/.usagi/`（`$USAGI_HOME` で上書き可） | 登録済みワークスペースの一覧、アプリ設定 | `infrastructure/storage.rs` (`Storage`) |
| **② workspace 毎（リポジトリ単位）** | `<repo>/.usagi/state.json` | そのリポジトリの worktree / ブランチの状態 | `infrastructure/workspace_store.rs` (`WorkspaceStore`) |

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
  "theme": "system",             // light | dark | system
  "default_workspace": "usagi",  // 既定で開くワークスペース名（未設定なら null）
  "workspace_root": "/home/me/git" // 新規プロジェクトのクローン先ベース（未設定なら null）
}
```

| フィールド | 型 | 意味 |
|---|---|---|
| `theme` | enum | UI のカラーテーマ（`light` / `dark` / `system`、既定 `system`） |
| `default_workspace` | string?\| | 既定で開くワークスペース名。無ければ `null` |
| `workspace_root` | string?\| | 新規プロジェクトのクローン先ベースディレクトリ。未設定時は `~/git` にフォールバック |

対応するユースケース（`usecase/settings.rs`）: `load` / `save` / `set_theme` /
`set_default_workspace`。設定画面（Config）は `load` で読み込み、変更を `save` で永続化します。

---

## ② workspace 毎（リポジトリ単位）

### 保存場所

各リポジトリの **プライマリ（main）worktree のルート直下** に `.usagi/state.json` を保存します。

```
<repo>/.usagi/state.json
```

- どの worktree からコマンドを実行しても、`git worktree list` の先頭（＝プライマリ worktree）を基準に保存先を解決します（`infrastructure/git.rs` の `primary_worktree`）。これによりリポジトリ内で 1 つの `state.json` に集約されます。
- `.usagi/` は `.gitignore` 済みのため、この状態ファイルは **コミットされずローカルにのみ保持** されます（マシンごとのローカルな状態スナップショットという位置づけ）。

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
      "path": "/Users/me/git/usagi/.claude/worktrees/login",
      "head": "aaf5459",
      "primary": false,
      "upstream": null,
      "status": "local",
      "updated_at": "2026-06-13T05:01:18.659149Z"
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
| `updated_at` | RFC3339(UTC) | この状態を git から同期した日時 |

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
    /Users/me/git/usagi/.claude/worktrees/login
```

### git 検査の方針（`infrastructure/git.rs`）

- `git2` などのライブラリに依存せず、システムの `git` コマンドを読み取り専用で呼び出す（`doctor` と同じ方針。ユーザーの git 設定をそのまま尊重できる）。
- すべての呼び出しで `-C <repo>` を渡し、対象リポジトリを明示する。
- git hook 実行中に環境へ注入される `GIT_DIR` / `GIT_WORK_TREE` / `GIT_INDEX_FILE` などの **repo-scoping 環境変数を除去** してから git を呼ぶ。これにより `-C <repo>` が常に優先され、hook 経由で実行されても別リポジトリを誤って操作しない。

---

## 関連モジュール一覧

| レイヤ | ファイル | 役割 |
|---|---|---|
| domain | `domain/workspace.rs` | グローバル登録エントリ `Workspace` |
| domain | `domain/settings.rs` | アプリ設定 `Settings` / `Theme` |
| domain | `domain/workspace_state.rs` | リポジトリ状態 `WorkspaceState` / `WorktreeState` / `BranchStatus` |
| infrastructure | `infrastructure/storage.rs` | グローバル `~/.usagi/` の load/save（`Storage`） |
| infrastructure | `infrastructure/workspace_store.rs` | リポジトリ `<repo>/.usagi/state.json` の load/save（`WorkspaceStore`） |
| infrastructure | `infrastructure/git.rs` | git CLI 経由の読み取り専用検査 |
| usecase | `usecase/workspace.rs` | グローバル登録の add/list/remove/touch |
| usecase | `usecase/settings.rs` | 設定の load/更新 |
| usecase | `usecase/workspace_state.rs` | リポジトリ状態の inspect/sync/load |
