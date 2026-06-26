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
- [ペインの復旧](#ペインの復旧)

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
  worktree 作成直後に submodule を初期化・チェックアウトする（`git submodule update --init --recursive` 相当）。
  submodule を持つリポジトリではそのまま作業でき、持たないリポジトリ（`.gitmodules` がない）では何もしない。
  破棄時もこの worktree を問題なく取り除く（git は submodule を含む worktree の削除を素の `git worktree remove` では一律拒否するが、クリーンであることを確認した上で内部的に強制削除する。未コミット変更があれば従来どおり `--force` が必要）。
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

セッション名は `session create <name>` の引数で渡すほか、名前を省くと[切替（Switch）モード](design/05-home.md#切替switch既定)の
左ペイン内インライン入力で指定できます。次の名前はバリデーションで弾かれます。

- **空文字・パス区切り**（`/` `\` `.` `..`）を含む名前。
- **`-` で始まる**名前。セッション名は各リポジトリで `<name>` ブランチ名になり git コマンドの引数に渡るため、先頭が `-` だと git にオプション（`-D` など）と誤認される。
- **既存セッションと重複**する名前。
- **既存ブランチの名前空間と衝突**する名前。セッションは各リポジトリで `<name>` ブランチを切るため、
  すでに `<name>/…`（例: `test/foo`）配下にブランチがあると git が `<name>` ブランチを作れない。
  この場合は作成前に衝突しているブランチ名を示して中断する（別のセッション名を選ぶ）。

## 新ブランチの基点（local / remote）

新しい `<name>` ブランチを**どの基点から切るか**は、各リポジトリのローカル設定 `default_branch`（基点ブランチ）
と `default_branch_source`（その基点を `local` 形・`remote` 形のどちらで解決するか）で決まります。**各設定の
意味・既定値・選択肢・フォールバック順は
[05-settings.md#ローカル設定（プロジェクト単位の上書き）](05-settings.md#ローカル設定プロジェクト単位の上書き) が正本**です。

設定は**リポジトリ単位**です。複数リポジトリ構成では `session create` 実行時に各リポジトリの
`<repo>/.usagi/settings.json` をそれぞれ参照し、リポジトリごとに異なる基点で worktree を切れます。基点解決は
`infrastructure/git.rs` の `resolve_base_ref`、適用は `usecase/session` の `create` / `build_dir` が担います。

## state.json との同期（孤児セッションの掃除）

`session create` / `session remove` の実行時に、`.usagi/sessions/` 配下のディレクトリと `state.json` の記録を照合します。**`state.json` に記録のないディレクトリ**（中断された作成・手で編集された `state.json`・クラッシュなどで取り残されたもの）は「孤児」とみなし、**未コミット変更の有無にかかわらず強制削除**して同期を取ります（worktree の登録解除・セッションブランチの削除・コピーしたファイルの除去）。

- これにより、作成時は同名の取り残しディレクトリが新規セッションの作成を妨げません。
- セッションの掃除では、そのセッションパス配下にある worktree を**ブランチ名が一致するかどうかに依らず**登録解除します。これがないと、想定外のブランチに切り替わった worktree（例: セッション内で別ブランチを切ったもの）はディレクトリだけ削除され、**git の worktree 登録だけが取り残されて**しまいます。
- 加えて `session create` は、各リポジトリで**実体ディレクトリの消えた worktree 登録（dangling 登録）を作成前に `git worktree prune` で掃除**します。これがないと、同じセッションパスに対する `git worktree add` が「missing but already registered worktree」で失敗し、同名セッションを二度と作れなくなります。
- 記録済みセッション本体の削除には引き続き未コミット変更のガード（`--force` 必須）が効きます。掃除されるのは **記録のない** ディレクトリだけです。
- 逆に、**`state.json` に記録はあるが worktree 実体が無い**セッション（作成が途中で中断され worktree が構築されなかった、あるいは手で消されたもの）の削除も滞りません。git の worktree 登録解除が対象不在で失敗しても、その後始末はスキップして記録自体は確実に取り除きます。
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
| 切替 | `session switch [<name>]` | アクティブなセッションを切り替え（引数なしで[切替](design/05-home.md#切替switch既定)モード） |
| 作業 | `terminal` / `agent` | アクティブな worktree でシェル / Agent CLI を右ペインに埋め込み起動 |
| 状態確認 | `usagi status` | 各 worktree のブランチ・`local` / `pushed` / `merged` 状態を同期・表示 |
| 破棄 | `session remove [<name>] [--force]` | worktree・ブランチ・コピー・会話履歴・Agent phase を削除（未コミット変更があれば `--force` 必須） |

`session` のサブコマンドは短縮形を受け付けます（`create`=`c`/`new`、`list`=`ls`、`remove`=`rm`）。

## アクティブなセッションと AI 連携

- `session switch` で選択したセッションが「アクティブ」になり、ホーム画面の左ペインで `*`（緑）と太字で強調されます。
- `terminal` / `agent` はアクティブな worktree（ルート行選択時はワークスペースルート）をカレントディレクトリに実行します。
- `agent` は設定の Agent CLI（`claude` / `codex` / `gemini` など。[5. 設定](05-settings.md)）を埋め込みシェルで起動し、
  usagi の MCP サーバ（`usagi mcp`。[3.3 MCP サーバ](03-commands/03-mcp.md)）を組み込みます。
  これにより Agent 自身が、issue / memory の操作に加えて `session_create` / `session_list` / `session_prompt` /
  `session_remove` で並行セッションを作成・委譲・整理できます（別セッションのエージェントへのタスク委譲・不要セッションの削除）。ローカル LLM が有効なら
  `usagi llm-mcp` も組み込み、軽量タスクをローカル LLM へ委譲してクラウド Agent のトークン消費を抑えます
  （[3.4 ローカル LLM MCP サーバ](03-commands/04-llm-mcp.md)）。
- `agent`（Claude / Codex / Gemini）は対象 worktree に前回の会話が残っていれば**前回セッションの続きから**起動し
  （Claude は `claude --continue`、Codex は `codex resume --last`、Gemini は `gemini -r latest`）、無ければ通常起動します。
  Codex は再開とキュー済みプロンプトが両立しない（`codex resume` の位置引数プロンプトが `--last` と衝突する）ため、
  プロンプトがキューされている場合は再開せずそのプロンプトで新規起動します。Claude / Gemini は再開とプロンプトを併用できます
  （Gemini はプロンプトを `gemini -i <prompt>` で渡す。挙動の正本は [3.2 TUI 内コマンド#agent](03-commands/02-tui.md#agent)）。

各 worktree のシェル / Agent は「ターミナルプール」が worktree パスをキーに保持し、画面を開いている間は
セッションを切り替えても裏で動き続けます。`session remove` でセッションを破棄するときは、その worktree
パス配下のシェル / Agent もプールから取り除いて終了させ、さらに**その worktree の会話履歴も削除します**
（Claude の transcript ディレクトリ `~/.claude/projects/<encoded>`、Codex の rollout transcript
`~/.codex/sessions/.../rollout-*.jsonl` のうち当該 worktree のもの、Gemini の chat transcript
`~/.gemini/tmp/<project>/chats` のうち当該 worktree のもの、および usagi が記録する Agent phase
`~/.usagi/agent-state/`）。worktree ディレクトリを消してもシェルや会話履歴は別の場所に残るため、明示的に消します。
これにより、同じ名前・同じパスでセッションを作り直しても前回の Agent・会話・状態を一切引き継がず、常にまっさらな
状態から始まります（`--continue` で復活する古い会話も残りません）。入力待ちになった（`◆ waiting`）／終了した（`✓ done`）セッションは
左ペインのマーカーとデスクトップ通知で知らされます。この埋め込みターミナルの永続化・切り替え・通知の詳細は
[design/05-home.md](design/05-home.md#没入のキー操作attached--terminal--agent-実行中) を参照してください。

### Agent フックによる状態報告

`claude` / `codex` で起動した Agent が「起動直後（ready）」「稼働中（running）」「入力待ち（waiting）」「完了（ended）」のどれかを正確に判定するため、usagi は
起動コマンドにライフサイクルフックを差し込みます（MCP サーバや system prompt と同様、起動時に
インラインで渡す。Claude は `--settings`、Codex は `-c hooks.<Event>` 設定上書き）。フックは Agent 自身の状態遷移ごとに `usagi agent-phase <phase>` を実行し、対象 worktree
の phase を記録します。フックの payload は Claude / Codex とも同じ形（stdin の JSON に `cwd` と `source` を含む）なので、`usagi agent-phase` は CLI ごとの分岐なしに動きます。

| フックイベント | 記録する phase | 意味 |
|---|---|---|
| `SessionStart` | `ready` | 起動・再開直後＝プロンプト未投入の待機（ただしターン中の再開は例外、下記） |
| `UserPromptSubmit` | `running` | プロンプト送信＝ターン開始 |
| `PreToolUse` | `running` | ツール実行直前＝ターン中に稼働している（下記） |
| `PostToolUse` | `running` | ツール実行直後＝ターン中に稼働している（下記） |
| `Notification` | `waiting` | ターン中に**ユーザーの入力・許可を待って**停止（質問・ツール承認） |
| `PermissionRequest` | `waiting` | **ツール使用の許可プロンプト**が出た（下記） |
| `Stop` | `ended` | **ターン完了**＝Agent の実行が終わった |
| `SessionEnd` | `ended` | Agent プロセス終了（素のシェルは残る） |

- フックは payload を stdin で受け取り、usagi はそこから `cwd`（Agent を起動した worktree）を読んで対象を特定
  します。phase は `~/.usagi/agent-state/` 配下の worktree 別ファイルに記録され、ホーム画面の監視スレッドが
  読み取って左ペインの `☾ ready` / `▶ running` / `◆ waiting` / `✓ done` を駆動します（描画と検知の詳細は
  [design/05-home.md#使用中-agent-の表示入力待ちの検知と通知](design/05-home.md#使用中-agent-の表示入力待ちの検知と通知)）。
- `SessionStart` は起動・再開だけでなく**コンテキストのコンパクション後**にも発火します。自動コンパクションは**ターンの途中**でも起こり、その後 Agent は新たな `UserPromptSubmit` なしに作業を続けます。これを一律 `ready` にすると、稼働中のセッションが次の `Stop` まで `☾ ready` のまま固まってしまうため、usagi は次のいずれかに当たる `SessionStart` では **phase を書き換えず現状を維持**します（ターン中なら `running` のまま、待機中ならそのまま）。
  - payload の `source` が `compact`（明示的なコンパクション後の再開）。
  - 記録済みの phase が `running` / `waiting`（＝ターンの途中）。新規 spawn のたびに phase ファイルはクリアされる（[data/01-global.md](data/01-global.md) 参照）ため、**真の起動なら記録済み phase は無い**。途中にもかかわらず `ready` が来たのはターン中の再開（`source` を伴わないコンパクションや、`source` を読めなかった payload を含む）であり、`source: compact` の判定だけでは取りこぼすケースもこの条件で守られます。
- ツール使用の許可プロンプトも入力待ちですが、`Notification` はユーザーが**離席している**ときにしか発火しないため、それだけでは見ているセッションの許可待ちを取りこぼします。専用の `PermissionRequest` フックを `waiting` に割り当て、**プロンプト表示の直前に・フォーカス中でも**確実に `◆ waiting` へ遷移させます（観測のみで許可可否の判定には介入しません）。
- `running` を駆動するのは `UserPromptSubmit` だけではありません。**ターン中のツール呼び出し（`PreToolUse` / `PostToolUse`）も `running` に割り当て**ます。これは `◆ waiting` への貼り付きを解消するためです。ユーザーが質問に答えたり許可を承認したりして Agent が作業を再開しても、新たな `UserPromptSubmit` は発火しないので、`waiting` のままでは「実際は稼働中なのに `◆ waiting`」になってしまいます。再開後の最初のツール呼び出しで `PreToolUse` / `PostToolUse` が発火し、セッションを `▶ running` へ引き戻します。これらのフックは**ターンの途中でしか発火しない**ため、アイドル中のセッションを誤って `running` にすることはありません。
- 割り当てを見送ったフック: `SubagentStop`（サブエージェントの終了は本体ターンの終了ではない。本体は作業を続けており、`Task` ツールの `PostToolUse` が `running` を保つ）、`PreCompact` / `PostCompact`（コンパクションは上記の `SessionStart` ガードで処理され、再開後のツール呼び出しが改めて `running` を主張する）。
- `--settings` は**ユーザー自身の設定に追加マージ**されるため、既存の Claude 設定を壊しません。
- **Codex** も同じ仕組みで phase を報告します。上表のうち Codex が持つイベントは `SessionStart` / `UserPromptSubmit` / `PreToolUse` / `PostToolUse` / `PermissionRequest` / `Stop` で、`ready` / `running` / `waiting` / `ended` の割り当ては Claude と同一です（Codex には `Notification` / `SessionEnd` イベントが無く、`ended` は `Stop` が担います）。`SessionStart` の `source`（`startup` / `resume` / `clear` / `compact`）も Claude と同じ値なので、コンパクションガードもそのまま機能します。Codex のフックは**信頼されていない command フック**として扱われ既定では実行前に承認を求めるため、usagi は `--dangerously-bypass-hook-trust` を付けて起動します（フックが実行するのは usagi 自身のみ）。
- フックを持たない Agent（`gemini` など）はこの仕組みの対象外で、入力待ちは従来のターミナルベルで推定します。
- フック・MCP サーバが呼び戻す `usagi` は、`$PATH` 上の名前ではなく **usagi 自身の実行ファイルの絶対パス**
  （`std::env::current_exe()` で解決）を埋め込みます。これにより、インストール済みでも `cargo run` のように
  ビルド成果物（`target/debug/usagi`）を直接起動した場合でも、`usagi mcp` / `usagi agent-phase` が
  `command not found` にならず解決できます（パスが取得できない場合のみ素の名前 `usagi` にフォールバック）。

## ペインの復旧

ターミナルプールが保持するシェル / Agent は**画面を開いている間だけ**生きていて、usagi を終了すると
プロセスごと破棄されます。次回起動時にそれらを呼び戻すのが**ペインの復旧**で、設定
[`restore_panes_enabled`](05-settings.md#設定項目)（既定 ON）で切り替えます。

- **保存**: 各セッションのペイン構成（タブ順のペイン種別＝agent / terminal と、agent ペインはどの CLI か、
  どのタブがアクティブだったか）を worktree 別のスナップショットに記録します（保存フォーマットは
  [data/01-global.md#open-panesペイン復旧スナップショット](data/01-global.md#open-panesペイン復旧スナップショット)）。
  記録はペインを開閉してそのペインから制御が戻るたびに更新され、ペインが 1 つも無くなったセッションの
  スナップショットは消去されます。`session remove` でセッションを破棄すると、その worktree のスナップショットも
  併せて消えます（同名・同パスで作り直しても前回のペインは復活しません）。
- **復旧**: 起動直後、まだどのペインも attach していない段階で、各セッションのスナップショットを読み、
  記録されたペインをバックグラウンドで spawn します。terminal ペインは素のシェルを開き直し、agent ペインは
  記録された CLI を起動します。このとき対象 worktree に前回の会話が残っていれば**前回の続きから再開**します
  （`agent` 起動時の再開ロジックと同じ。上記「アクティブなセッションと AI 連携」参照）。復旧されたペインは
  監視スレッドが拾うため、attach しなくても左ペインのバッジ（`▶ running` / `◆ waiting` / `✓ done`）が動きます。
- 復旧は**ベストエフォート**です。スナップショットが無いセッションや、spawn に失敗したペインは黙って飛ばし、
  画面の起動を妨げません。設定を OFF にすると保存も復旧も行わず、常にまっさらな状態で起動します。

### 復帰フォーカス（いた場所の復元）

ペインの復旧が「どのペインが開いていたか」を呼び戻すのに対し、**復帰フォーカス**は「終了時にユーザーが
*どこにいたか*」を呼び戻します。同じ [`restore_panes_enabled`](05-settings.md#設定項目) で一括制御します
（両者で 1 つの「セッション状態を復元する」機能）。

- **保存**: 終了が確定した時（quit 確認モーダルの承認、即時 Ctrl-C、`:quit`）に、カーソルがあったセッションと
  その**エンゲージメント段階**（切替 / 在席 / 没入）をワークスペース別のスナップショットに記録します（保存
  フォーマットは [data/01-global.md#resume-focus復帰フォーカススナップショット](data/01-global.md#resume-focus復帰フォーカススナップショット)）。
  没入中の `Ctrl-Q` は確認モーダルへ抜ける際に在席へ降格するため、降格前に「没入だった」ことを記録しておきます。
- **復旧**: 起動時、ペインの復旧が済んだ後に読み出し、記録された段階へ戻します。切替ならカーソルをそのセッションへ、
  在席ならそのセッションを在席に、没入ならイベントループの初回パスで自動的に attach します（このときペインは
  既に復旧済みなので生きており、attach できます）。
- 復旧は**ベストエフォート**です。記録されたセッションが既に消えている（`session remove` された）場合は何も
  復元せず、既定の切替で起動します。設定を OFF にすると保存も復旧も行いません。
