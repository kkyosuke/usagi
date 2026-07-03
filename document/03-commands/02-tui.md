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
| コマンドパレット（統括 / Overview。`:` で開く） | Workspace（全体） | `session` / `unite` / `issue` / `config` / `preview` |
| 在席（Focus）の右ペイン | Session（個別） | `agent` / `ai` / `chat` / `close` / `terminal`（Menu は引数不要の `agent` / `chat` / `close` / `terminal` をコマンド名のアルファベット順に並べる。`ai <prompt>` は引数が要るので Prompt で入力する） |
| 両方 | 共通 | `man` / `history` / `clear` / `quit` |

ワークスペース全体のコマンドは、切替（Switch）・在席（Focus）から `:`（コロン）で開く**コマンドパレット**（中央オーバーレイ）で実行します。
入力欄に応じた候補・ヒント（コマンド一覧の絞り込み、または引数入力中の `usage` / `examples`）が
表示され、`Tab` でキャレット位置の語（コマンド名／引数のサブコマンド・オプション・既存のセッション名）を補完、`↑↓` で履歴を遡れます。`session switch`・`session remove` の `<name>` 位置では現在のセッション名が補完候補になります（統合（unite）モードの `session remove` では `workspace:session` 形式の修飾名が候補になり、`workspace:` まで打って `Tab` でそのワークスペースのセッションに絞り込めます）。フッターに現在のスコープ（`[palette]` / `[session: <名前>]`）が出ます。
モード遷移・キー操作の詳細は [design/home/01-modes.md](../design/home/01-modes.md#コマンドスコープ) を参照してください。

## コマンド一覧

| コマンド | 説明 |
|---|---|
| `man` / `help` | `man` でコマンド一覧、`man <command>` で個別の書式（Usage）と例（Examples）をスクロール可能なテキストモーダルで表示 |
| `history` | 入力したコマンドの履歴を番号付きでテキストモーダルに表示（過去セッション分も含む） |
| `clear` | 出力ログを消去 |
| `quit` / `exit` | usagi を終了してプロジェクト一覧へ戻る |
| `session` | セッション（branch + worktree）の作成・一覧・切替・削除（Workspace スコープ） |
| `unite` | 統合(unite)ビューにワークスペースを追加・削除（Workspace スコープ） |
| `issue` | タスク issue を一覧・依存ツリー・ガント・1 件表示で閲覧（Workspace スコープ） |
| `preview <path\|name>` | Markdown ファイルを右ペインにレンダリング表示（Workspace スコープ） |
| `terminal` | 選択中セッションの worktree でシェルを右ペインに埋め込み起動（Session スコープ） |
| `agent [名前]` | `terminal` ＋ Agent CLI を起動（Session スコープ）。引数なしは設定中の既定 CLI を起動。名前（`claude` / `codex` / `codex-fugu` / `sakana.ai` / `gemini`）を付けるとその CLI を起動する |
| `ai <prompt>` | 設定中の既定 Agent CLI を選択中セッションの worktree で起動し、`<prompt>` を初期プロンプトとして渡す（Session スコープ。Prompt で入力する） |
| `chat` | ローカル LLM（Ollama 経由）と対話する専用画面を開く（Session スコープ）。外部 Agent CLI ではなくローカルモデルに直接話しかけるため、クラウド Agent のトークンを消費しない |
| `close [--force]` | 在席中のセッションを削除して切替へ移る（`session remove <名前>` と同じ。既定では未コミット変更があれば削除を拒否し `--force` の案内をログに出す。`--force` / `-f` で未コミット変更を破棄。Session スコープ） |
| `config` | 現在のワークスペースのローカル設定を編集する Config 画面を開く（Workspace スコープ） |

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
- `preview diff`（差分プレビュー）は未実装で、その旨を返します。
- 巨大なファイルでも UI を止めないよう、**先頭 512 KiB まで**を読み込み、超過分は切り詰めて末尾に省略行を出します（レンダリング行数にも上限あり）。

レンダリングは Markdown のサブセット（見出し・箇条書き／番号付きリスト・引用・`**強調**`／`*斜体*`／`` `コード` ``／リンク）を色付けして表示します。**フェンスドコードブロック（` ``` ` / `~~~`）は、開きフェンスの言語トークン（例 ` ```rust `）に応じてシンタックスハイライト**します（[syntect](https://github.com/trishume/syntect) によるトークン化を端末の 256 色へマッピング。`sh`／`yml`／`ts` などの別名も解決し、言語トークンが無い／未知のときはプレーン表示にフォールバック。コード行のタブはタブ幅 4 でスペース展開）。

## terminal

**在席の右ペイン**から実行します。選択中の worktree（先頭の**ルート行**を選んでいればワークスペースルート）を
作業ディレクトリに、対話型シェルを**右ペインに埋め込んで**起動し**没入**へ移ります（疑似ターミナル: portable-pty + vt100）。
左ペインの一覧は表示したままなので、シェルを操作しながらセッションを見渡せます。

没入中のキー操作（切替・在席へのズームアウト、タブの追加/切替/クローズ、メモ編集、終了など）は**[キー方式（`key_scheme`）](../05-settings.md#設定項目)**で決まり、既定の `prefix` 方式ではリーダー `Ctrl-O` の次キーで操作します。予約キーの全一覧・`alt` 方式・スクロール・マウスでのテキスト選択とコピー・端末ごとの差異・[直前のセッションへ切り替え](../design/home/03-sidebar.md#直前のセッションへ切り替えctrl-)は [design/home/04-keys.md](../design/home/04-keys.md#没入のキー操作attached--terminal--agent-実行中) が正本です。シェルは画面を開いている間プールに常駐し、行き来しても終了しません。

## agent

`terminal` と同じ埋め込みシェルを開いたうえで、Agent CLI を**シェルの引数として渡して**起動します（stdin にタイプしないので
長い起動コマンド行がペインにエコーされません）。実質 `terminal` → `claude` のショートカットで、ルート行選択時はワークスペース
ルートで起動します。Agent CLI を終了すると埋め込みシェルもそのまま終了し、素のシェルプロンプトに落ちずに[在席](../design/home/01-modes.md#各モードの説明)へ戻ります。

どの Agent CLI を起動するかは、引数で**そのセッションだけ**上書きできます。

- 引数なし（在席 Menu の `agent` 行 / `a`、Prompt の `agent`）: 設定中の**既定 CLI**（ローカル設定で `gemini` などに変更可）を起動。
- 名前付き（Prompt の `agent codex` / `agent sakana.ai`、または在席 Menu の[エージェントピッカー](../design/home/02-layout.md#在席のアクション-uimenu--prompt)）: 指定した CLI を起動。名前は起動コマンド名（`claude` / `codex` / `codex-fugu` / `gemini`）と表示名（`sakana.ai`）を大文字小文字を問わず受け付ける。
- 既定 CLI 以外でかつ**インストールされていない**（PATH に無い）名前を指定するとエラーになり起動しない。未知の名前も同様に拒否する。Menu のピッカーは**インストール済みの CLI だけ**を候補に出す。

**1 セッションが持てる agent は 1 つだけ**です。動作中の agent ペインがあるセッションでは、在席の **Menu から `agent` 行を外します**（切替プレビューも同様）。Prompt の `agent`・`a`・没入の agent-タブ追加キー `Ctrl-O g`／`Alt-g` から `agent` を実行しても 2 つ目を起動せず、**既存の agent タブへ移動**します（terminal タブは何枚でも追加できます）。CLI が終了したまま残っている agent ペインは「動作中」に数えず、次の `agent` / `ai` は新規起動になります。

起動時に usagi 自身の issue MCP サーバ（[`usagi mcp`](03-mcp.md)）を Agent CLI に組み込むため、エージェントは起動直後から
`issue_*` tool でタスクを操作できます。さらにローカル LLM が有効なら [`usagi llm-mcp`](04-llm-mcp.md) も組み込みます。
Agent CLI ごとの組み込み方法（Claude は `--mcp-config` / `--append-system-prompt`、Codex（および Codex 互換の `codex-fugu`）は `-c` 設定上書き（MCP＋ライフサイクルフック）、Gemini はインライン注入フラグが無いため MCP・フック・system prompt は組み込まず、再開と初期プロンプトのみ配線）は
[3.4 ローカル LLM MCP サーバ](04-llm-mcp.md#起動と登録)を参照してください。

対象 worktree に前回の会話が残っている場合は、**前回セッションの続きから**起動します（Claude は `claude --continue`、
Codex は `codex resume --last`（`codex-fugu` も同様に `codex-fugu resume --last`）、Gemini は `gemini -r latest`。中断・離席後も文脈を引き継いで再開できます）。過去の会話が無ければ通常起動になります。判定は worktree ごとに行い、
再開フラグは埋め込みシェルを**新規に起動するときだけ**付与されます（裏で動き続けるシェルへ再アタッチする場合は再起動しないため対象外）。
なお Codex は、キュー済みプロンプト（`session_prompt`）がある起動では再開せず、そのプロンプトで新規セッションを開始します
（Claude / Gemini は再開とプロンプトを併用でき、Gemini はプロンプトを `gemini -i=<prompt>` で渡します）。
キュー済みプロンプトはキューされた時点ではなく**配送された起動時**に在席のログへ 1 行表示されるため、あとから起動した agent が
何に取り組み始めたかを画面で確認できます。

入力待ちの検知・`◆ waiting` マーカー・デスクトップ通知の挙動は
[design/home/04-keys.md](../design/home/04-keys.md#使用中-agent-の表示入力待ちの検知と通知) を参照してください。

## ai

**在席の Prompt** から `ai <prompt>` と入力すると、設定中の**既定 Agent CLI**を選択中セッションの worktree で起動し、
`<prompt>` を初期プロンプトとして渡します。起動先のディレクトリ・MCP 配線・会話再開の扱いは [`agent`](#agent) と同じです。

- 動作中の agent ペインが無い場合（未起動、または CLI が終了済み）: 既定 CLI を新規起動し、`<prompt>` を CLI の初期プロンプト引数として渡す。プロンプトは `--` 区切り（Gemini は `-i=<prompt>`）で渡すため、`-` で始まるテキストもフラグと誤解釈されない。
- 動作中の agent ペインがある場合: そのペインのタブへ移動し、`<prompt>` を対話入力として送る（貼り付け＋Enter。bracketed paste 対応の CLI には 1 ブロックとして貼り付く）。送り先はそのペインで動いている CLI（`agent <名前>` で起動した既定以外の CLI でも同じ）で、新規起動はしないため次項のインストール判定も行わない。
- `<prompt>` を省くと `usage: ai <prompt>` を表示して起動しない。
- 新規起動になる場合で、インストール済み Agent CLI の検出結果（起動時にバックグラウンドで PATH を調べる）に既定 CLI が**含まれていない**ときは、Config でインストール済みの Agent CLI を選ぶよう案内して起動しない。検出が未完了、または 1 つも見つからなかったときは判定せず起動を試みる（`agent` と同じ寛容さ）。Config で既定 CLI を変更すると、Config を閉じた時点で `ai` / `agent` に反映される。

在席の **Menu** は引数を入力できないため `ai` 行を出しません。`ai` を使うワークスペースでは
[設定](../05-settings.md#設定項目)の `session_action_ui` を `prompt` にします。

## chat

**在席の右ペイン**から実行します。ローカル LLM（[設定](../05-settings.md#設定項目)の `local_llm.model` が指すモデルを
Ollama で提供）と対話する専用画面を全画面で開きます。`agent` / `ai` が外部 Agent CLI を worktree で起動するのに対し、`chat` は
ローカルモデルに直接話しかけるので、ちょっとした質問にクラウド Agent のトークンを使いません。

- 引数は取りません。在席の **Menu**（`agent` / `chat` / `close` / `terminal` をアルファベット順に並べる）でカーソル＋`Enter`、
  または Prompt に `chat` と打つと開きます。
- 画面では入力欄に書いて `Enter` で送信、`↑`/`↓` で履歴をスクロール、`Esc`（または `Ctrl-C`）で在席へ戻ります。応答を待つ間は
  スピナーを表示し、入力は応答が届くまで読み取り専用になります。
- モデル呼び出しはバックグラウンドスレッドで `ollama run` を実行します。Ollama サーバーが起動していなければ自動で立ち上げ、
  失敗（未インストール等）は会話内にエラーとして表示します。対話は 1 ターンごとにそれまでの会話を丸ごとモデルへ渡して文脈を保ちます。

## close

**在席の右ペイン**から実行します。在席中のセッションを削除します。`session remove <名前>`
と同じ挙動で、そのセッションの worktree・ブランチ・コピーを削除します。既定の `close` は未コミット変更があれば削除を拒否し、
`--force` の案内をログに出します。`close --force`（短縮 `close -f`）は `session remove <名前> --force` と同じく未コミット変更を破棄します。
削除が成功するとセッションは消えるので、次のセッションを選べるよう**切替**へ移ります（基底の切替なので `Esc` での戻り先はありません）。
在席の Menu では `close` をカーソル＋`Enter` で（未コミット変更を保護する既定の挙動）、`Shift`+`c`（大文字 `C`）で `close --force`（未コミット変更を破棄）を起動できます。
ルート行はワークスペースそのものでセッションではないため `close` できません。在席の Menu ではルート行で `close` を出さず、
Prompt から打ってもエラーをログに出して在席に留まります。削除そのものはバックグラウンドのコールバックが行い、孤児ディレクトリの掃除など
ライフサイクルの概念は [4. オーケストレーション](../04-orchestration.md) を参照してください。

## config

Config（設定）画面を**ワークスペーススコープ**で開き、現在のワークスペースのローカル設定
（`<workspace>/.usagi/settings.json`）のみを編集します。グローバル設定は CLI（`usagi config`）または起動画面の Config で
編集します。`Esc` / `q` でホーム画面へ復帰、`Ctrl+C` でアプリ終了。設定項目は [5. 設定](../05-settings.md)、
画面は [design/04-config.md](../design/04-config.md) を参照してください。
