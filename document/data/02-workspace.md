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
- [`history.jsonl`](#historyjsonl)
- [`issues/`: タスク issue](#issues-タスク-issue)

## 保存場所

各リポジトリの **プライマリ（main）worktree のルート直下** に `.usagi/` を作り、その中に保存します。

```
<repo>/.usagi/
├── .gitignore      # .usagi/ 配下の git 管理を制御（usagi が生成・後述）
├── .lock           # state.json 更新を直列化するプロセス間ロック（git 管理外）
├── state.json      # worktree / ブランチの状態スナップショット
├── settings.json   # プロジェクト固有の設定上書き（ローカル設定）
├── history.jsonl    # ワークスペース画面で実行したコマンドの履歴
├── issues/         # タスク issue（git で共有する。後述）
│   ├── 001-*.md    # 1 issue = 1 ファイル（frontmatter 付き markdown）
│   └── index.json  # 一覧・検索を速くする派生キャッシュ（git 管理外）
└── memory/         # AI エージェントのメモリ（git で共有する。後述）
    ├── <slug>.md   # 1 メモリ = 1 ファイル（frontmatter 付き markdown）
    ├── MEMORY.md   # 目次（1 メモリ = 1 行。git で共有する）
    └── index.json  # 一覧・検索を速くする派生キャッシュ（git 管理外）
```

- どの worktree からコマンドを実行しても、`git worktree list` の先頭（＝プライマリ worktree）を基準に保存先を解決します（`infrastructure/git.rs` の `primary_worktree`）。これによりリポジトリ内で 1 つの `.usagi/` に集約されます。
- `.usagi/` の大半（`state.json` / `settings.json` / `history.jsonl` / `sessions/`）は **マシンローカルな状態・設定** で、後述の `.gitignore` により **コミットされません**。`state.json` 更新を直列化するプロセス間ロック `.usagi/.lock` も同様で、トップレベルの `/*` で除外されます（`issues/` / `memory/` は git 管理対象に戻すため、それぞれの `.lock` を個別に再除外する点が異なります）。
- 例外は **`.usagi/issues/`** と **`.usagi/memory/`**。タスク issue とエージェントのメモリはチームで共有したいので git 管理対象とします。それぞれの派生キャッシュ `index.json` と、プロセス間書き込みロック用の `.lock` ファイルは再生成可能・ローカル専用なので除外したままにします（メモリの目次 `MEMORY.md` は共有対象）。
- git 管理の制御は **リポジトリルートの `.gitignore` には書かず、`.usagi/.gitignore` に自己完結させます**（`usagi::usecase::project::ignore_usagi_dir`）。リポジトリルートを汚さず、`.usagi/` 配下だけで完結するのが利点です。`usagi init` 時に次の内容（`.usagi/` 配下からの相対パターン）を書き込み、リポジトリルートの `.gitignore` に `.usagi/` 系エントリがあれば除去します。

  ```gitignore
  # <repo>/.usagi/.gitignore
  /*
  !/.gitignore
  !/issues/
  /issues/index.json
  /issues/.lock
  !/memory/
  /memory/index.json
  /memory/.lock
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
      "display_name": "ログイン機能",
      "note": "バリデーションを実装中\n・API は未着手",
      "root": "/Users/me/git/usagi/.usagi/sessions/login",
      "worktrees": [
        {
          "branch": "usagi/login",
          "path": "/Users/me/git/usagi/.usagi/sessions/login/app-a",
          "head": "aaf5459",
          "primary": false,
          "upstream": null,
          "status": "local",
          "diff": { "added": 124, "removed": 18 },
          "ahead_behind": { "ahead": 2, "behind": 1 },
          "pr": [{ "number": 412, "url": "https://github.com/KKyosuke/usagi/pull/412" }],
          "updated_at": "2026-06-13T05:01:18.659149Z"
        }
      ],
      "created_at": "2026-06-13T05:01:18.659149Z",
      "last_active": "2026-06-13T09:42:07.123456Z"
    }
  ],
  "root_note": "リリース前に CHANGELOG を更新する",
  "updated_at": "2026-06-13T05:01:18.659149Z"
}
```

### トップレベル（`WorkspaceState`）

| フィールド | 型 | 意味 |
|---|---|---|
| `sessions` | array | 作成済みセッションの一覧（`.usagi/sessions/` 配下）。**配列順がホーム画面の表示順**で、初期値は作成順、切替（Switch）の `K`/`J` で並び替えると入れ替えた順序がこの配列に永続化される（[design/home/02-layout.md](../design/home/02-layout.md#切替switch既定)）。古いファイルには無く、その場合は空として扱う |
| `root_note` | string? | ワークスペース**ルート行（`⌂ root`）**に紐づく自由記述の**複数行メモ**（任意）。セッションが持つ `note` のルート版で、ルートはどのセッションにも属さないためトップレベルに置く。**見た目だけ**の付加情報で、未設定（既定）なら省略される |
| `updated_at` | RFC3339(UTC) | この状態を git から最後に更新した日時 |

> ワークスペース共通の「既定ブランチ」は持ちません。複数リポジトリで既定ブランチが異なり得る（`main` / `master` など）ため、各 worktree の status は**その worktree のリポジトリの既定ブランチ**に対して個別に判定します。

### セッションごと（`SessionRecord`）

セッションは usagi が追跡する唯一の状態単位で、**ルート配下の全リポジトリを横断**して
worktree を束ねます。各 worktree は git ステータス付き（下記 `WorktreeState`）で記録される
ため、ワークスペースの状態はセッションだけで完全に表現でき、ルートが git でない複数
リポジトリ構成にも対応できます。

| フィールド | 型 | 意味 |
|---|---|---|
| `name` | string | セッション名。セッションの識別子で、作成後は変わらない。各リポジトリで作成するブランチは `usagi/<name>`（`usagi/` 名前空間に収め、手で切ったブランチと衝突させない） |
| `display_name` | string? | サイドメニューでの表示名（任意）。設定時は一覧の `name` の代わりに表示する**見た目だけ**の上書きで、ブランチ名・識別子は変えない。未設定（既定）なら省略され、`name` を表示する |
| `note` | string? | セッションに紐づく自由記述の**複数行メモ**（任意）。用途・残タスク・リンクなどの覚え書きで、**見た目だけ**の付加情報。ブランチ名・識別子には影響しない。未設定（既定）なら省略される |
| `root` | path | セッションツリーのルート（`<workspace>/.usagi/sessions/<name>`） |
| `worktrees` | array&lt;WorktreeState&gt; | worktree を作成した各リポジトリの状態（下記） |
| `created_at` | RFC3339(UTC) | セッションの作成日時 |
| `last_active` | RFC3339(UTC)? | このセッションを最後に触った日時（切替・在席でアクティブにした、または端末／Agent の活動を観測した）。ホーム画面の鮮度ドット（[design/home/02-layout.md](../design/home/02-layout.md#レイアウト)）の基準時刻で、放置するほど淡く沈む。未設定（既定。一度も触っていない）なら省略され、`created_at` にフォールバックする |

セッション作成（`usecase/session`）はこの `SessionRecord` を `state.json` に追記します。
表示名の変更（`usecase/session::set_display_name`、ホーム画面の[切替モードの `r`](../design/home/01-modes.md#各モードの説明)）は `display_name` だけを、メモの編集（`usecase/session::set_note`、ホーム画面の[切替モードの `n` / 没入の `Ctrl-E`](../design/home/05-overlays.md#セッションメモの編集)）は `note` だけを書き換えます。ルート行（`⌂ root`）のメモも同じ操作で編集でき、こちらはトップレベルの `root_note`（`usecase/session::set_root_note`）を書き換えます。`state.json` はマシンローカル（git 管理外）なので、メモはこの環境にだけ保存され、ブランチや PR では共有されません。
再同期（`usecase/workspace_state::sync`）は各セッション worktree の git ステータスを
読み直して更新します。

これらの更新（作成・削除・表示名変更・再同期）はいずれも `state.json` の read-modify-write（読み込み→編集→保存）です。個々の保存は一時ファイル＋rename でアトミックですが、その**一連**を `.usagi/.lock` に対するプロセス間排他ロックで直列化します。TUI と各セッションの `usagi mcp` サーバなど同一ワークスペースを共有する複数プロセスが同時に書いても、後勝ちで一方の更新を取りこぼす（lost update）ことがありません。

### worktree ごと（`WorktreeState`）

各セッションの `worktrees` 配列の要素。1 リポジトリにつき 1 つ。

| フィールド | 型 | 意味 |
|---|---|---|
| `branch` | string? | チェックアウト中のブランチ名（セッションの worktree なら `usagi/<name>`）。detached HEAD なら `null` |
| `path` | path | worktree ディレクトリの絶対パス（`.usagi/sessions/<name>/...`） |
| `head` | string | チェックアウト中コミットの短縮ハッシュ（7 桁） |
| `primary` | bool | 予約フィールド（セッション worktree では常に `false`） |
| `upstream` | string? | 上流追跡ブランチ（例 `origin/login`）。無ければ `null` |
| `status` | enum | ブランチのライフサイクル状態（下記） |
| `diff` | object? | 既定ブランチとの累積差分の行数 `{ "added": N, "removed": M }`。サイドバーの `+N -M` バッジの元。差分が無い（手つかず）・既定ブランチ自身・detached HEAD・読めなかったときは省略（`null` 相当）。古いファイルにキーが無くても読める |
| `ahead_behind` | object? | 既定ブランチとのコミット単位の差 `{ "ahead": N, "behind": M }`（`ahead`＝ブランチ側に多いコミット数・`behind`＝既定ブランチ側に多いコミット数）。サイドバーの `↑N ↓M` マーカーの元。差が無い（ahead も behind も 0）・既定ブランチ自身・detached HEAD・読めなかったときは省略（`null` 相当）。古いファイルにキーが無くても読める |
| `pr` | array | このセッションに紐づく Pull Request の配列 `[{ "number": N, "url": "..." }, …]`。サイドバーの `#N` バッジ（[design/home/03-sidebar.md](../design/home/03-sidebar.md#pr-バッジ)）の元で、クリックで各 `url` をブラウザで開く。セッションが複数リポジトリに跨り複数 PR を持つ場合は**全部**並ぶ。上の git 由来フィールドと違い**再同期で git から読み直さない**——没入中にエージェントが出力した PR の URL（`/pull/<N>`）をターミナル出力から拾い、worktree キーの保存先（`pr-links/`、URL 単位で重複排除しつつ蓄積）経由でここへ畳み込む。一度きりの URL でもバッジが再起動後も残るよう永続化する。未観測なら空配列で省略される。古いファイルにキーが無くても読める |
| `updated_at` | RFC3339(UTC) | この worktree の状態を更新した日時 |

## `status`: ブランチのライフサイクル状態

`new` → `dirty` → `local` → `pushed` → `synced` の 5 状態で、ブランチが「作業ツリー・リモート・既定ブランチ」に対してどの段階にあるかを表します。ブランチがこの順に一直線に進むわけではなく、**更新のたびに git から再判定**されます（編集すれば `dirty`、コミットすれば `local`、push すれば `pushed`）。

| 値（JSON） | 表示 | 意味 |
|---|---|---|
| `new` | `new` | 切ったばかりで未着手。作業ツリーがクリーンで独自コミットが 0、かつ既定ブランチも先行していない（既定と同じ位置）。セッション作成直後の状態 |
| `dirty` | `dirty` | 作業ツリーに未コミットの変更（変更・ステージ済み・未追跡）がある＝コミット前の作業中 |
| `local` | `local` | クリーンで、push されていない独自コミットがある（上流追跡ブランチ無し） |
| `pushed` | `pushed` | クリーンで、独自コミットがあり上流追跡ブランチもある（push 済み・未マージ） |
| `synced` | `synced` | 既定ブランチがこのブランチを追い越した（独自コミット 0 で behind > 0）。ブランチが持っていた変更は既定ブランチに取り込まれている＝マージ済み／最新追従済み |

> **`new` と `synced` の区別**: 独自コミット 0 のブランチは、新規（既定と同位置 = behind 0）と、既定ブランチが追い越したマージ済み（behind > 0）を **ahead/behind のコミット数**で区別し、前者を `new`、後者を `synced` とします。

> **後方互換**: `BranchStatus` は `#[serde(alias = "merged", alias = "up_to_date")]` を持ち、`"merged"` / `"up_to_date"` と書かれた `state.json` も `synced` として読み込めます（書き出しは常に `"synced"`）。

> **前方互換**: 新しい usagi が書いた**未知の `status` 値**（将来追加される状態）を古い usagi が読んでも、`state.json` 全体の読み込みは失敗しません。未知値はその worktree の `status` だけを既定（`new`）に縮退させて読み込み、次回 `sync` で git から再判定します（`domain::serde_fallback`）。

### 判定ロジック（`domain::workspace_state` の `BranchStatus::derive`）

判定そのもの（dirty・ahead/behind・上流の有無から status を決める純粋なルール）は domain の `BranchStatus::derive` が持つ。git からの事実収集（`ahead_behind` 呼び出しと、既定ブランチ・detached HEAD を比較対象から外す判断）は `usecase/workspace_state.rs` の `classify` が行い、集めた事実を `derive` に渡す。

判定の順序:

1. **dirty**: 作業ツリーに未コミット変更があれば、コミット状況によらず最優先で `dirty`。
2. それ以外は、**既定ブランチに対する ahead（独自コミット数）** で分岐する。ahead/behind は `infrastructure/git.rs` の `ahead_behind` が `git rev-list --left-right --count` で求め、基準の既定ブランチはリモート（`origin/<default>`）を優先するため、ローカル fetch 前でも「リモート main に取り込まれたか」を反映できる。
   - **ahead > 0**: 上流追跡ブランチがあれば `pushed`、無ければ `local`。
   - **ahead == 0**: 既定ブランチが先行していれば（behind > 0）`synced`、そうでなければ（既定と同位置）`new`。
3. 既定ブランチと同名のブランチ・detached HEAD は自分自身に対して比較しないため ahead/behind を参照せず、上流の有無で `local` / `pushed` のみになる。

### 集約（複数リポジトリ → セッション 1 行）

セッションは複数リポジトリの worktree を束ねるため、ホーム画面では各リポジトリの status を **最も進んでいないもの**に集約して 1 行に表示します（`BranchStatus::aggregate`）。進捗順は `new < dirty < local < pushed < synced`。したがってセッションが `synced` と読めるのは **全リポジトリのブランチが synced** のときだけです。詳細は [design/home/README.md](../design/home/README.md) を参照。

## 同期と参照

`usecase/workspace_state.rs` がセッション worktree の git 検査 → status 分類 → 保存をまとめます。

| 関数 | 役割 |
|---|---|
| `inspect_worktree(path, default)` | 1 つの worktree の git ステータス（ブランチ・HEAD・上流・未コミット変更・ahead/behind）から `WorktreeState` を組み立てる。ブランチ・HEAD・上流・未コミット変更は `git status --porcelain=v2 --branch` 1 回でまとめて取得する。分類の基準となる既定ブランチ `default` は呼び出し側が渡す |
| `inspect_worktrees(paths)` | worktree パスのリストを検査して `Vec<WorktreeState>` を返す共通ヘルパ。既定ブランチはリポジトリ単位の属性なので、各 worktree の属するリポジトリごとに 1 回だけ解決して使い回し、検査自体は並列に行う。セッション記録（`usecase/session` の `record`）と `sync` の両方がこれを使うので、両者が別実装に分かれない |
| `sync(cwd)` | 保存済み state を読み込み、各セッション worktree のステータスを再計算して `<repo>/.usagi/state.json` に保存して返す（セッションが無ければ空の state を保存）。全セッションの worktree をまとめて `inspect_worktrees` に渡し、既定ブランチをリポジトリ単位で 1 回だけ解決する |
| `load(cwd)` | 保存済みの状態を読み込む（無ければ `None`） |

### 更新タイミング

status は再計算した時点のスナップショットで、`sync` が走るたびに最新化されます。再計算の契機は次のとおり:

| 契機 | 動作 |
|---|---|
| `usagi status`（CLI） | `sync` を実行して最新化し、セッションごとに一覧表示する |
| ホーム画面の起動時 | 画面を開いた瞬間に `sync` して最新の status を表示する |
| 埋め込み terminal / agent を閉じた・切り替えた直後 | ペインでコミット・push・マージした可能性があるため再 `sync` する。`sync` は worktree ごとの `git status` とプロセス間ロック待ちで重く、複数セッションで `agent` を動かしているときに顕著なので**バックグラウンドスレッドで実行**し、ペイン離脱自体は待たせない。同期が終わるとイベントループが結果を取り込み、カーソル位置を保ったまま status を更新する |
| セッションの作成・削除時 | 作成・削除に伴い `sync`（`reload_sessions`）して一覧と status を更新する |

> git 呼び出しはユーザー操作の区切り（画面遷移・ペイン離脱・コマンド実行）でのみ行い、常時ポーリングはしない。ペイン離脱に伴う再 `sync` はバックグラウンドで走るため、離脱直後はペインを出る前の status が一瞬残り、同期完了後に最新化される。

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
  "local_llm_enabled": true,         // 任意。未設定ならグローバル値（local_llm.enabled）
  "skill_features": {                 // 任意。機能 ID ごとに上書き（未記載はグローバル値）
    "pull-request": false             // 例: このプロジェクトでだけ PR スキル群を無効化
  }
}
```

| フィールド | 型 | 未設定（`null`）時 |
|---|---|---|
| `agent_cli` | enum? | グローバル `agent_cli` にフォールバック |
| `notifications_enabled` | bool? | グローバル `notifications_enabled` にフォールバック |
| `default_branch` | string? | リポジトリの検出済み既定ブランチ（auto）。**リポジトリ単位**（グローバルに対応項目なし） |
| `default_branch_source` | enum? | `remote`。**リポジトリ単位**（グローバルに対応項目なし） |
| `local_llm_enabled` | bool? | グローバル `local_llm.enabled` にフォールバック |
| `skill_features` | map<string, bool> | 機能 ID 単位で上書き。未記載の機能はグローバル `skill_features` にフォールバック |

- 全フィールドが任意（`Option`）で、`null` は「グローバル設定に従う」を意味します。各項目の意味・選択肢は
  [../05-settings.md#ローカル設定プロジェクト単位の上書き](../05-settings.md#ローカル設定プロジェクト単位の上書き)、`default_branch` / `default_branch_source` を使った
  新ブランチの基点解決は [4. オーケストレーション#新ブランチの基点local--remote](../04-orchestration.md#新ブランチの基点local--remote)、編集画面は
  [design/04-config.md](../design/04-config.md) が正本です。
- **実効設定 = グローバル設定にローカルの上書きを適用した結果**。解決は `domain/settings.rs` の `Settings::with_local`、ユースケースは `usecase/settings.rs` の `effective(storage, repo_root)` が担います。
- 全項目を未上書きに戻しても `settings.json` は残し（中身は実質空）、「グローバルに従う／既定に従う」を意味します。

対応するユースケース（`usecase/settings.rs`）: `load_local` / `save_local` / `effective` /
`set_local_agent_cli` / `set_local_notifications_enabled`。

## `history.jsonl`

ワークスペース画面（`usagi hop` 後の操作画面）のコマンドモードで実行したコマンドの履歴です。実行のたびに 1 件ずつ追記され、次回以降の画面起動時に読み込まれて `history` コマンドや `↑`/`↓` での履歴遡りに使われます。

**追記専用の JSONL**（1 行 = 1 件の `HistoryEntry`）です。古い順に並びます。

```jsonl
{"command":"man","executed_at":"2026-06-14T01:02:03.456789Z"}
{"command":"doctor","executed_at":"2026-06-14T01:02:30.123456Z"}
```

| フィールド | 型 | 意味 |
|---|---|---|
| `command` | string | 入力されたコマンド行（トリム済み） |
| `executed_at` | RFC3339(UTC) | コマンドを実行した日時 |

- 保存先は `state.json` と同じ `.usagi/` ディレクトリ（`<repo>/.usagi/history.jsonl`）。
- `HistoryStore::append` は **1 行を `O_APPEND` で追記する**。全件を読み直して書き戻す read-modify-write をしないため、2 つの書き手（複数の TUI ペインや TUI とコマンド実行）が同時に追記しても互いのエントリを取りこぼさない。読み込み時にファイルが無ければ空の履歴として扱う。
- 読み込みは 1 行ずつパースする。空行は読み飛ばし、改行で終端されていない末尾行は「追記中の書きかけ」とみなして捨てる（破損として失敗させない）。改行で終端された不正な行は本物の破損としてエラーになる。
- 追記専用ゆえファイルは際限なく伸びるため、読み込みは**末尾の最新 1,000 件のみ**を取り込む（起動時のパース量と画面のメモリを一定に保つ。それより前の行は再検証しない）。画面側の履歴バッファも同数で頭打ちし、直前と同一のコマンドは記録しない。
- ワークスペース画面での永続化は **ベストエフォート**。書き込みに失敗しても画面の操作は止めない（履歴が残らないだけ）。
- 対応するドメイン型は `domain/history.rs` の `HistoryEntry`。表示側の挙動は [../design/home/05-overlays.md](../design/home/05-overlays.md#履歴の永続化) を参照。

## `issues/`: タスク issue

`.usagi/issues/` のタスク issue は、`.usagi/` の他のファイルと異なり **git で共有**されます。保存フォーマット
（issue ファイルの frontmatter・`index.json`・依存解決）は独立した [3. タスク issue（`issues/`）](03-issues.md) に
まとめています。

## `memory/`: エージェントのメモリ

`.usagi/memory/` の AI エージェントのメモリも、issue と同じく **git で共有**されます。保存フォーマット
（メモリファイルの frontmatter・目次 `MEMORY.md`・`index.json`）は独立した [4. メモリ（`memory/`）](04-memory.md) に
まとめています。
