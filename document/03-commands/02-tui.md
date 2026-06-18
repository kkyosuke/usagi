# 3.2 TUI 内コマンド

> [コマンドリファレンス目次](README.md) ｜ ← 前へ [CLI コマンド](01-cli.md)

`usagi hop` のホーム画面で実行する TUI 内コマンドの一覧です。本書は**コマンドの構文と役割**に絞ります。
画面のモード・キー操作・スクロール方針・入力待ち通知などの画面側の挙動は
[design/05-home.md](../design/05-home.md) が正本です。

## 入力面とスコープ

コマンドの入力面は**物理的に 2 つ**あります。各コマンドは自分のスコープ＋共通コマンドだけに現れます
（補完・`man` 一覧もそのスコープに限定）。

| 入力面 | スコープ | 出るコマンド |
|---|---|---|
| 統括（Overview）の下部コマンドライン | Workspace（全体） | `session` / `issue` / `config` |
| 在席（Focus）の右ペイン | Session（個別） | `terminal` / `agent` / `close` |
| 両方 | 共通 | `man` / `history` / `clear` / `quit` |

入力欄の直上には入力中の内容に応じた候補・ヒント（コマンド一覧の絞り込み、または引数入力中の `usage` / `examples`）が
表示され、`Tab` で補完、`↑↓` で履歴を遡れます。フッターに現在のスコープ（`[workspace]` / `[session: <名前>]`）が出ます。
モード遷移・キー操作の詳細は [design/05-home.md](../design/05-home.md#コマンドスコープ) を参照してください。

## コマンド一覧

| コマンド | 説明 |
|---|---|
| `man` / `help` | `man` でコマンド一覧、`man <command>` で個別の書式（Usage）と例（Examples）をスクロール可能なテキストモーダルで表示 |
| `history` | 入力したコマンドの履歴を番号付きでテキストモーダルに表示（過去セッション分も含む） |
| `clear` | 出力ログを消去 |
| `quit` / `exit` | アプリを終了 |
| `session` | セッション（branch + worktree）の作成・一覧・切替・削除（Workspace スコープ） |
| `issue` | タスク issue を一覧・依存ツリー・1 件表示で閲覧（Workspace スコープ） |
| `terminal` | 選択中セッションの worktree でシェルを右ペインに埋め込み起動（Session スコープ） |
| `agent` | `terminal` ＋ Agent CLI（既定 `claude`）を起動（Session スコープ） |
| `close` | 在席中のセッションを強制削除して切替へ移る（`session remove <名前> --force` と同じ。Session スコープ） |
| `config` | 現在のワークスペースのローカル設定を編集する Config 画面を開く（Workspace スコープ） |

> `man` / `help`、`quit` / `exit` はそれぞれ別名（同じ動作）です。

## session

セッション（ワークスペース配下の各 git リポジトリに同名ブランチの worktree を張る作業単位）を操作します。
**統括の下部コマンドライン**で実行します。サブコマンドは短縮形を受け付けます（`create`=`c`/`new`、`list`=`ls`、`remove`=`rm`）。

| サブコマンド | 動作 |
|---|---|
| `session create <name>` | `.usagi/sessions/<name>/` 配下に再帰的に worktree を構築してセッションを作成。名前を省くと[切替](../design/05-home.md#切替switch)の左ペイン内インライン入力で作成 |
| `session list` | セッション一覧（件数 + 各セッション名 + worktree 数）をテキストモーダルに表示 |
| `session switch <name>` | アクティブセッションを切り替えて**在席**へ。`switch root` でルート行へ。引数なしで[切替](../design/05-home.md#切替switch)モードを開く |
| `session remove <name> [--force]` | セッションの worktree・ブランチ・コピーを削除。未コミット変更があれば警告し `--force` で破棄。名前を省くと一覧モーダルを開き、`Space` で選択して `Enter` で一括削除 |

セッション作成・削除時の孤児ディレクトリの掃除など、ライフサイクルの概念は
[4. オーケストレーション](../04-orchestration.md)を参照してください。

## issue

ワークスペースのタスク issue（[data/03-issues.md](../data/03-issues.md)）を**読み取り専用**で閲覧します。**統括の下部コマンドライン**で実行し、結果はスクロール可能なテキストモーダルに出ます。issue の作成・更新はエージェントが MCP 経由で行う前提のため、TUI からは閲覧のみです。画面を開いた時点の内容を表示します。

| サブコマンド | 動作 |
|---|---|
| `issue` / `issue list`（別名 `ls`） | 全 issue を ready/blocked/done 付きで一覧し、末尾に進捗サマリ（件数・完了率・ready 数・バー）を表示 |
| `issue graph`（別名 `tree`） | `dependson` の依存ツリーを進捗サマリ付きで表示 |
| `issue show <番号>`（別名 `view`） | 1 件の frontmatter + 本文を表示 |

issue が 1 件も無いときは「No issues yet.」を 1 行だけログに出します。

## terminal

**在席の右ペイン**から実行します。選択中の worktree（先頭の**ルート行**を選んでいればワークスペースルート）を
作業ディレクトリに、対話型シェルを**右ペインに埋め込んで**起動し**没入**へ移ります（疑似ターミナル: portable-pty + vt100）。
左ペインの一覧は表示したままなので、シェルを操作しながらセッションを見渡せます。

没入中は **`Ctrl-O` だけが予約キー**で、`Esc` を含む他キーはすべてシェルへ流れます。`Ctrl-O` はリーダーキーで、続く 1 キーで
動作が決まります（`Ctrl-O Ctrl-O` で[切替](../design/05-home.md#切替switch)へズームアウト、`Ctrl-O t`/`a` で同一セッションに
terminal/agent ペインを追加、`Ctrl-O ]`/`[`/数字でタブ切替、`Ctrl-O w` でペインを閉じる）。シェルは画面を開いている間
プールに常駐し、行き来しても終了しません。
没入中のキー操作・スクロール・マウスでのテキスト選択とコピー・端末ごとの差異は [design/05-home.md](../design/05-home.md#没入のキー操作attached--terminal--agent-実行中) を参照してください。

## agent

`terminal` と同じ埋め込みシェルを開いたうえで、設定中の Agent CLI（既定 `claude`、ローカル設定で `gemini` などに変更可）を
**シェルの引数として渡して**起動します（stdin にタイプしないので長い起動コマンド行がペインにエコーされません）。実質
`terminal` → `claude` のショートカットで、ルート行選択時はワークスペースルートで起動します。Agent CLI を終了すると埋め込みシェルもそのまま終了し、素のシェルプロンプトに落ちずに[在席](../design/05-home.md#各モードの説明)へ戻ります。

起動時に usagi 自身の issue MCP サーバ（[`usagi mcp`](03-mcp.md)）を Agent CLI に組み込むため、エージェントは起動直後から
`issue_*` tool でタスクを操作できます。さらにローカル LLM が有効なら [`usagi llm-mcp`](04-llm-mcp.md) も組み込みます。
Agent CLI ごとの組み込み方法（Claude は `--mcp-config` / `--append-system-prompt`、Gemini は現状素のまま）は
[3.4 ローカル LLM MCP サーバ](04-llm-mcp.md#起動と登録)を参照してください。

入力待ちの検知・`◆ waiting` マーカー・デスクトップ通知の挙動は
[design/05-home.md](../design/05-home.md#使用中-agent-の表示入力待ちの検知と通知) を参照してください。

## close

**在席の右ペイン**から実行します。在席中のセッションを強制削除します。`session remove <名前> --force`
と同じ挙動で、そのセッションの worktree・ブランチ・コピーを削除し、未コミット変更があっても破棄します。
削除が成功するとセッションは消えるので、次のセッションを選べるよう**切替**へ移ります（`Esc` で統括へ抜けます。
ルート行の在席など削除できない対象ではエラーをログに出して在席に留まります）。削除そのものはバックグラウンドのコールバックが行い、孤児ディレクトリの掃除など
ライフサイクルの概念は [4. オーケストレーション](../04-orchestration.md) を参照してください。

## config

Config（設定）画面を**ワークスペーススコープ**で開き、現在のワークスペースのローカル設定
（`<workspace>/.usagi/settings.json`）のみを編集します。グローバル設定は CLI（`usagi config`）または起動画面の Config で
編集します。`Esc` / `q` でホーム画面へ復帰、`Ctrl+C` でアプリ終了。設定項目は [5. 設定](../05-settings.md)、
画面は [design/04-config.md](../design/04-config.md) を参照してください。
