# 2. workspace 毎（リポジトリ単位）

> [データ永続化トップ](README.md) ｜ ← 前へ [1. usagi 全体（グローバル）](01-global.md)

「そのリポジトリの中で各 worktree が今どういう状態か」を保持する層です。各リポジトリ内に
`state.json` として保存され、`infrastructure/workspace_store.rs` の `WorkspaceStore` が管理します。

## 目次

- [保存場所](#保存場所)
- [`state.json`](#statejson)
- [`status`: ブランチのライフサイクル状態](#status-ブランチのライフサイクル状態)
- [同期と参照](#同期と参照)
- [git 検査の方針](#git-検査の方針)
- [`settings.json`: プロジェクト固有の設定上書き（ローカル設定）](#settingsjson-プロジェクト固有の設定上書きローカル設定)
- [`history.json`](#historyjson)
- [`issues/`: タスク issue](#issues-タスク-issue)

## 保存場所

各リポジトリの **プライマリ（main）worktree のルート直下** に `.usagi/` を作り、その中に保存します。

```
<repo>/.usagi/
├── state.json      # worktree / ブランチの状態スナップショット
├── settings.json   # プロジェクト固有の設定上書き（ローカル設定）
├── history.json    # ワークスペース画面で実行したコマンドの履歴
└── issues/         # タスク issue（git で共有する。後述）
    ├── 001-*.md    # 1 issue = 1 ファイル（frontmatter 付き markdown）
    └── index.json  # 一覧・検索を速くする派生キャッシュ（git 管理外）
```

- どの worktree からコマンドを実行しても、`git worktree list` の先頭（＝プライマリ worktree）を基準に保存先を解決します（`infrastructure/git.rs` の `primary_worktree`）。これによりリポジトリ内で 1 つの `.usagi/` に集約されます。
- `.usagi/` の大半（`state.json` / `settings.json` / `history.json` / `worktree/`）は **マシンローカルな状態・設定** で、`.gitignore` 済みのため **コミットされません**。
- 例外は **`.usagi/issues/`**。タスク issue はチームで共有したいので git 管理対象とし、`usagi init` は次の選択的な `.gitignore` を書き込みます（既存の `.usagi/` 単独エントリは移行されます）。派生キャッシュの `index.json` だけは再生成可能なので除外したままにします。

  ```gitignore
  .usagi/*
  !.usagi/issues/
  .usagi/issues/index.json
  ```

### セッションの worktree 配置

`session new <name>` で作られる worktree は、ワークスペースルート直下の **`.usagi/worktree/<name>/`** に集約します（`.gitignore` 済み）。これによりセッションの所在が一意に定まり、一覧・削除・クリーンアップが扱いやすくなります。

ワークスペースのルートは git リポジトリである必要はありません。セッション作成時にルートを**再帰的に走査**し、

- **git リポジトリのディレクトリ** → その `git worktree` を `.usagi/worktree/<name>/<相対パス>/` に作成
- **git 管理外のファイル・ディレクトリ** → 同じ相対パスへコピー

として処理します。これにより、ルートが git でない複数リポジトリ構成（`/root` 直下に `app-a`・`app-b`、`be/be1` がそれぞれ git など）でも、各リポジトリごとに worktree が作られます。各 worktree の状態は引き続き下記 `WorktreeState` の配列として `state.json` に記録されます（`path` が `.usagi/worktree/<name>/...` を指す）。

> このセッション構築の仕組み（再帰走査・複数リポジトリ対応・ライフサイクル）の全体像は
> [../04-orchestration.md](../04-orchestration.md) を参照してください。

## `state.json`

ワークスペースの**セッション**一覧と、各セッションの worktree の状態です。usagi が
追跡する状態の単位はセッションだけで、トップレベルの worktree 一覧は持ちません
（各 worktree は所属するセッションの中に記録されます）。

```jsonc
{
  "version": 1,
  "sessions": [
    {
      "name": "login",
      "root": "/Users/me/git/usagi/.usagi/worktree/login",
      "worktrees": [
        {
          "branch": "login",
          "path": "/Users/me/git/usagi/.usagi/worktree/login/app-a",
          "head": "aaf5459",
          "primary": false,
          "upstream": null,
          "status": "local",
          "updated_at": "2026-06-13T05:01:18.659149Z"
        }
      ],
      "created_at": "2026-06-13T05:01:18.659149Z"
    }
  ],
  "updated_at": "2026-06-13T05:01:18.659149Z"
}
```

### トップレベル（`WorkspaceState`）

| フィールド | 型 | 意味 |
|---|---|---|
| `sessions` | array | 作成済みセッションの一覧（`.usagi/worktree/` 配下）。古いファイルには無く、その場合は空として扱う |
| `updated_at` | RFC3339(UTC) | この状態を git から最後に更新した日時 |

> ワークスペース共通の「既定ブランチ」は持ちません。複数リポジトリで既定ブランチが異なり得る（`main` / `master` など）ため、各 worktree の status は**その worktree のリポジトリの既定ブランチ**に対して個別に判定します。

### セッションごと（`SessionRecord`）

セッションは usagi が追跡する唯一の状態単位で、**ルート配下の全リポジトリを横断**して
worktree を束ねます。各 worktree は git ステータス付き（下記 `WorktreeState`）で記録される
ため、ワークスペースの状態はセッションだけで完全に表現でき、ルートが git でない複数
リポジトリ構成にも対応できます。

| フィールド | 型 | 意味 |
|---|---|---|
| `name` | string | セッション名（各リポジトリで作成したブランチ名でもある） |
| `root` | path | セッションツリーのルート（`<workspace>/.usagi/worktree/<name>`） |
| `worktrees` | array&lt;WorktreeState&gt; | worktree を作成した各リポジトリの状態（下記） |
| `created_at` | RFC3339(UTC) | セッションの作成日時 |

セッション作成（`usecase/session.rs`）はこの `SessionRecord` を `state.json` に追記します。
再同期（`usecase/workspace_state::sync`）は各セッション worktree の git ステータスを
読み直して更新します。

### worktree ごと（`WorktreeState`）

各セッションの `worktrees` 配列の要素。1 リポジトリにつき 1 つ。

| フィールド | 型 | 意味 |
|---|---|---|
| `branch` | string?\| | チェックアウト中のブランチ名。detached HEAD なら `null` |
| `path` | path | worktree ディレクトリの絶対パス（`.usagi/worktree/<name>/...`） |
| `head` | string | チェックアウト中コミットの短縮ハッシュ（7 桁） |
| `primary` | bool | 予約フィールド（セッション worktree では常に `false`） |
| `upstream` | string?\| | 上流追跡ブランチ（例 `origin/login`）。無ければ `null` |
| `status` | enum | ブランチのライフサイクル状態（下記） |
| `updated_at` | RFC3339(UTC) | この worktree の状態を更新した日時 |

## `status`: ブランチのライフサイクル状態

`local` → `pushed` → `merged` の 3 状態で、ブランチがリモート・既定ブランチに対してどの段階にあるかを表します。

| 値 | 意味 |
|---|---|
| `local` | ローカルにのみ存在。上流追跡ブランチが無い（未 push） |
| `pushed` | 上流追跡ブランチがある（push 済み） |
| `merged` | 既定ブランチにマージ済み（既定ブランチの ancestor） |

### 判定ロジック（`usecase/workspace_state.rs` の `classify`）

優先度は **merged > pushed > local**。

1. **merged**: そのブランチの先端が、**その worktree のリポジトリの**既定ブランチの ancestor（`git merge-base --is-ancestor`）であればマージ済みとみなす。リモートの既定ブランチ（`origin/<default>`）を優先的に基準にするため、ローカル fetch 前でも「リモート main に取り込まれたか」を反映できる。
   - ただし既定ブランチと同名のブランチは、自分自身に対する merged 判定から除外する（`local` / `pushed` のみ）。
2. **pushed**: 上流追跡ブランチ（`<branch>@{upstream}`）があれば push 済み。
3. **local**: 上記いずれにも当てはまらない。

## 同期と参照

`usecase/workspace_state.rs` がセッション worktree の git 検査 → status 分類 → 保存をまとめます。

| 関数 | 役割 |
|---|---|
| `inspect_worktree(path)` | 1 つの worktree の git ステータスから `WorktreeState` を組み立てる（既定ブランチはその worktree のリポジトリから解決） |
| `sync(cwd)` | 保存済み state を読み込み、各セッション worktree のステータスを再計算して `<repo>/.usagi/state.json` に保存して返す（セッションが無ければ空の state を保存） |
| `load(cwd)` | 保存済みの状態を読み込む（無ければ `None`） |

CLI からは `usagi status` で `sync` が走り、最新状態を保存しつつセッションごとに一覧表示します。

```
$ usagi status
updated 2026-06-13 05:01 UTC

session "login"  (/Users/me/git/usagi/.usagi/worktree/login)
  local    login                    aaf5459
    /Users/me/git/usagi/.usagi/worktree/login/app-a
```

## git 検査の方針（`infrastructure/git.rs`）

- `git2` などのライブラリに依存せず、システムの `git` コマンドを読み取り専用で呼び出す（`doctor` と同じ方針。ユーザーの git 設定をそのまま尊重できる）。
- すべての呼び出しで `-C <repo>` を渡し、対象リポジトリを明示する。
- git hook 実行中に環境へ注入される `GIT_DIR` / `GIT_WORK_TREE` / `GIT_INDEX_FILE` などの **repo-scoping 環境変数を除去** してから git を呼ぶ。これにより `-C <repo>` が常に優先され、hook 経由で実行されても別リポジトリを誤って操作しない。

## `settings.json`: プロジェクト固有の設定上書き（ローカル設定）

グローバル設定（`~/.usagi/settings.json`、[01-global.md](01-global.md#settingsjson)）のうち、**プロジェクトごとに変えたい項目だけ**を上書きするローカル設定です。`.usagi/` 配下にあるためコミットされず、マシンごとに保持されます。設定の全体像（実効値の考え方）は [../05-settings.md](../05-settings.md) を参照してください。

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

> 編集 UI（[issue 022](../../issues/022-local-settings-ui.md)）: git リポジトリ内で開いた設定画面（Config）に、
> グローバル設定の下へ「Local · Agent CLI」「Local · Notifications」の行が追加されます。各行は
> **「グローバルに従う / ローカルで上書き」** を 1 つのセレクタで切り替えられ、未上書き時は現在の実効値
> （`Global (...)`）を表示します。保存時にグローバル設定とローカル設定（`save_local`）をまとめて書き込みます。
> 全項目を未上書きに戻しても `settings.json` は残し（中身は実質空）、「グローバルに従う」を意味します。

## `history.json`

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
- 対応するドメイン型は `domain/history.rs` の `HistoryEntry`。表示側の挙動は [../design/05-home.md](../design/05-home.md#履歴の永続化) を参照。

## `issues/`: タスク issue

プロジェクトのタスクを管理する issue です。`.usagi/` の他のファイルと異なり **git で共有** され、チームで同じタスク一覧を見られます。`infrastructure/issue_store.rs` の `IssueStore` が管理します。

```
<repo>/.usagi/issues/
├── 001-add-doctor-command.md   # 1 issue = 1 ファイル
├── 002-fix-login.md
└── index.json                  # 派生キャッシュ（git 管理外）
```

### issue ファイル（`NNN-<slug>.md`）

本リポジトリが `issues/` 配下で使う形式と同じく、上部に **frontmatter**（行ベースのメタデータ）、その下に自由記述の markdown 本文を持ちます。ファイル名は `番号(3桁ゼロ詰め)-タイトルのスラッグ.md`。

```markdown
---
number: 1
title: doctor コマンドを追加
status: todo
priority: medium
labels: [cli, infra]
dependson: [2, 3]
created_at: 2026-06-14T00:00:00+00:00
updated_at: 2026-06-14T00:00:00+00:00
---

# doctor コマンドを追加

本文（markdown 自由記述）。
```

| フィールド | 型 | 意味 |
|---|---|---|
| `number` | u32 | 採番された一意な番号（ファイル名の接頭辞と一致）。新規作成時に「既存の最大値 + 1」で振る |
| `title` | string | タイトル |
| `status` | enum | `todo` / `in-progress` / `done` |
| `priority` | enum | `high` / `medium` / `low`（既定 `medium`） |
| `labels` | array&lt;string&gt; | 任意のラベル |
| `dependson` | array&lt;u32&gt; | 先に `done` になっている必要がある issue 番号 |
| `created_at` / `updated_at` | RFC3339(UTC) | 作成・更新日時 |

- frontmatter は JSON 標準（serde_yaml 不採用）の方針に合わせ、既知フィールドを対象にした軽量パーサで読み書きします。未知のキーは無視するので、フォーマットを後方互換に拡張できます。
- 書き込みはアトミック（一時ファイル + `rename`）。タイトル変更でスラッグが変わった場合は、同じ番号の旧ファイルを削除して 1 issue = 1 ファイルを保ちます。

### `index.json`（派生キャッシュ）

一覧・検索を速くするための、各 issue のメタデータ（本文を除く）のキャッシュです。`version` を持ち、markdown ファイルが常に **正**。`index.json` が無い／壊れている場合は markdown 群から自動再構築されるため、欠落しても整合性は損なわれません（だから git 管理外でよい）。

```jsonc
{
  "version": 1,
  "issues": [
    {
      "number": 1,
      "title": "doctor コマンドを追加",
      "status": "todo",
      "priority": "medium",
      "labels": ["cli"],
      "dependson": [2, 3],
      "file": "001-add-doctor-command.md",
      "created_at": "2026-06-14T00:00:00+00:00",
      "updated_at": "2026-06-14T00:00:00+00:00"
    }
  ]
}
```

### 依存関係の解決（着手可能な issue）

一覧・検索では各 issue に **着手可能か（ready）** を付与します。`dependson` に挙げた issue が **すべて `done`** で、かつ自身が `done` でないものを ready とみなします（存在しない依存番号は未達として扱う）。これにより「今すぐ着手できるタスク」を絞り込めます。

| 層 | モジュール | 役割 |
|---|---|---|
| domain | `domain/issue.rs` | `Issue` / `IssueSummary` / `IssueStatus` / `IssuePriority`、frontmatter の読み書き |
| infrastructure | `infrastructure/issue_store.rs` | `.usagi/issues/` の走査・読み書き、`index.json` の生成・再構築・採番 |
| usecase | `usecase/issue.rs` | `create` / `get` / `list` / `search` / `update` / `delete` と ready 判定 |

> CLI（`usagi issue`）からの操作は [issue 024](../../issues/024-issue-cli.md)、AI エージェント（MCP）からの操作は [issue 025](../../issues/025-issue-mcp.md) で扱います。永続化基盤そのものは [issue 023](../../issues/023-issue-store.md)。
