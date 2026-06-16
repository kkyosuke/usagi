# 4. オーケストレーション（セッション・worktree 管理）

> [ドキュメント目次](README.md) ｜ ← 前へ [3. コマンドリファレンス](03-commands/README.md) ｜ 次へ → [5. 設定](05-settings.md)

`usagi` の中核は、**複数の作業を worktree ベースの「セッション」として束ね、複数リポジトリ構成でも
一括でオーケストレーションする**ことです。本書はその概念モデルとライフサイクルをまとめます。各コマンドの
構文は [3. コマンドリファレンス](03-commands/README.md)、画面の操作は [design/05-home.md](design/05-home.md)、
永続化されるデータは [data/02-workspace.md](data/02-workspace.md) を参照してください。

## 目次

- [用語](#用語)
- [なぜ worktree を 1 か所に集約するのか](#なぜ-worktree-を-1-か所に集約するのか)
- [セッションの構築（再帰走査と複数リポジトリ対応）](#セッションの構築再帰走査と複数リポジトリ対応)
- [新ブランチの基点（local / remote）](#新ブランチの基点local--remote)
- [state.json との同期（孤児セッションの掃除）](#statejson-との同期孤児セッションの掃除)
- [セッションのライフサイクル](#セッションのライフサイクル)
- [アクティブなセッションと AI 連携](#アクティブなセッションと-ai-連携)

## 用語

| 用語 | 意味 |
|---|---|
| ワークスペース | usagi に登録したプロジェクトのルートディレクトリ。git リポジトリでなくてもよい（複数リポジトリのルートでも可）。グローバルレジストリ `workspaces.json` に登録される |
| セッション | 1 つの作業単位。`session create <name>` でワークスペースルート配下に作られる worktree 群（＋コピー）の集合。名前 `<name>` で識別する |
| worktree | git の作業ツリー。各 git リポジトリにつき 1 つ、セッション用ブランチをチェックアウトして作られる |
| アクティブなセッション | `session switch` で選択中の作業対象。`terminal` / `agent` の実行カレントディレクトリになる |
| ルート行 | どのセッションにも属さない常設の行（`⌂ root`）。選ぶと `terminal` / `agent` がワークスペースルートで起動する |

## なぜ worktree を 1 か所に集約するのか

usagi は worktree を **リポジトリ任意の場所ではなく、ワークスペースルート直下の
`.usagi/sessions/<name>/` に集約** して管理します。これにより、

- セッションの所在が一意に定まる（どこに作られたか探さなくてよい）。
- 一覧・削除・クリーンアップが扱いやすくなる。
- `.usagi/` は `.gitignore` 済みのため、各 worktree がワークスペースのコミット対象に混入しない。

## セッションの構築（再帰走査と複数リポジトリ対応）

ワークスペースのルート自体が git リポジトリである必要はありません。`session create <name>` は
ルートを**再帰的に走査**し、各エントリを次のように扱います。

- **git リポジトリのディレクトリ** → そのリポジトリの `git worktree` を
  `.usagi/sessions/<name>/<相対パス>/` に、新しい `<name>` ブランチを切って作成する。
- **既存のリンク worktree**（`.git` がディレクトリでなくファイル＝他所で管理されている
  `git worktree`。例: `.workspace`、`.claude/worktrees/*`）→ 走査対象から除外し、複製も
  ブランチ作成もしない。
- **git 管理外のファイル・ディレクトリ** → 同じ相対パス `.usagi/sessions/<name>/<相対パス>/` へコピーする。

> `.git` / `.usagi` も走査対象から除外されます。

これにより、単一リポジトリだけでなく、ルートが git でない複数リポジトリ構成（モノレポ的な
ディレクトリツリー）にも対応できます。

```text
/root                         （git でなくてもよい）
├── app-a/      = git    → app-a の worktree を作成
├── app-b/      = git    → app-b の worktree を作成
├── be/                  （git でない素のディレクトリ → 再帰）
│   └── be1/    = git    → be/be1 の worktree を作成
└── README.md            （git 管理外 → コピー）
```

セッション `feature-x` を作成すると、`.usagi/sessions/feature-x/` 配下にルートと同じディレクトリ
構造が再現され、git 配下の各サブディレクトリはそれぞれ `feature-x` ブランチの worktree、それ以外は
コピーになります。各 worktree の状態は `state.json` の該当セッション（`SessionRecord`）の
`worktrees` 配列（`WorktreeState`）に記録されます（`path` が `.usagi/sessions/<name>/...` を指す）。
データ構造は [data/02-workspace.md](data/02-workspace.md) を参照してください。

セッション名は `session create <name>` の引数で渡すほか、名前を省くと[切替（Switch）モード](design/05-home.md#切替switch)の
左ペイン内インライン入力で指定できます（空文字・重複はバリデーション）。

## 新ブランチの基点（local / remote）

新しい `<name>` ブランチを**どの基点から切るか**は、各リポジトリのローカル設定
`default_branch_source`（[05-settings.md](05-settings.md#ローカル設定プロジェクト単位の上書き)）で選べます。

| `default_branch_source` | 基点 |
|---|---|
| `local` | そのリポジトリのローカル既定ブランチ（例 `main`） |
| `remote`（既定） | リモート追従の既定ブランチ（例 `origin/main`）。`origin/<既定>` が無ければローカル既定ブランチ → 現在の HEAD にフォールバック |

設定は**リポジトリ単位**です。複数リポジトリ構成では `session create` 実行時に各リポジトリの
`<repo>/.usagi/settings.json` をそれぞれ参照し、リポジトリごとに異なる基点で worktree を切れます。基点解決は
`infrastructure/git.rs` の `resolve_base_ref`、適用は `usecase/session` の `create` / `build_dir` が担います。

## state.json との同期（孤児セッションの掃除）

`session create` / `session remove` の実行時に、`.usagi/sessions/` 配下のディレクトリと `state.json` の記録を照合します。**`state.json` に記録のないディレクトリ**（中断された作成・手で編集された `state.json`・クラッシュなどで取り残されたもの）は「孤児」とみなし、**未コミット変更の有無にかかわらず強制削除**して同期を取ります（worktree の登録解除・セッションブランチの削除・コピーしたファイルの除去）。

- これにより、作成時は同名の取り残しディレクトリが新規セッションの作成を妨げません。
- 記録済みセッション本体の削除には引き続き未コミット変更のガード（`--force` 必須）が効きます。掃除されるのは **記録のない** ディレクトリだけです。
- セッションディレクトリ直下の単なるファイルは対象外です。

## セッションのライフサイクル

セッションは「作成 → 作業 → 破棄」で完結します。各操作は[ホーム画面](design/05-home.md)の `session` /
`terminal` / `agent` コマンドで行います。

```text
  session create <name>        terminal / agent          session remove <name>
        │                          │                            │
        ▼                          ▼                            ▼
   [セッション作成] ───────▶ [作業（worktree 上で          ───▶ [worktree・ブランチ・
   （再帰走査・worktree 構築）   シェル / Agent を起動）]          コピーを削除］
```

| 段階 | コマンド | 役割 |
|---|---|---|
| 作成 | `session create [<name>]` | ルートを再帰走査して `.usagi/sessions/<name>/` 配下に worktree 群を構築 |
| 一覧 | `session list` | セッション一覧（件数・各セッション名・worktree 数）を表示 |
| 切替 | `session switch [<name>]` | アクティブなセッションを切り替え（引数なしで[切替](design/05-home.md#切替switch)モード） |
| 作業 | `terminal` / `agent` | アクティブな worktree でシェル / Agent CLI を右ペインに埋め込み起動 |
| 状態確認 | `usagi status` | 各 worktree のブランチ・`local` / `pushed` / `merged` 状態を同期・表示 |
| 破棄 | `session remove [<name>] [--force]` | worktree・ブランチ・コピーを削除（未コミット変更があれば `--force` 必須） |

`session` のサブコマンドは短縮形を受け付けます（`create`=`c`/`new`、`list`=`ls`、`remove`=`rm`）。

## アクティブなセッションと AI 連携

- `session switch` で選択したセッションが「アクティブ」になり、ホーム画面の左ペインで `*`（緑）と太字で強調されます。
- `terminal` / `agent` はアクティブな worktree（ルート行選択時はワークスペースルート）をカレントディレクトリに実行します。
- `agent` は設定の Agent CLI（`claude` / `gemini` など。[5. 設定](05-settings.md)）を埋め込みシェルで起動し、
  usagi の issue MCP サーバ（`usagi mcp`）と[セッション MCP サーバ](03-commands/05-session-mcp.md)（`usagi session-mcp`）を組み込みます。
  後者により、Agent 自身が `session_create` / `session_list` / `session_prompt` で並行セッションを作成し、
  別セッションのエージェントへタスクを委譲できます。ローカル LLM が有効なら `usagi llm-mcp` も組み込み、
  軽量タスクをローカル LLM へ委譲してクラウド Agent のトークン消費を抑えます（[3.4 ローカル LLM MCP サーバ](03-commands/04-llm-mcp.md)）。

各 worktree のシェル / Agent は「ターミナルプール」が worktree パスをキーに保持し、画面を開いている間は
セッションを切り替えても裏で動き続けます。`session remove` でセッションを破棄するときは、その worktree
パス配下のシェル / Agent もプールから取り除いて終了させます（worktree ディレクトリを消してもシェル自体は
生き続けるため）。これにより、同じ名前でセッションを作り直しても前回の Agent やその履歴を引き継がず、
常にまっさらなシェルから始まります。入力待ちになった（`◆ waiting`）／完了してアイドルになった（`⏸ idle`）セッションは
左ペインのマーカーとデスクトップ通知で知らされます。この埋め込みターミナルの永続化・切り替え・通知の詳細は
[design/05-home.md](design/05-home.md#没入のキー操作attached--terminal--agent-実行中) を参照してください。

### Agent フックによる状態報告

`claude` で起動した Agent が「稼働中（running）」「入力待ち（waiting）」「完了（ended）」のどれかを正確に判定するため、usagi は
起動コマンドに `--settings` でライフサイクルフックを差し込みます（MCP サーバや system prompt と同様、起動時に
インラインで渡す）。フックは Agent 自身の状態遷移ごとに `usagi agent-phase <phase>` を実行し、対象 worktree
の phase を記録します。

| フックイベント | 記録する phase | 意味 |
|---|---|---|
| `UserPromptSubmit` | `running` | プロンプト送信＝ターン開始 |
| `Stop` / `Notification` | `waiting` | ターン終了・許可待ち＝入力待ち |
| `SessionStart` | `waiting` | 起動・再開直後の入力待ち |
| `SessionEnd` | `ended` | Agent 終了＝完了（素のシェルは残る） |

- フックは payload を stdin で受け取り、usagi はそこから `cwd`（Agent を起動した worktree）を読んで対象を特定
  します。phase は `~/.usagi/agent-state/` 配下の worktree 別ファイルに記録され、ホーム画面の監視スレッドが
  読み取って左ペインの `▶ running` / `◆ waiting` / `⏸ idle` を駆動します（描画と検知の詳細は
  [design/05-home.md#使用中-agent-の表示入力待ちの検知と通知](design/05-home.md#使用中-agent-の表示入力待ちの検知と通知)）。
- `--settings` は**ユーザー自身の設定に追加マージ**されるため、既存の Claude 設定を壊しません。
- フックを持たない Agent（`gemini` など）はこの仕組みの対象外で、入力待ちは従来のターミナルベルで推定します。
- フック・MCP サーバが呼び戻す `usagi` は、`$PATH` 上の名前ではなく **usagi 自身の実行ファイルの絶対パス**
  （`std::env::current_exe()` で解決）を埋め込みます。これにより、インストール済みでも `cargo run` のように
  ビルド成果物（`target/debug/usagi`）を直接起動した場合でも、`usagi mcp` / `usagi agent-phase` が
  `command not found` にならず解決できます（パスが取得できない場合のみ素の名前 `usagi` にフォールバック）。
