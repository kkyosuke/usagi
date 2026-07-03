# 3.2 TUI 内コマンド

> [コマンドリファレンス目次](README.md) ｜ ← 前へ [CLI コマンド](01-cli.md)

`usagi hop` のホーム画面で実行する TUI 内コマンドの一覧です。本書は**コマンドの構文と役割**に絞ります。
画面のモード・キー操作・スクロール方針・入力待ち通知などの画面側の挙動は
[design/home/README.md](../design/home/README.md) が正本です。

## 入力面とスコープ

コマンドの入力面は**物理的に 2 つ**あります。各コマンドは自分のスコープ＋共通コマンドだけに現れます
（補完・`man` 一覧もそのスコープに限定）。表示だけでなく**実行もそのスコープに限定**され、スコープ外のコマンドは
手で打ってもエラーになり実行されません。

| 入力面 | スコープ | 出るコマンド |
|---|---|---|
| コマンドパレット（統括 / Overview。`:` で開く） | Workspace（全体） | `session` / `unite` / `issue` / `config` / `env` / `preview` / `diff` |
| 在席（Focus）の右ペイン | Session（個別） | `agent` / `close` / `terminal`（Menu はコマンド名のアルファベット順に並べる） |
| 両方 | 共通 | `man` / `history` / `clear` / `quit` |

ワークスペース全体のコマンドは、切替（Switch）・在席（Focus）から `:`（コロン）で開く**コマンドパレット**（中央オーバーレイ）で実行します。
入力欄に応じた候補・ヒント（コマンド一覧の絞り込み、または引数入力中の `usage` / `examples`）が
表示され、`Tab` でキャレット位置の語（コマンド名／引数のサブコマンド・オプション・既存のセッション名）を補完、`↑↓` で履歴を遡れます。`session switch`・`session remove` の `<name>` 位置では現在のセッション名が補完候補になります（統合（unite）モードの `session remove` では `workspace:session` 形式の修飾名が候補になり、`workspace:` まで打って `Tab` でそのワークスペースのセッションに絞り込めます）。フッターに現在のスコープ（`[palette]` / `[session: <名前>]`）が出ます。
モード遷移・キー操作の詳細は [design/home/01-modes.md](../design/home/01-modes.md#コマンドスコープ) を参照してください。

## コマンド一覧

| コマンド | 説明 |
|---|---|
| `man` / `help` | `man` でコマンド一覧、`man <command>` で個別の書式（Usage）と例（Examples）をスクロール可能なテキストモーダルで表示 |
| `history [session]` | 入力したコマンドの履歴を時系列にテキストモーダルで表示。各行に実行時刻・成功/失敗・セッション名・コマンドを併記し、`session` を指定するとそのセッションの履歴だけに絞り込む |
| `clear` | 出力ログを消去 |
| `quit` / `exit` | usagi を終了してプロジェクト一覧へ戻る |
| `session` | セッション（branch + worktree）の作成・一覧・切替・削除（Workspace スコープ） |
| `unite` | 統合(unite)ビューにワークスペースを追加・削除（Workspace スコープ） |
| `issue` | タスク issue を一覧・依存ツリー・ガント・1 件表示で閲覧（Workspace スコープ） |
| `preview <path\|name>` | Markdown ファイルを右ペインにレンダリング表示（Workspace スコープ） |
| `diff` | 選択中セッションの差分（既定ブランチとの差分）を右ペインに色付き表示（Workspace スコープ） |
| `terminal [open\|new]` | 選択中セッションの worktree でシェルを開く（Session スコープ）。引数なし / `open` は右ペインに埋め込みタブを追加、`new` は同じディレクトリで OS ネイティブの新規ターミナルを開く |
| `agent [名前]` | `terminal` ＋ Agent CLI を起動（Session スコープ）。引数なしは設定中の既定 CLI を起動。名前（`claude` / `codex` / `codex-fugu` / `sakana.ai` / `gemini`）を付けるとその CLI を起動する |
| `close [--force]` | 在席中のセッションを削除し、完了まで他の操作が無ければ隣のセッションへ在席する（`session remove <名前>` と同じ。既定では未コミット変更があれば削除を拒否し `--force` の案内をログに出す。`--force` / `-f` で未コミット変更を破棄。Session スコープ） |
| `config` | 現在のワークスペースのローカル設定を編集する Config 画面を開く（Workspace スコープ） |
| `env` | Config 画面を開き、そのまま workspace-env エディタ（1Password 参照の環境変数）へ入る（Workspace スコープ） |

> `man` / `help`、`quit` / `exit` はそれぞれ別名（同じ動作）です。

## session

セッション（ワークスペース配下の各 git リポジトリに同名ブランチの worktree を張る作業単位）を操作します。
**`:` で開くコマンドパレット**で実行します。サブコマンドは短縮形を受け付けます（`create`=`c`/`new`、`list`=`ls`、`remove`=`rm`）。

| サブコマンド | 動作 |
|---|---|
| `session create <name>` | `.usagi/sessions/<name>/` 配下に再帰的に worktree を構築してセッションを作成。完了まで他の操作が無ければ新規セッションへ自動で在席。名前を省くと[切替](../design/home/02-layout.md#切替switch既定)の左ペイン内インライン入力で作成 |
| `session list` | セッション一覧（件数 + 各セッション名 + worktree 数）をテキストモーダルに表示 |
| `session switch <name>` | アクティブセッションを切り替えて**在席**へ。`switch root` でルート行へ。引数なしで[切替](../design/home/02-layout.md#切替switch既定)モードを開く |
| `session remove <name> [--force]` | セッションの worktree・ブランチ・コピーに加え、その worktree の会話履歴（Claude の transcript / Codex の rollout / Gemini の chats）と Agent phase も削除。未コミット変更があれば警告し `--force` で破棄。名前を省くと一覧モーダルを開き、`Space` で選択して `Enter` で一括削除。統合（unite）モードでは名前を `workspace:session`（コロン区切り・空白なし）で修飾して、ワークスペース間で重複する名前を一意に指定できる（無修飾の名前は表示中のワークスペースを先頭から探して最初に一致したものを対象にする）。一覧モーダルは表示中の全ワークスペースのセッションを `workspace: session` 形式で並べる |

セッション作成・削除時の孤児ディレクトリの掃除など、ライフサイクルの概念は
[4. オーケストレーション](../04-orchestration.md)を参照してください。

## unite

**`:` で開くコマンドパレット**から、いま開いているホーム画面に**別の登録済みワークスペースを足し引き**します（統合 / unite モード。[design/home/03-sidebar.md#統合uniteモードの積み重ね表示](../design/home/03-sidebar.md#統合uniteモードの積み重ね表示)）。

| コマンド | 役割 |
|---|---|
| `unite add <workspace>` | 登録済みワークスペース（`workspaces.json` の名前）を統合ビューに追加し、サイドバーにそのグループを積む。既に表示中・未登録の名前は拒否してログに出す |
| `unite remove <workspace>`（別名 `rm`） | 統合ビューからそのワークスペースを外す。最後の 1 つを外すと単一ワークスペース表示へ戻る |

Open 画面の複数選択で開く導線は [design/02-open.md#統合uniteモードで開く](../design/02-open.md#統合uniteモードで開く) を参照してください。

## issue

ワークスペースのタスク issue（[data/03-issues.md](../data/03-issues.md)）を**読み取り専用**で閲覧します。**`:` で開くコマンドパレット**で実行し、結果はスクロール可能なテキストモーダルに出ます。issue の作成・更新はエージェントが MCP 経由で行う前提のため、TUI からは閲覧のみです。画面を開いた時点の内容を表示します。

| サブコマンド | 動作 |
|---|---|
| `issue` / `issue list`（別名 `ls`） | 全 issue を ready/blocked/done 付きで一覧し、末尾に進捗サマリ（件数・完了率・ready 数・バー）を表示 |
| `issue graph`（別名 `tree`） | `dependson` の依存ツリーを進捗サマリ付きで表示。各ノードの先頭に状態グリフ（`✓` done / `○` ready / `⊘` blocked）を付け、完了行は淡色（dim）、ブロック行は赤で描いて未完了の作業を際立たせる |
| `issue gantt`（別名 `chart`） | 各 issue の `created_at`→`updated_at` を実日付軸のガントチャートで表示。バーの字形でステータス（`█` done / `▒` in-progress / `░` todo）を、各行末の `←依存`（`!` は未完了）で依存関係を表す |
| `issue show <番号>`（別名 `view`） | 1 件の frontmatter + 本文を表示 |

issue が 1 件も無いときは「No issues yet.」を 1 行だけログに出します。

## preview

ワークスペース内の Markdown ファイルを**右ペインにレンダリング表示**します。**`:` で開くコマンドパレット**で実行します。

| 書式 | 動作 |
|---|---|
| `preview <path>` | パス指定（例 `preview document/design/home/README.md`）でそのファイルを表示 |
| `preview <name>` | 拡張子なしの名前（例 `preview README`）は Markdown 拡張子（`.md` / `.markdown`）を補って解決 |

- ファイルはワークスペースルートを基点に解決し、**ルート外へは出られません**（絶対パス・`..` での親への離脱は拒否）。
- 読めないパスや存在しないファイルはエラーをログに出します。
- 引数なしは `usage` をログに出します。
- `preview diff` は [`diff`](#diff) コマンドの別名で、差分ビューを開きます（`diff.md` の表示は試みません）。
- 巨大なファイルでも UI を止めないよう、**先頭 512 KiB まで**を読み込み、超過分は切り詰めて末尾に省略行を出します（レンダリング行数にも上限あり）。

レンダリングは Markdown のサブセット（見出し・箇条書き／番号付きリスト・引用・`**強調**`／`*斜体*`／`` `コード` ``／リンク）を色付けして表示します。**フェンスドコードブロック（` ``` ` / `~~~`）は、開きフェンスの言語トークン（例 ` ```rust `）に応じてシンタックスハイライト**します（[syntect](https://github.com/trishume/syntect) によるトークン化を端末の 256 色へマッピング。`sh`／`yml`／`ts` などの別名も解決し、言語トークンが無い／未知のときはプレーン表示にフォールバック。コード行のタブはタブ幅 4 でスペース展開）。

## diff

選択中セッションの**差分を右ペインに色付き表示**します（別名 `preview diff`）。**`:` で開くコマンドパレット**で実行します。サイドバーの[差分バッジ](../design/home/03-sidebar.md#差分バッジ)（`+N -M`）が示す差分そのものの全文で、追加行は緑・削除行は赤・ハンク見出し（`@@`）は水色・ファイル見出しは淡色で描きます。

| 書式 | 動作 |
|---|---|
| `diff` | カーソル中セッションの worktree の、既定ブランチとの累積差分（`git diff --merge-base`）を表示 |

- 差分の基準は worktree のリポジトリの既定ブランチ（`origin/<既定>` を先に解決し、無ければローカル）。コミット済みと未コミットの両方を含みます（差分バッジと同じ範囲）。
- **プレビューと同じ右ペイン**を使い、`↑↓` / `j` `k`・`PageUp` / `PageDown`・`Space` でスクロール、`Esc` / `q` / `Enter` で閉じます。
- セッションを選んでいない（ルート行）ときや基準ブランチを解決できないときは、その旨をログに出して何も開きません。差分が無いときは「No changes」を表示します。

## terminal

**在席の右ペイン**から実行します。書式は `terminal [open|new]` です。選択中の worktree（先頭の**ルート行**を選んでいればワークスペースルート）を
作業ディレクトリにします。引数なし、または `terminal open` は、対話型シェルを**右ペインに埋め込んで**起動し**没入**へ移ります（疑似ターミナル: portable-pty + vt100）。
左ペインの一覧は表示したままなので、シェルを操作しながらセッションを見渡せます。

`terminal new` は、同じ作業ディレクトリを OS ネイティブの新規ターミナルアプリで開きます。usagi の右ペインには埋め込まず、実行後も在席に留まります。在席 Menu では `terminal` 行を `→` / `Tab` で展開して `open`（既定）/ `new` を選べます。

没入中のキー操作（切替・在席へのズームアウト、タブの追加/切替/クローズ、メモ編集、終了など）は**[キー方式（`key_scheme`）](../05-settings.md#設定項目)**で決まり、既定の `prefix` 方式ではリーダー `Ctrl-O` の次キーで操作します。予約キーの全一覧・`alt` 方式・スクロール・マウスでのテキスト選択とコピー・端末ごとの差異・[直前のセッションへ切り替え](../design/home/03-sidebar.md#直前のセッションへ切り替えctrl-)は [design/home/04-keys.md](../design/home/04-keys.md#没入のキー操作attached--terminal--agent-実行中) が正本です。シェルは画面を開いている間プールに常駐し、行き来しても終了しません。

## agent

`terminal` と同じ埋め込みシェルを開いたうえで、Agent CLI を**シェルの引数として渡して**起動します（stdin にタイプしないので
長い起動コマンド行がペインにエコーされません）。実質 `terminal` → `claude` のショートカットで、ルート行選択時はワークスペース
ルートで起動します。Agent CLI を終了すると埋め込みシェルもそのまま終了し、素のシェルプロンプトに落ちずに[在席](../design/home/01-modes.md#各モードの説明)へ戻ります。

どの Agent CLI を起動するかは、引数で**そのセッションだけ**上書きできます。

- 引数なし（在席 Menu の `agent` 行 / `a`、Prompt の `agent`）: 設定中の**既定 CLI**（ローカル設定で `gemini` などに変更可）を起動。
- 名前付き（Prompt の `agent codex` / `agent sakana.ai`、または在席 Menu の[エージェントピッカー](../design/home/02-layout.md#在席のアクション-uimenu--prompt)）: 指定した CLI を起動。名前は起動コマンド名（`claude` / `codex` / `codex-fugu` / `gemini`）と表示名（`sakana.ai`）を大文字小文字を問わず受け付ける。
- 既定 CLI 以外でかつ**インストールされていない**（PATH に無い）名前を指定するとエラーになり起動しない。未知の名前も同様に拒否する。Menu のピッカーは**インストール済みの CLI だけ**を候補に出す。

**1 セッションが持てる agent は 1 つだけ**です。すでに agent ペインがあるセッションでは、在席の **Menu から `agent` 行を外します**（切替プレビューも同様）。Prompt の `agent`・`a`・没入の agent-タブ追加キー `Ctrl-O g`／`Alt-g` から `agent` を実行しても 2 つ目を起動せず、**既存の agent タブへ移動**します（terminal タブは何枚でも追加できます）。

起動時に usagi 自身の issue MCP サーバ（[`usagi mcp`](03-mcp.md)）を Agent CLI に組み込むため、エージェントは起動直後から
`issue_*` tool でタスクを操作できます。さらにローカル LLM が有効なら [`usagi llm-mcp`](04-llm-mcp.md) も組み込みます。
Agent CLI ごとの組み込み方法（Claude は `--mcp-config` / `--append-system-prompt`、Codex（および Codex 互換の `codex-fugu`）は `-c` 設定上書き（MCP＋ライフサイクルフック）、Gemini はインライン注入フラグが無いため MCP・フック・system prompt は組み込まず、再開と初期プロンプトのみ配線）は
[3.4 ローカル LLM MCP サーバ](04-llm-mcp.md#起動と登録)を参照してください。
Codex / `codex-fugu` では、usagi が組み込む MCP サーバに `default_tools_approval_mode = "approve"` も渡すため、MCP tool 呼び出しごとの確認は出ません（シェルコマンドの承認は `--ask-for-approval on-request` のままです）。

対象 worktree に前回の会話が残っている場合は、**前回セッションの続きから**起動します（Claude は `claude --continue`、
Codex は `codex resume --last`（`codex-fugu` も同様に `codex-fugu resume --last`）、Gemini は `gemini -r latest`。中断・離席後も文脈を引き継いで再開できます）。過去の会話が無ければ通常起動になります。判定は worktree ごとに行い、
再開フラグは埋め込みシェルを**新規に起動するときだけ**付与されます（裏で動き続けるシェルへ再アタッチする場合は再起動しないため対象外）。
なお Codex は、キュー済みプロンプト（`session_prompt`）がある起動では再開せず、そのプロンプトで新規セッションを開始します
（Claude / Gemini は再開とプロンプトを併用でき、Gemini はプロンプトを `gemini -i <prompt>` で渡します）。

入力待ちの検知・`◆ waiting` マーカー・デスクトップ通知の挙動は
[design/home/04-keys.md](../design/home/04-keys.md#使用中-agent-の表示入力待ちの検知と通知) を参照してください。

## close

**在席の右ペイン**から実行します。在席中のセッションを削除します。`session remove <名前>`
と同じ挙動で、そのセッションの worktree・ブランチ・コピーを削除します。既定の `close` は未コミット変更があれば削除を拒否し、
`--force` の案内をログに出します。`close --force`（短縮 `close -f`）は `session remove <名前> --force` と同じく未コミット変更を破棄します。
削除を投げた直後はいったん切替へ出ますが、削除完了まで他の操作が無ければ、閉じたセッションの 1 つ上（上に無ければ下）のセッションへ自動で在席します。
移動先にライブなペインがあれば既存のアクティブペインをプレビューし、無ければアクション UI を表示します。
削除待ち中に別の操作をした場合は、一覧だけを更新して現在の操作先を保ちます。
在席の Menu では `close` をカーソル＋`Enter` で（未コミット変更を保護する既定の挙動）、`Shift`+`c`（大文字 `C`）で `close --force`（未コミット変更を破棄）を起動できます。
ルート行はワークスペースそのものでセッションではないため `close` できません。在席の Menu ではルート行で `close` を出さず、
Prompt から打ってもエラーをログに出して在席に留まります。削除そのものはバックグラウンドのコールバックが行い、孤児ディレクトリの掃除など
ライフサイクルの概念は [4. オーケストレーション](../04-orchestration.md) を参照してください。

## config

Config（設定）画面を**ワークスペーススコープ**で開き、現在のワークスペースのローカル設定
（`<workspace>/.usagi/settings.json`）のみを編集します。グローバル設定は CLI（`usagi config`）または起動画面の Config で
編集します。`Esc` / `q` でホーム画面へ復帰、`Ctrl+C` でアプリ終了。設定項目は [5. 設定](../05-settings.md)、
画面は [design/04-config.md](../design/04-config.md) を参照してください。

## env

このワークスペースの 1Password 参照の環境変数（`NAME=op://vault/item/field`）を編集します。**コマンドパレット（Overview）
の上に重なる複数行エディタのオーバーレイ**として開き、Config 画面へは遷移しません — `Ctrl-S`（保存）でも `Esc`（取消）でも
オーバーレイを閉じて **元の Overview に戻ります**。`Ctrl-S` を押すと、有効な binding（`=` を含み・変数名が有効・reference が
空でない行。同名は後勝ち）を `<workspace>/.usagi/settings.json` の `env` に書き込みます（保存する値は 1Password reference
だけで、解決した secret 本体は保存しません）。同じ binding は Config 画面（`config`）の `Env Vars` 行からも編集できます。
オーバーレイは binding 数によって高さが変わらない固定サイズで描画します。設定項目 `env` は [5. 設定](../05-settings.md#設定項目)、
画面は [design/home/05-overlays.md](../design/home/05-overlays.md#workspace-env-エディタ) を参照してください。
