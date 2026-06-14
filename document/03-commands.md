# 3. コマンドリファレンス

> [ドキュメント目次](README.md) ｜ ← 前へ [2. アーキテクチャ](02-architecture.md) ｜ 次へ → [4. オーケストレーション](04-orchestration.md)

`usagi` のコマンドは **CLI コマンド**（シェルから `usagi <cmd>` で実行）と **TUI 内コマンド**
（`usagi hop` 起動後、ホーム画面のコマンドモードで実行）の 2 系統に分かれます。本書では両方を
一覧し、実装状況（✅ 実装済み / 🚧 予定）を明示します。予定コマンドの詳細仕様は
[../issues/README.md](../issues/README.md) の各 issue を参照してください。

## 目次

- [CLI コマンド](#cli-コマンド)
  - [実装済みの CLI コマンド](#実装済みの-cli-コマンド)
  - [予定の CLI コマンド](#予定の-cli-コマンド)
- [TUI 内コマンド](#tui-内コマンド)
- [凡例](#凡例)

## CLI コマンド

### 実装済みの CLI コマンド

| コマンド | 説明 | 状態 |
|---|---|---|
| `usagi init` | カレントディレクトリをプロジェクトとして登録する（`.usagi/` を初期化し、グローバルレジストリ `workspaces.json` に追加） | ✅ |
| `usagi init --git <URL>` | カレントディレクトリ配下に `<リポジトリ名>/` を作成して clone し、プロジェクトとして登録する | ✅ |
| `usagi hop` | メインの TUI を起動する。起動画面 → プロジェクト選択 → ホーム画面へ遷移（[design/](design/README.md)） | ✅ |
| `usagi status` | カレントリポジトリの worktree 状態を `.usagi/state.json` に同期し一覧表示する（[data/02-workspace.md](data/02-workspace.md)） | ✅ |
| `usagi config` | 現在のグローバル設定（`settings.json`）を一覧表示する（[5. 設定](05-settings.md)） | ✅ |
| `usagi config --edit` | グローバル設定ファイルを `$EDITOR` で開いて編集し、保存時に形式（JSON / 必須 `version` / 型）を検証する。不正な場合は直前の内容に巻き戻す | ✅ |
| `usagi doctor` | Git / Bash / AWS CLI / Node.js / Python などの依存ツールの導入状況を確認する | ✅ |
| `usagi doctor --fix` | 不足ツールを OS のパッケージマネージャ（brew / apt-get / dnf / pacman）で導入を試行し、修復不可なら手動手順を提示する | ✅ |

#### `usagi init`

カレントディレクトリ（または `--git` 指定時はクローン先）を usagi のワークスペースとして登録します。

- `.usagi/` を初期化し、グローバルレジストリ `~/.usagi/workspaces.json` にエントリを追加。
- `.gitignore` に `.usagi/` を無視する設定を追記。
- `--git <URL>` 指定時は、カレントディレクトリ配下に `<リポジトリ名>/` を作って `git clone` してから登録。

#### `usagi hop`

TUI を起動します。代替スクリーン上で起動画面を表示し、Open / New / Config / Quit を選べます。
画面遷移とキー操作は [design/README.md](design/README.md) を参照してください。

#### `usagi status`

`git worktree list` などを読み取り専用で検査し、`<repo>/.usagi/state.json` を同期したうえで、
各 worktree のブランチ・HEAD・`local` / `pushed` / `merged` 状態を一覧表示します。

#### `usagi config`

usagi の設定ファイル（グローバルな `settings.json`、`~/.usagi/` または `$USAGI_HOME` 配下）を扱います。

- 引数なし: 現在の設定を `key  value` 形式で一覧表示します。
- `--edit`: 設定ファイルを `$EDITOR`（→ `$VISUAL` → OS 既定の `vi` / `notepad`）で開いて編集します。
  保存後に再パースして形式（JSON 構文・必須 `version`・各フィールドの型）を検証し、不正な場合は
  **編集前の内容へ巻き戻して** エラーを表示するため、設定ミスで usagi が壊れません。

#### `usagi doctor`

依存ツールの導入状況を診断します。システムの `git` などを読み取り専用で確認し、ユーザーの
環境設定を尊重します。

`--fix` を付けると、`missing` の依存ツールを OS に合わせたパッケージマネージャでの導入を試行します
（macOS: `brew install`、Linux: 利用可能な `sudo apt-get` / `dnf` / `pacman` を優先順に選択）。
自動修復できない場合（パッケージマネージャ未検出・インストール失敗）は、手動インストール手順を提示します。
不足が無ければ何もしません。

### 予定の CLI コマンド

usagi.ai から移植予定の CLI コマンドです（[../issues/README.md](../issues/README.md)）。

| コマンド | 説明 | issue | 状態 |
|---|---|---|---|
| `usagi sync` | origin の既定ブランチの最新を、現在のセッションへ `rebase` / `merge` で取り込む | [009](../issues/009-sync.md) | 🚧 |
| `usagi finish` / `submit` | 現在のセッションを main へ統合し、worktree を削除する。`--pr` で PR 作成も可 | [010](../issues/010-finish.md) | 🚧 |
| `usagi list` | 全セッション（worktree）を ahead/behind とともに俯瞰表示する | [011](../issues/011-list.md) | 🚧 |
| `usagi logs` | コマンド実行履歴（`history.json`）を閲覧・検索する | [013](../issues/013-logs.md) | 🚧 |
| `usagi clean` | 古い・統合済みのセッションを整理する | [014](../issues/014-clean.md) | 🚧 |
| `usagi context` | AI エージェントに読み込ませるプロジェクトコンテキストを生成・出力する | [016](../issues/016-context.md) | 🚧 |
| `usagi init-agent` | `.clinerules` / `CLAUDE.md` などのエージェント設定ファイルを初期化する | [017](../issues/017-init-agent.md) | 🚧 |
| `usagi alias` | コマンドエイリアスを定義する | [018](../issues/018-alias.md) | 🚧 |
| gh Issue 連携 | GitHub Issue からセッションを作成する | [020](../issues/020-gh-issue.md) | 🚧 |

## TUI 内コマンド

`usagi hop` のホーム画面でコマンドモード（`:` または `i`）に入って実行します。`Tab` で補完、
`↑↓` で履歴を遡れます。画面側の挙動は [design/05-home.md](design/05-home.md) を参照してください。

| コマンド | 説明 | issue | 状態 |
|---|---|---|---|
| `man` / `help` | コマンド一覧、または `man <command>` で個別の説明を表示 | [008](../issues/008-man.md) | ✅ |
| `history` | 入力したコマンドの履歴を番号付きで表示 | [007](../issues/007-history.md) | ✅ |
| `clear` | 右ペインの出力ログを消去 | — | ✅ |
| `quit` / `exit` | アプリを終了 | — | ✅ |
| `session` | `session <name>`（または `session new <name>`）でセッション（`.usagi/worktree/<name>/` 配下に再帰的に worktree を構築）を作成。名前省略時は名前入力モーダルを表示。`session list` で一覧表示。`session remove <name> [--force]` で削除（未コミット変更があれば警告し、`--force` で破棄） | [003](../issues/003-session.md) | ✅ 実装済み |
| `space` | アクティブなワークスペース（worktree）の切り替え | [004](../issues/004-space.md) | 🚧 |
| `ai` | 選択中の Agent CLI を起動し、現在の worktree をコンテキストに AI へ指示・対話する | [005](../issues/005-ai.md) | 🚧 |
| `terminal` | アクティブな worktree で対話型ターミナルを起動する | [006](../issues/006-terminal.md) | 🚧 |
| `doctor` | 依存関係チェック（TUI 版） | [019](../issues/019-doctor-fix.md) | 🚧 |
| `diff` | TUI Diff ビューア（セッションの差分閲覧） | [012](../issues/012-diff.md) | 🚧 |

> 🚧 のコマンドはホーム画面で名前としては認識されますが、本体は未実装で「coming soon」を表示します
> （`session` / `space` / `ai` / `terminal` / `doctor`）。`session` / `space` / `ai` などが司る
> worktree オーケストレーションの全体像は [4. オーケストレーション](04-orchestration.md) を参照してください。

## 凡例

- **✅ 実装済み**: 現在のビルドで動作する。
- **🚧 予定**: usagi.ai から移植予定。詳細仕様は対応する issue を参照。
