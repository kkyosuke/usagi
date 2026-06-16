# 2. workspace 毎（リポジトリ単位）

> [データ永続化トップ](README.md) ｜ ← 前へ [1. usagi 全体（グローバル）](01-global.md) ｜ 次へ → [3. タスク issue（`issues/`）](03-issues.md)

「そのリポジトリの中で各 worktree が今どういう状態か」を保持する層です。各リポジトリ内に
`state.json` として保存され、`infrastructure/workspace_store.rs` の `WorkspaceStore` が管理します。

## 目次

- [保存場所](#保存場所)
- [`state.json`](#statejson)
- [`status`: ブランチのライフサイクル状態](#status-ブランチのライフサイクル状態)
- [同期と参照](#同期と参照)
- [git 検査の方針](#git-検査の方針infrastructuregitrs)
- [`settings.json`: プロジェクト固有の設定上書き（ローカル設定）](#settingsjson-プロジェクト固有の設定上書きローカル設定)
- [`history.json`](#historyjson)
- [`issues/`: タスク issue](#issues-タスク-issue)

## 保存場所

各リポジトリの **プライマリ（main）worktree のルート直下** に `.usagi/` を作り、その中に保存します。

```
<repo>/.usagi/
├── .gitignore      # .usagi/ 配下の git 管理を制御（usagi が生成・後述）
├── state.json      # worktree / ブランチの状態スナップショット
├── settings.json   # プロジェクト固有の設定上書き（ローカル設定）
├── history.json    # ワークスペース画面で実行したコマンドの履歴
└── issues/         # タスク issue（git で共有する。後述）
    ├── 001-*.md    # 1 issue = 1 ファイル（frontmatter 付き markdown）
    └── index.json  # 一覧・検索を速くする派生キャッシュ（git 管理外）
```

- どの worktree からコマンドを実行しても、`git worktree list` の先頭（＝プライマリ worktree）を基準に保存先を解決します（`infrastructure/git.rs` の `primary_worktree`）。これによりリポジトリ内で 1 つの `.usagi/` に集約されます。
- `.usagi/` の大半（`state.json` / `settings.json` / `history.json` / `sessions/`）は **マシンローカルな状態・設定** で、後述の `.gitignore` により **コミットされません**。
- 例外は **`.usagi/issues/`**。タスク issue はチームで共有したいので git 管理対象とします。派生キャッシュの `index.json` だけは再生成可能なので除外したままにします。
- git 管理の制御は **リポジトリルートの `.gitignore` には書かず、`.usagi/.gitignore` に自己完結させます**（`usagi::usecase::project::ignore_usagi_dir`）。リポジトリルートを汚さず、`.usagi/` 配下だけで完結するのが利点です。`usagi init` 時に次の内容（`.usagi/` 配下からの相対パターン）を書き込み、旧バージョンがルート `.gitignore` に追記していた `.usagi/` 系エントリがあれば除去します。

  ```gitignore
  # <repo>/.usagi/.gitignore
  /*
  !/.gitignore
  !/issues/
  /issues/index.json
  ```

### セッションの worktree 配置

`session create <name>` で作られる worktree は、ワークスペースルート直下の **`.usagi/sessions/<name>/`** に集約します（`.gitignore` 済み）。これによりセッションの所在が一意に定まり、一覧・削除・クリーンアップが扱いやすくなります。

ワークスペースのルートは git リポジトリである必要はありません。セッション作成時にルートを**再帰的に走査**し、

- **git リポジトリのディレクトリ** → その `git worktree` を `.usagi/sessions/<name>/<相対パス>/` に作成
- **git 管理外のファイル・ディレクトリ** → 同じ相対パスへコピー

として処理します。これにより、ルートが git でない複数リポジトリ構成（`/root` 直下に `app-a`・`app-b`、`be/be1` がそれぞれ git など）でも、各リポジトリごとに worktree が作られます。各 worktree の状態は引き続き下記 `WorktreeState` の配列として `state.json` に記録されます（`path` が `.usagi/sessions/<name>/...` を指す）。

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
      "root": "/Users/me/git/usagi/.usagi/sessions/login",
      "worktrees": [
        {
          "branch": "login",
          "path": "/Users/me/git/usagi/.usagi/sessions/login/app-a",
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
| `sessions` | array | 作成済みセッションの一覧（`.usagi/sessions/` 配下）。古いファイルには無く、その場合は空として扱う |
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
| `root` | path | セッションツリーのルート（`<workspace>/.usagi/sessions/<name>`） |
| `worktrees` | array&lt;WorktreeState&gt; | worktree を作成した各リポジトリの状態（下記） |
| `created_at` | RFC3339(UTC) | セッションの作成日時 |

セッション作成（`usecase/session`）はこの `SessionRecord` を `state.json` に追記します。
再同期（`usecase/workspace_state::sync`）は各セッション worktree の git ステータスを
読み直して更新します。

### worktree ごと（`WorktreeState`）

各セッションの `worktrees` 配列の要素。1 リポジトリにつき 1 つ。

| フィールド | 型 | 意味 |
|---|---|---|
| `branch` | string? | チェックアウト中のブランチ名。detached HEAD なら `null` |
| `path` | path | worktree ディレクトリの絶対パス（`.usagi/sessions/<name>/...`） |
| `head` | string | チェックアウト中コミットの短縮ハッシュ（7 桁） |
| `primary` | bool | 予約フィールド（セッション worktree では常に `false`） |
| `upstream` | string? | 上流追跡ブランチ（例 `origin/login`）。無ければ `null` |
| `status` | enum | ブランチのライフサイクル状態（下記） |
| `updated_at` | RFC3339(UTC) | この worktree の状態を更新した日時 |

## `status`: ブランチのライフサイクル状態

`local` → `pushed` → `up_to_date`（synced）の 3 状態で、ブランチがリモート・既定ブランチに対してどの段階にあるかを表します。

| 値（JSON） | 表示 | 意味 |
|---|---|---|
| `local` | `local` | ローカルにのみ存在。上流追跡ブランチが無い（未 push） |
| `pushed` | `pushed` | 上流追跡ブランチがある（push 済み） |
| `up_to_date` | `synced` | **独自コミットが 0**（既定ブランチの ancestor）。未マージの変更が無く最新に追従済み＝ up to date |

> **後方互換**: 旧 `state.json` はこの状態を `"merged"` と綴っていました。`BranchStatus` に `#[serde(alias = "merged")]` を付けているため、旧データの `"merged"` も `up_to_date` として読み込めます（書き出しは常に `"up_to_date"`）。

> **なぜ「merged」ではなく「up to date / synced」なのか**: 判定は「ブランチ先端が既定ブランチの ancestor か（＝独自コミットが 0 か）」だけを見ます。**新規に切っただけでまだコミットしていないブランチ**も、**完全にマージ済みのブランチ**も、git 上はどちらも「独自コミット 0・ancestor」で区別できません。そのため「マージ済み」と断定せず、「未マージの変更が無い＝最新に追従済み（up to date）」という事実だけを表す `synced` にしています。

### 判定ロジック（`usecase/workspace_state.rs` の `classify`）

優先度は **up_to_date（synced） > pushed > local**。

1. **up_to_date（synced）**: そのブランチの先端が、**その worktree のリポジトリの**既定ブランチの ancestor（`git merge-base --is-ancestor`）であれば、独自コミットが 0 ＝未マージの変更が無いとみなす。リモートの既定ブランチ（`origin/<default>`）を優先的に基準にするため、ローカル fetch 前でも「リモート main に対して追従済みか」を反映できる。
   - ただし既定ブランチと同名のブランチは、自分自身に対する判定から除外する（`local` / `pushed` のみ）。
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

セッションの作成・削除時（`usecase/session` の `reconcile`）には、`.usagi/sessions/` 配下のディレクトリと `state.json` の記録を照合し、**記録のない孤児ディレクトリを未コミット変更の有無にかかわらず強制削除**してディスクと state の同期を保ちます。記録済みセッション本体の削除には引き続き `--force` のガードが効きます。詳細は [4. オーケストレーション](../04-orchestration.md) を参照。

```
$ usagi status
updated 2026-06-13 05:01 UTC

session "login"  (/Users/me/git/usagi/.usagi/sessions/login)
  local    login                    aaf5459
    /Users/me/git/usagi/.usagi/sessions/login/app-a
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
  "agent_cli": "gemini",             // 任意。未設定ならグローバル値
  "notifications_enabled": false,    // 任意。未設定ならグローバル値
  "default_branch": "develop",       // 任意。未設定なら検出済み既定ブランチ（auto）
  "default_branch_source": "local",  // 任意。未設定なら remote
  "local_llm_enabled": true          // 任意。未設定ならグローバル値（local_llm.enabled）
}
```

| フィールド | 型 | 未設定（`null`）時 |
|---|---|---|
| `agent_cli` | enum? | グローバル `agent_cli` にフォールバック |
| `notifications_enabled` | bool? | グローバル `notifications_enabled` にフォールバック |
| `default_branch` | string? | リポジトリの検出済み既定ブランチ（auto）。**リポジトリ単位**（グローバルに対応項目なし） |
| `default_branch_source` | enum? | `remote`。**リポジトリ単位**（グローバルに対応項目なし） |
| `local_llm_enabled` | bool? | グローバル `local_llm.enabled` にフォールバック |

- 全フィールドが任意（`Option`）で、`null` は「グローバル設定に従う」を意味します。各項目の意味・選択肢は
  [../05-settings.md#ローカル設定プロジェクト単位の上書き](../05-settings.md#ローカル設定プロジェクト単位の上書き)、`default_branch` / `default_branch_source` を使った
  新ブランチの基点解決は [4. オーケストレーション#新ブランチの基点local--remote](../04-orchestration.md#新ブランチの基点local--remote)、編集画面は
  [design/04-config.md](../design/04-config.md) が正本です。
- **実効設定 = グローバル設定にローカルの上書きを適用した結果**。解決は `domain/settings.rs` の `Settings::with_local`、ユースケースは `usecase/settings.rs` の `effective(storage, repo_root)` が担います。
- 全項目を未上書きに戻しても `settings.json` は残し（中身は実質空）、「グローバルに従う／既定に従う」を意味します。

対応するユースケース（`usecase/settings.rs`）: `load_local` / `save_local` / `effective` /
`set_local_agent_cli` / `set_local_notifications_enabled`。

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

`.usagi/issues/` のタスク issue は、`.usagi/` の他のファイルと異なり **git で共有**されます。保存フォーマット
（issue ファイルの frontmatter・`index.json`・依存解決）は独立した [3. タスク issue（`issues/`）](03-issues.md) に
まとめています。
