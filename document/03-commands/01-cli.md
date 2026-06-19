# 3.1 CLI コマンド

> [コマンドリファレンス目次](README.md) ｜ 次へ → [TUI 内コマンド](02-tui.md)

シェルから `usagi <cmd>` で実行する CLI コマンドの一覧です。

`issue` / `memory` / `mcp` / `llm-mcp` は **AI エージェントが MCP 経由で扱うためのコマンド**で、`usagi --help` の一覧には表示しません（実行自体は可能）。人手で叩くものではないため、ヘルプを汚さないよう隠しています。

## 目次

- [CLI コマンド一覧](#cli-コマンド一覧)
  - [`usagi issue`](#usagi-issue)
  - [`usagi memory`](#usagi-memory)
  - [`usagi mcp`](#usagi-mcp)
  - [`usagi llm-mcp`](#usagi-llm-mcp)

## CLI コマンド一覧

| コマンド | 説明 |
|---|---|
| `usagi init` | カレントディレクトリをプロジェクトとして登録する（`.usagi/` を初期化し、グローバルレジストリ `workspaces.json` に追加） |
| `usagi init --git <URL>` | カレントディレクトリ配下に `<リポジトリ名>/` を作成して clone し、プロジェクトとして登録する |
| `usagi hop` | メインの TUI を起動する。起動画面 → プロジェクト選択 → ホーム画面へ遷移（[design/](../design/README.md)） |
| `usagi status` | カレントリポジトリの worktree 状態を `.usagi/state.json` に同期し一覧表示する（[data/02-workspace.md](../data/02-workspace.md)） |
| `usagi config` | 現在のグローバル設定（`settings.json`）を一覧表示する（[5. 設定](../05-settings.md)） |
| `usagi config --edit` | グローバル設定ファイルを `$EDITOR` で開いて編集し、保存時に形式（JSON / 必須 `version` / 型）を検証する。不正な場合は直前の内容に巻き戻す |
| `usagi doctor` | `git` / `bash` の導入状況、デスクトップ通知の可否、設定ストレージの健全性を確認する（ローカル LLM 有効時は `ollama`・モデルも） |
| `usagi doctor --fix` | 不足ツールを OS のパッケージマネージャ（brew / apt-get / dnf / pacman）で導入を試行し、修復不可なら手動手順を提示する。ローカル LLM が有効なら `ollama`・サーバ起動・モデルも導入する |
| `usagi issue <create\|list\|graph\|show\|update\|search\|delete>` | （ヘルプ非表示・エージェント向け）ワークスペースのタスク issue（`.usagi/issues/`）を操作する。セッション内から実行してもワークスペースルートに解決し全セッションで共有（[data/02-workspace.md](../data/02-workspace.md#issues-タスク-issue)） |
| `usagi memory <save\|list\|show\|update\|search\|delete>` | （ヘルプ非表示・エージェント向け）カレントリポジトリのエージェントのメモリ（`.usagi/memory/`）を操作する（[data/04-memory.md](../data/04-memory.md)） |
| `usagi mcp` | （ヘルプ非表示・エージェント向け）issue・メモリ・セッションの操作を MCP（Model Context Protocol）サーバとして stdio で公開し、AI エージェントから使えるようにする |
| `usagi llm-mcp [--model <MODEL>]` | （ヘルプ非表示・エージェント向け）ローカル LLM（Ollama）を MCP サーバとして公開し、クラウド Agent が軽量タスクを委譲できるようにする（トークン節約） |

### `usagi init`

カレントディレクトリ（または `--git` 指定時はクローン先）を usagi のワークスペースとして登録します。

- `.usagi/` を初期化し、グローバルレジストリ `~/.usagi/workspaces.json` にエントリを追加。
- `.usagi/.gitignore` を生成してローカル状態を無視する設定を自己完結で書き込む（ただし共有対象の `.usagi/issues/` は追跡。リポジトリルートの `.gitignore` は汚さない。詳細は [data/02-workspace.md](../data/02-workspace.md#保存場所)）。
- `--git <URL>` 指定時は、カレントディレクトリ配下に `<リポジトリ名>/` を作って `git clone` してから登録。

### `usagi hop`

TUI を起動します。代替スクリーン上で起動画面を表示し、Open / New / Config / Quit を選べます。
画面遷移とキー操作は [design/README.md](../design/README.md) を参照してください。

### `usagi status`

`git worktree list` などを読み取り専用で検査し、`<repo>/.usagi/state.json` を同期したうえで、
各 worktree のブランチ・HEAD・`local` / `pushed` / `synced`（up to date）状態を一覧表示します。

### `usagi config`

usagi の設定ファイル（グローバルな `settings.json`、`~/.usagi/` または `$USAGI_HOME` 配下）を扱います。

- 引数なし: 現在の設定を `key  value` 形式で一覧表示します。
- `--edit`: 設定ファイルを `$EDITOR`（→ `$VISUAL` → OS 既定の `vi` / `notepad`）で開いて編集します。
  `$EDITOR="code --wait"` のように引数付きの値も POSIX シェル規則で分割して扱います（シェルは起動しません）。
  保存後に再パースして形式（JSON 構文・必須 `version`・各フィールドの型）を検証し、不正な場合は
  **編集前の内容へ巻き戻して** エラーを表示するため、設定ミスで usagi が壊れません。

### `usagi doctor`

依存ツールの導入状況を診断します。システムの `git` などを読み取り専用で確認し、ユーザーの
環境設定を尊重します。

`--fix` を付けると、`missing` の依存ツールを OS に合わせたパッケージマネージャでの導入を試行します
（macOS: `brew install`、Linux: 利用可能な `sudo apt-get` / `dnf` / `pacman` を優先順に選択）。
自動修復できない場合（パッケージマネージャ未検出・インストール失敗）は、手動インストール手順を提示します。
不足が無ければ何もしません。

ローカル LLM が有効な場合は、`ollama` 本体の導入に加えて Ollama サーバの起動を確認し、停止していれば
`ollama serve` をバックグラウンドで起動してからモデルを取得します（Homebrew 版 `ollama` はサーバを
常駐させないため、これがないとモデル取得や `local_llm_ask` が `could not connect to ollama server` で失敗します）。

### `usagi issue`

ワークスペースのタスク issue（`<workspace>/.usagi/issues/`、[data/02-workspace.md](../data/02-workspace.md#issues-タスク-issue)）を操作します。セッション内（`.usagi/sessions/<名>/…`）から実行しても**ワークスペースルートに解決**するため、全セッションが同じ 1 つの issue ストアを共有します（MCP / TUI と同じ正本。[data/03-issues.md#保存場所](../data/03-issues.md#保存場所)）。

| サブコマンド | 説明 |
|---|---|
| `create --title <T> [--priority <p>] [--label <L>…] [--depends-on <N>…] [--related <N>…] [--parent <N>] [--milestone <名>] [--body <md>]` | issue を作成し、採番した番号を表示 |
| `list [--status <s>] [--priority <p>] [--label <L>] [--parent <N>] [--milestone <名>] [--group-by <軸>] [--ready]` | 一覧表示。`--group-by` で軸ごとにグループ化（進捗付き）、`--ready` で着手可能な issue だけに絞り込む |
| `graph` | 依存ツリー（issue を依存先の下にネスト）を進捗サマリ付きで表示 |
| `show <番号>` | 1 件の frontmatter + 本文を表示 |
| `update <番号> [--title …] [--status …] [--priority …] [--label <L>…] [--depends-on <N>…] [--related <N>…] [--parent <N>\|--clear-parent] [--milestone <名>\|--clear-milestone] [--body …]` | 指定したフィールドだけを更新 |
| `search <クエリ> [--status …] [--priority …] [--label …] [--parent <N>] [--milestone <名>] [--ready]` | タイトル・本文を大文字小文字を無視して全文検索（ASCII 以外も含む Unicode 単位で照合） |
| `delete <番号> --yes` | issue を削除（`--yes` 必須） |

- `create` / `list` / `show` / `update` / `search` は `--json` を付けると機械可読な JSON を出力します（スクリプトや MCP 連携向け。`delete` / `graph` は対象外。`list --json` はグループ化せず配列を返す）。
- **関連の表現**: `--depends-on` はブロックする先行条件、`--related` はブロックしない緩い関連、`--parent` は所属（Epic ⊃ サブタスク）、`--milestone` は束ね。`update` の `--clear-parent` / `--clear-milestone` で解除します。
- **着手可能（ready）の可視化**: `list` / `search` は各 issue が ready かを示します。ready = `dependson` に挙げた issue が**すべて `done`** で、かつ自身が未 `done`。ブロック中の issue には未達の依存番号（`(blocked by 1, 3)`）を併記するので、いま着手できるタスクが一目で分かります。
- **グループ化・グラフ・進捗**: `--group-by` は `status` / `priority` / `milestone` / `parent` を受け付け、グループごとに見出しと進捗サマリ（件数・完了率・ready 数・バー）を出します。`graph` は `dependson` の依存ツリーを描き、ダイヤモンドや循環は一度だけ展開して `↑` を付けます。

```
$ usagi issue list
#1   done         high   done      認証基盤を実装
#2   todo         medium ready     ログイン画面
#3   todo         low    blocked   ログアウト  (blocked by 2)

$ usagi issue graph
#1 認証基盤を実装 [done]
└─ #2 ログイン画面 [todo]
   └─ #3 ログアウト [todo]

3 issues · 1 done (33%) · 1 ready  [######--------------]
```

### `usagi memory`

カレントリポジトリの AI エージェントのメモリ（`<repo>/.usagi/memory/`、[data/04-memory.md](../data/04-memory.md)）を操作します。issue がタスクを管理するのに対し、メモリはユーザーの好み・作業指針・プロジェクト固有の前提・外部リソースへのポインタといった、コードや git からは読み取れない事実を蓄積します。

| サブコマンド | 説明 |
|---|---|
| `save --name <名> --title <T> [--type <t>] [--related <名>…] [--body <md>]` | メモリを保存。**同名なら上書き**（in-place 更新）するので重複しない |
| `list [--type <t>]` | 一覧表示（`updated_at` の新しい順、`--type` でフィルタ） |
| `show <名>` | 1 件の frontmatter + 本文を表示 |
| `update <名> [--title …] [--type …] [--related <名>…] [--body …]` | 指定したフィールドだけを更新 |
| `search <クエリ> [--type <t>]` | 名前・タイトル・本文を大文字小文字を無視して全文検索（ASCII 以外も含む Unicode 単位で照合） |
| `delete <名> --yes` | メモリを削除（`--yes` 必須） |

- `--type` は `user` / `feedback` / `project` / `reference`（既定 `project`）。
- `--name` / `<名>` は与えた文字列をスラッグ化して識別子にします（例: `"User Prefers Tabs"` → `user-prefers-tabs`）。
- `save` / `list` / `show` / `update` / `search` は `--json` で機械可読な JSON を出力します（`delete` は対象外）。
- メモリを保存・更新・削除すると、目次 `MEMORY.md` と派生キャッシュ `index.json` が再生成されます。

```
$ usagi memory save --name "tabs" --title "ユーザーはタブを好む" --type user
saved tabs (user)

$ usagi memory list
user         tabs                     ユーザーはタブを好む
```

### `usagi mcp`

`usagi issue` / `usagi memory` と同じ issue・メモリ操作に加え、セッション操作（`session_create` / `session_list` / `session_prompt`）を、**MCP（Model Context Protocol）サーバ**として AI エージェント（Claude Code など）に stdio 経由で公開します。issue・memory・session の tool を 1 つの `usagi` サーバが提供します。アーキテクチャ・対応 tool・`session_prompt` の挙動・JSON-RPC プロトコルの詳細は専用の章 [3.3 MCP サーバ](03-mcp.md) を参照してください。

### `usagi llm-mcp`

ローカル LLM（Ollama）を **MCP サーバ**として公開し、クラウド Agent が要約・命名・定型文生成などの軽量タスクを `local_llm_ask` ツールで委譲できるようにします。`--model` で委譲先モデルを指定します（既定は `qwen2.5-coder:7b`）。設定での有効化・資材のインストール・対応 tool の詳細は専用の章 [3.4 ローカル LLM MCP サーバ](04-llm-mcp.md) を参照してください。
