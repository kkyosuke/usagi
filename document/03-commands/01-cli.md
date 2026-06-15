# 3.1 CLI コマンド

> [コマンドリファレンス目次](README.md) ｜ 次へ → [TUI 内コマンド](02-tui.md)

シェルから `usagi <cmd>` で実行する CLI コマンドの一覧です。状態記号の凡例は
[README.md](README.md#凡例) を参照してください。

## 目次

- [実装済みの CLI コマンド](#実装済みの-cli-コマンド)
  - [`usagi issue`](#usagi-issue)
  - [`usagi mcp`](#usagi-mcp)
  - [`usagi llm-mcp`](#usagi-llm-mcp)
- [予定の CLI コマンド](#予定の-cli-コマンド)

## 実装済みの CLI コマンド

| コマンド | 説明 | 状態 |
|---|---|---|
| `usagi init` | カレントディレクトリをプロジェクトとして登録する（`.usagi/` を初期化し、グローバルレジストリ `workspaces.json` に追加） | ✅ |
| `usagi init --git <URL>` | カレントディレクトリ配下に `<リポジトリ名>/` を作成して clone し、プロジェクトとして登録する | ✅ |
| `usagi hop` | メインの TUI を起動する。起動画面 → プロジェクト選択 → ホーム画面へ遷移（[design/](../design/README.md)） | ✅ |
| `usagi status` | カレントリポジトリの worktree 状態を `.usagi/state.json` に同期し一覧表示する（[data/02-workspace.md](../data/02-workspace.md)） | ✅ |
| `usagi config` | 現在のグローバル設定（`settings.json`）を一覧表示する（[5. 設定](../05-settings.md)） | ✅ |
| `usagi config --edit` | グローバル設定ファイルを `$EDITOR` で開いて編集し、保存時に形式（JSON / 必須 `version` / 型）を検証する。不正な場合は直前の内容に巻き戻す | ✅ |
| `usagi doctor` | Git / Bash / AWS CLI / Node.js / Python などの依存ツールの導入状況を確認する | ✅ |
| `usagi doctor --fix` | 不足ツールを OS のパッケージマネージャ（brew / apt-get / dnf / pacman）で導入を試行し、修復不可なら手動手順を提示する。ローカル LLM が有効なら `ollama`・モデルも導入する | ✅ |
| `usagi issue <create\|list\|show\|update\|search\|delete>` | カレントリポジトリのタスク issue（`.usagi/issues/`）を操作する（[data/02-workspace.md](../data/02-workspace.md#issues-タスク-issue)） | ✅ |
| `usagi mcp` | issue 操作を MCP（Model Context Protocol）サーバとして stdio で公開し、AI エージェントから使えるようにする | ✅ |
| `usagi llm-mcp [--model <MODEL>]` | ローカル LLM（Ollama）を MCP サーバとして公開し、クラウド Agent が軽量タスクを委譲できるようにする（トークン節約） | ✅ |

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
各 worktree のブランチ・HEAD・`local` / `pushed` / `merged` 状態を一覧表示します。

### `usagi config`

usagi の設定ファイル（グローバルな `settings.json`、`~/.usagi/` または `$USAGI_HOME` 配下）を扱います。

- 引数なし: 現在の設定を `key  value` 形式で一覧表示します。
- `--edit`: 設定ファイルを `$EDITOR`（→ `$VISUAL` → OS 既定の `vi` / `notepad`）で開いて編集します。
  保存後に再パースして形式（JSON 構文・必須 `version`・各フィールドの型）を検証し、不正な場合は
  **編集前の内容へ巻き戻して** エラーを表示するため、設定ミスで usagi が壊れません。

### `usagi doctor`

依存ツールの導入状況を診断します。システムの `git` などを読み取り専用で確認し、ユーザーの
環境設定を尊重します。

`--fix` を付けると、`missing` の依存ツールを OS に合わせたパッケージマネージャでの導入を試行します
（macOS: `brew install`、Linux: 利用可能な `sudo apt-get` / `dnf` / `pacman` を優先順に選択）。
自動修復できない場合（パッケージマネージャ未検出・インストール失敗）は、手動インストール手順を提示します。
不足が無ければ何もしません。

### `usagi issue`

カレントリポジトリのタスク issue（`<repo>/.usagi/issues/`、[data/02-workspace.md](../data/02-workspace.md#issues-タスク-issue)）を操作します。

| サブコマンド | 説明 |
|---|---|
| `create --title <T> [--priority <p>] [--label <L>…] [--depends-on <N>…] [--body <md>]` | issue を作成し、採番した番号を表示 |
| `list [--status <s>] [--priority <p>] [--label <L>] [--ready]` | 一覧表示。`--ready` で着手可能な issue だけに絞り込む |
| `show <番号>` | 1 件の frontmatter + 本文を表示 |
| `update <番号> [--title …] [--status …] [--priority …] [--label <L>…] [--depends-on <N>…] [--body …]` | 指定したフィールドだけを更新 |
| `search <クエリ> [--status …] [--priority …] [--label …] [--ready]` | タイトル・本文を大文字小文字を無視して全文検索 |
| `delete <番号> --yes` | issue を削除（`--yes` 必須） |

- どのサブコマンドも `--json` を付けると機械可読な JSON を出力します（スクリプトや MCP 連携向け）。
- **着手可能（ready）の可視化**: `list` / `search` は各 issue が ready かを示します。ready = `dependson` に挙げた issue が**すべて `done`** で、かつ自身が未 `done`。ブロック中の issue には未達の依存番号（`(blocked by 1, 3)`）を併記するので、いま着手できるタスクが一目で分かります。

```
$ usagi issue list
#1   done         high   done      認証基盤を実装
#2   todo         medium ready     ログイン画面
#3   todo         low    blocked   ログアウト  (blocked by 2)
```

### `usagi mcp`

`usagi issue` と同じ issue 操作を、**MCP（Model Context Protocol）サーバ**として AI エージェント（Claude Code など）に stdio 経由で公開します。アーキテクチャ・対応 tool・JSON-RPC プロトコルの詳細は専用の章 [3.3 MCP サーバ](03-mcp.md) を参照してください。

### `usagi llm-mcp`

ローカル LLM（Ollama）を **MCP サーバ**として公開し、クラウド Agent が要約・命名・定型文生成などの軽量タスクを `local_llm_ask` ツールで委譲できるようにします。`--model` で委譲先モデルを指定します（既定は `qwen2.5-coder:7b`）。設定での有効化・資材のインストール・対応 tool の詳細は専用の章 [3.4 ローカル LLM MCP サーバ](04-llm-mcp.md) を参照してください。

## 予定の CLI コマンド

usagi.ai から移植予定の CLI コマンドです（[../../issues/README.md](../../issues/README.md)）。

| コマンド | 説明 | issue | 状態 |
|---|---|---|---|
| `usagi sync` | origin の既定ブランチの最新を、現在のセッションへ `rebase` / `merge` で取り込む | [009](../../issues/009-sync.md) | 🚧 |
| `usagi finish` / `submit` | 現在のセッションを main へ統合し、worktree を削除する。`--pr` で PR 作成も可 | [010](../../issues/010-finish.md) | 🚧 |
| `usagi list` | 全セッション（worktree）を ahead/behind とともに俯瞰表示する | [011](../../issues/011-list.md) | 🚧 |
| `usagi logs` | コマンド実行履歴（`history.json`）を閲覧・検索する | [013](../../issues/013-logs.md) | 🚧 |
| `usagi clean` | 古い・統合済みのセッションを整理する | [014](../../issues/014-clean.md) | 🚧 |
| `usagi context` | AI エージェントに読み込ませるプロジェクトコンテキストを生成・出力する | [016](../../issues/016-context.md) | 🚧 |
| `usagi init-agent` | `.clinerules` / `CLAUDE.md` などのエージェント設定ファイルを初期化する | [017](../../issues/017-init-agent.md) | 🚧 |
| `usagi alias` | コマンドエイリアスを定義する | [018](../../issues/018-alias.md) | 🚧 |
| gh Issue 連携 | GitHub Issue からセッションを作成する | [020](../../issues/020-gh-issue.md) | 🚧 |
