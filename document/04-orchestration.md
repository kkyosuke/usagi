# 4. オーケストレーション（セッション・worktree 管理）

> [ドキュメント目次](README.md) ｜ ← 前へ [3. コマンドリファレンス](03-commands/README.md) ｜ 次へ → [5. 設定](05-settings.md)

`usagi` の中核は、**複数の作業を worktree ベースの「セッション」として束ね、複数リポジトリ構成でも
一括でオーケストレーションする**ことです。本書はその概念モデルとライフサイクルをまとめます。各コマンドの
構文は [3. コマンドリファレンス](03-commands/README.md)、画面の操作は [design/home/README.md](design/home/README.md)、
永続化されるデータは [data/02-workspace.md](data/02-workspace.md) を参照してください。

## 目次

- [用語](#用語)
- [なぜ worktree を 1 か所に集約するのか](#なぜ-worktree-を-1-か所に集約するのか)
- [セッションの構築（再帰走査と複数リポジトリ対応）](#セッションの構築再帰走査と複数リポジトリ対応)
- [複数ワークスペースの統合（unite）](#複数ワークスペースの統合unite)
- [新ブランチの基点（local / remote）](#新ブランチの基点local--remote)
- [state.json との同期（孤児セッションの掃除）](#statejson-との同期孤児セッションの掃除)
- [スキルの配布](#スキルの配布)
- [セッションのライフサイクル](#セッションのライフサイクル)
- [アクティブなセッションと AI 連携](#アクティブなセッションと-ai-連携)
- [ペインの復旧](#ペインの復旧)
- [キュー済みプロンプトの自動起動](#キュー済みプロンプトの自動起動)

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
  `.usagi/sessions/<name>/<相対パス>/` に、新しい `usagi/<name>` ブランチを切って作成する。
  ブランチ名はセッション名 `<name>` を `usagi/` 名前空間に収めたもので、手で切った
  ブランチ（素の `<name>` や `feat/…` など）と衝突しないようにしている。worktree の
  ディレクトリ・セッション名・サイドバー表示は `usagi/` を付けない `<name>` のまま。
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
構造が再現され、git 配下の各サブディレクトリはそれぞれ `usagi/feature-x` ブランチの worktree、それ以外は
コピーになります。各 worktree の状態は `state.json` の該当セッション（`SessionRecord`）の
`worktrees` 配列（`WorktreeState`）に記録されます（`path` が `.usagi/sessions/<name>/...` を指す）。
データ構造は [data/02-workspace.md](data/02-workspace.md) を参照してください。

セッション名は `session create <name>` の引数で渡すほか、名前を省くと[切替（Switch）モード](design/home/02-layout.md#切替switch既定)の
左ペイン内インライン入力で指定できます。次の名前はバリデーションで弾かれます。

- **空文字・パス区切り**（`/` `\` `.` `..`）を含む名前。
- **`-` で始まる**名前。セッション名は worktree のパスや `usagi/<name>` ブランチ名の一部として git コマンドの引数に渡るため、先頭が `-` だと git にオプション（`-D` など）と誤認される。
- **既存セッションと重複**する名前。
- **既存ブランチの名前空間と衝突**する名前。セッションは各リポジトリで `usagi/<name>` ブランチを切るため、
  すでに `usagi/<name>/…` 配下にブランチがあると git が `usagi/<name>` ブランチを作れない。
  この場合は作成前に衝突しているブランチ名を示して中断する（別のセッション名を選ぶ）。
  なお `usagi/` 名前空間に収めることで、`<name>/…`（例: `test/foo`）のような**手で切った
  ブランチとは衝突しなくなる**。

## 複数ワークスペースの統合（unite）

usagi は**複数のワークスペースを 1 つのホーム画面にまとめて**操作できます（統合 / unite モード）。
[プロジェクト選択画面（Open）](design/02-open.md#統合uniteモードで開く)で `Space` により複数の
ワークスペースをチェックして `Enter` で同時に開くか、開いた後に[コマンドパレット](03-commands/02-tui.md#unite)の
`unite add` / `unite remove` で足し引きします。

- **1 つのワークスペース**を開けば従来どおりの単一ホーム。**2 つ以上**を開くと、左ペインは
  ワークスペースごとの**グループ**を積み重ねて表示します（各グループ＝ワークスペース名のヘッダ＋
  その `⌂ root` 行とセッション群）。見た目とキー操作は
  [design/home/03-sidebar.md#統合uniteモードの積み重ね表示](design/home/03-sidebar.md#統合uniteモードの積み重ね表示)が正本です。
- **コマンドの対象解決**: 新規セッション作成はカーソルがいるグループのワークスペースに作られ、削除・表示名・
  メモ・root 行の `terminal` / `agent` は対象セッション（行）が属するワークスペースに作用します。各ワークスペースは
  自分の `.usagi/`（`state.json` / `settings.json` / issue / memory）をそれぞれ持つため、セッションのライフサイクルは
  ワークスペース単位で独立しています。
- **直近の組み合わせの記憶**: 最後にまとめて開いたワークスペースの集合を保存し、次回 Open 画面で
  あらかじめチェックします（保存先は [data/01-global.md#unite-setjson直近の統合セット](data/01-global.md#unite-setjson直近の統合セット)）。
- 各ワークスペースの worktree は引き続きそれぞれの `.usagi/sessions/<name>/` に集約され、統合はあくまで
  **表示と操作を 1 画面に束ねる**ものです。ターミナル・Agent・状態監視は worktree の絶対パスをキーに動くため、
  複数ワークスペースが混在しても取り違えません。

## 新ブランチの基点（local / remote）

新しい `usagi/<name>` ブランチを**どの基点から切るか**は、各リポジトリのローカル設定 `default_branch`（基点ブランチ）
と `default_branch_source`（その基点を `local` 形・`remote` 形のどちらで解決するか）で決まります。**各設定の
意味・既定値・選択肢・フォールバック順は
[05-settings.md#ローカル設定（プロジェクト単位の上書き）](05-settings.md#ローカル設定プロジェクト単位の上書き) が正本**です。

設定は**リポジトリ単位**です。複数リポジトリ構成では `session create` 実行時に各リポジトリの
`<repo>/.usagi/settings.json` をそれぞれ参照し、リポジトリごとに異なる基点で worktree を切れます。基点解決は
`infrastructure/git.rs` の `resolve_base_ref`、適用は `usecase/session` の `create` / `build_dir` が担います。

## セッション作成後のセットアップコマンド

ワークスペースのローカル設定 `setup_commands` にコマンド列がある場合、`session create` は worktree / コピー済み
ファイル / 同梱スキルのリンクを作成したあと、セッション root（`<workspace>/.usagi/sessions/<name>`）を
カレントディレクトリとしてコマンドを保存順に実行します。設定の意味・保存形式・編集方法は
[05-settings.md#ローカル設定（プロジェクト単位の上書き）](05-settings.md#ローカル設定プロジェクト単位の上書き) が正本です。

- コマンドは 1 要素 = 1 shell コマンド行で、Unix 系では `sh -lc`、Windows では `cmd /C` で実行します。
- 空白だけのコマンドは実行しません。
- コマンドが失敗しても作成済みセッションは削除せず、エラーログとトレースログに記録して次のコマンドへ進みます。
  セッション自体はそのまま残るため、ユーザーは対象セッションを開いて原因を確認・修正できます。

## state.json との同期（孤児セッションの掃除）

`session create` / `session remove` の実行時に、`.usagi/sessions/` 配下のディレクトリと `state.json` の記録を照合します。**`state.json` に記録のないディレクトリ**（中断された作成・手で編集された `state.json`・クラッシュなどで取り残されたもの）は「孤児」とみなし、**未コミット変更の有無にかかわらず強制削除**して同期を取ります（worktree の登録解除・セッションブランチの削除・コピーしたファイルの除去）。

- これにより、作成時は同名の取り残しディレクトリが新規セッションの作成を妨げません。
- セッションの掃除では、そのセッションパス配下にある worktree を**ブランチ名が一致するかどうかに依らず**登録解除します。これがないと、想定外のブランチに切り替わった worktree（例: セッション内で別ブランチを切ったもの）はディレクトリだけ削除され、**git の worktree 登録だけが取り残されて**しまいます。
- 加えて `session create` は、各リポジトリで**実体ディレクトリの消えた worktree 登録（dangling 登録）を作成前に `git worktree prune` で掃除**します。これがないと、同じセッションパスに対する `git worktree add` が「missing but already registered worktree」で失敗し、同名セッションを二度と作れなくなります。
- 記録済みセッション本体の削除には引き続き未コミット変更のガード（`--force` 必須）が効きます。掃除されるのは **記録のない** ディレクトリだけです。
- 逆に、**`state.json` に記録はあるが worktree 実体が無い**セッション（作成が途中で中断され worktree が構築されなかった、あるいは手で消されたもの）の削除も滞りません。git の worktree 登録解除が対象不在で失敗しても、その後始末はスキップして記録自体は確実に取り除きます。
- セッションディレクトリ直下の単なるファイルは対象外です。

## スキルの配布

usagi はバイナリに**スキル**（Claude Code の `SKILL.md`）を同梱し、起動した Agent へ配布します。スキルは
`assets/skills/<name>/SKILL.md` としてビルド時に埋め込まれ、`infrastructure/skills.rs` が次の 2 段で届けます。

同梱スキルは次の通りで、`usagi-session` 以外は**機能（feature）単位**で ON/OFF できます（[設定](05-settings.md#設定項目)）。

| スキル | 機能 | 役割 |
|---|---|---|
| `usagi-session` | （なし・常時 ON） | セッション worktree での作業規約 |
| `usagi-pr-create` | `pull-request` | PR を新規作成する手順 |
| `usagi-pr-update` | `pull-request` | PR の概要更新・レビュー返信 |
| `usagi-pr-fix` | `pull-request` | レビュー対応・最新化・コンフリクト解消 |

```
バイナリ埋め込み (assets/skills/)
      │ TUI / MCP 起動時に materialize
      ▼
~/.usagi/skills/<name>/SKILL.md          ← スキルの唯一の実体（正本）
      ▲ symlink（session create 時、スキルごと）
<worktree>/.claude/skills/<name> ─────────┘
```

1. **展開（materialize）**: TUI（`hop`）・MCP サーバ（`mcp`）の起動時に、埋め込んだスキルを
   `~/.usagi/skills/`（[data/01-global.md#skillsagent-へ配布するスキル](data/01-global.md#skillsagent-へ配布するスキル) が正本）へ
   冪等に展開する。バイナリ更新後の再起動で内容が更新される。
2. **symlink（session create）**: セッション作成時、各 worktree の `.claude/skills/<name>` を上記の各スキルへの
   symlink として張る（ディレクトリ全体ではなく**スキルごと**）。worktree ごとにコピーせず、正本 1 か所を
   全 worktree が参照する。Agent は cwd（worktree）直下の `.claude/skills` から自動的にスキルを発見する。
   このとき、ワークスペースの**実効設定**（グローバル ⊕ ローカル上書き）で**機能が無効なスキルは symlink しない**。
   `materialize` は機能の ON/OFF に関わらず全スキルを展開するので、後から機能を ON にした新規セッションでは
   そのまま配布される。`usagi-session` は機能に属さず常に配布される。

- **プロジェクト独自のスキルと共存**: symlink はスキル単位で張るため、ユーザーが用意した
  `.claude/skills/<別名>` と usagi のスキルは同じディレクトリに並んで共存する。usagi が張るのは埋め込み
  スキルの名前のエントリだけで、**同名の実体（ファイル/ディレクトリ）が既にある場合は上書きせずそのまま残す**
  （古い usagi の symlink だけは現在の正本へ張り替える）。
- **git から隠す**: 各 symlink は git 管理外（untracked）なので、そのままだとセッションが
  「未コミット変更あり」と判定され `remove` / `finish` を妨げ、TUI 上もダーティ表示になる。これを避けるため、
  symlink を張ると同時に worktree のローカル除外（`$GIT_DIR/info/exclude`）へ `/.claude/skills/<name>` を
  スキルごとに追記する。除外はリポジトリローカルでコミット・push されず、ユーザーの追跡対象 `.gitignore`
  にもユーザー独自のスキルにも触れない（`infrastructure/git.rs` の `ensure_excluded`）。
- いずれの段もベストエフォートで、失敗してもセッション作成・起動は止めない。

## セッションのライフサイクル

セッションは「作成 → 作業 → 破棄」で完結します。各操作は[ホーム画面](design/home/README.md)の `session` /
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
| 切替 | `session switch [<name>]` | アクティブなセッションを切り替え（引数なしで[切替](design/home/02-layout.md#切替switch既定)モード） |
| 作業 | `terminal` / `agent` | アクティブな worktree でシェル / Agent CLI を右ペインに埋め込み起動 |
| 状態確認 | `usagi status` | 各 worktree のブランチ・`local` / `pushed` / `merged` 状態を同期・表示 |
| 破棄 | `session remove [<name>] [--force]` | worktree・ブランチ・コピー・会話履歴と、その worktree をキーにした usagi の一時ファイル（Agent phase・PR リンク・キュー済みプロンプト・ペイン構成）を削除（未コミット変更があれば `--force` 必須） |

`session` のサブコマンドは短縮形を受け付けます（`create`=`c`/`new`、`list`=`ls`、`remove`=`rm`）。

## アクティブなセッションと AI 連携

- `session switch` で選択したセッションが「アクティブ」になり、ホーム画面の左ペインで `*`（緑）と太字で強調されます。
- `terminal` / `agent` はアクティブな worktree（ルート行選択時はワークスペースルート）をカレントディレクトリに実行します。
- `agent` は設定の Agent CLI（`claude` / `codex` / `gemini` など。[5. 設定](05-settings.md)）を埋め込みシェルで起動し、
  usagi の MCP サーバ（`usagi mcp`。[3.3 MCP サーバ](03-commands/03-mcp.md)）を組み込みます。
  これにより Agent 自身が、issue / memory の操作に加えて `session_create` / `session_list` / `session_prompt` /
  `session_pr` / `session_remove` で並行セッションを作成・委譲（`session_prompt` の `mode` で起動時キュー / live agent への送信を選択）・PR 参照・整理できます（別セッションのエージェントへのタスク委譲・不要セッションの削除）。issue を新しいセッションへ丸ごと委譲する定番手順は `session_delegate_issue` の 1 呼び出しにまとまっています。委譲先の進捗はコーディネータが `session_status` でポーリングできるほか、子セッションのエージェントが `session_prompt` に予約名 `:root` を渡して**完了をルート行のコーディネータへ push で報告**できます（ポーリング間隔を待たずに完了が届く。[3.3 MCP サーバ#ルート行への push 型完了報告](03-commands/03-mcp.md#ルート行コーディネータへの-push-型完了報告)）。ローカル LLM が有効なら
  `usagi llm-mcp` も組み込み、軽量タスクをローカル LLM へ委譲してクラウド Agent のトークン消費を抑えます
  （[3.4 ローカル LLM MCP サーバ](03-commands/04-llm-mcp.md)）。
- `agent` は対象 worktree に前回の会話が残っていれば**前回セッションの続きから**起動し、無ければ通常起動します
  （CLI ごとの再開フラグ・キュー済みプロンプト（`session_prompt`）との両立可否など挙動の正本は
  [3.2 TUI 内コマンド#agent](03-commands/02-tui.md#agent)）。
- **セッション単位のエージェント CLI・モデル指定**: `session_create` / `session_delegate_issue`（MCP）で作成・委譲する際に
  任意の `agent_cli` / `model` を渡すと、その値が `state.json` の `SessionRecord.agent`（[data/02-workspace.md](data/02-workspace.md#セッションごとsessionrecord)）に記録され、そのセッションのエージェント起動時に**ワークスペースの実効設定 `agent_cli` より優先**して解決されます
  （在席からの起動・[ペインの復旧](#ペインの復旧)・[キュー済みプロンプトの自動起動](#キュー済みプロンプトの自動起動)のいずれでも同じ）。CLI 解決の優先順位は
  **在席での明示選択（`agent <name>`）＞セッションの `agent`＞ワークスペースの実効 `agent_cli`**、モデルはセッションの `agent.model`
  を各 CLI のモデルフラグ（claude `--model`、codex / gemini `-m`）へ展開します。コーディネータが「軽いタスクは小さいモデル、
  重い設計は大きいモデル」とタスクごとに振り分けるための仕組みです。未指定なら従来どおり実効設定と各 CLI の既定モデルに従います。

各 worktree のシェル / Agent は「ターミナルプール」が worktree パスをキーに保持し、画面を開いている間は
セッションを切り替えても裏で動き続けます。`session remove` でセッションを破棄するときは、その worktree
パス配下のシェル / Agent もプールから取り除いて終了させ、さらに**その worktree の会話履歴も削除します**
（Claude の transcript ディレクトリ `~/.claude/projects/<encoded>`、Codex の rollout transcript
`~/.codex/sessions/.../rollout-*.jsonl` のうち当該 worktree のもの、Gemini の chat transcript
`~/.gemini/tmp/<project>/chats` のうち当該 worktree のもの、Antigravity の入力履歴
`~/.gemini/antigravity-cli/history.jsonl` のうち当該 worktree の行）。あわせて usagi が**その worktree パスをキーに保存している
一時ファイルもすべて消します**——Agent phase（`~/.usagi/agent-state/`）・PR リンク（`~/.usagi/pr-links/`）・キュー済み
プロンプト（`~/.usagi/agent-prompts/` / `~/.usagi/agent-live-prompts/`）・ペイン構成（`~/.usagi/open-panes/`）。これらの削除はユースケース層の
`session remove` で行うため、TUI からでも CLI / MCP からでも漏れなく消えます（[data/01-global.md](data/01-global.md) 参照）。
worktree ディレクトリを消してもシェルや会話履歴・これらの一時ファイルは別の場所に残るため、明示的に消します。
これにより、同じ名前・同じパスでセッションを作り直しても前回の Agent・会話・状態を一切引き継がず、常にまっさらな
状態から始まります（`--continue` で復活する古い会話も残りません）。入力待ちになった（`◆ waiting`）／終了した（`✓ done`）セッションは
左ペインのマーカーとデスクトップ通知で知らされます。この埋め込みターミナルの永続化・切り替え・通知の詳細は
[design/home/04-keys.md](design/home/04-keys.md#没入のキー操作attached--terminal--agent-実行中) を参照してください。

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
  [design/home/04-keys.md](design/home/04-keys.md#使用中-agent-の表示入力待ちの検知と通知)）。
- `SessionStart` は起動・再開だけでなく**コンテキストのコンパクション後**にも発火します。自動コンパクションは**ターンの途中**でも起こり、その後 Agent は新たな `UserPromptSubmit` なしに作業を続けます。これを一律 `ready` にすると、稼働中のセッションが次の `Stop` まで `☾ ready` のまま固まってしまうため、usagi は次のいずれかに当たる `SessionStart` では **phase を書き換えず現状を維持**します（ターン中なら `running` のまま、待機中ならそのまま）。
  - payload の `source` が `compact`（明示的なコンパクション後の再開）。
  - 記録済みの phase が `running` / `waiting`（＝ターンの途中）。新規 spawn のたびに phase ファイルはクリアされる（[data/01-global.md](data/01-global.md) 参照）ため、**真の起動なら記録済み phase は無い**。途中にもかかわらず `ready` が来たのはターン中の再開（`source` を伴わないコンパクションや、`source` を読めなかった payload を含む）であり、`source: compact` の判定だけでは取りこぼすケースもこの条件で守られます。
- ツール使用の許可プロンプトも入力待ちですが、`Notification` はユーザーが**離席している**ときにしか発火しないため、それだけでは見ているセッションの許可待ちを取りこぼします。専用の `PermissionRequest` フックを `waiting` に割り当て、**プロンプト表示の直前に・フォーカス中でも**確実に `◆ waiting` へ遷移させます（観測のみで許可可否の判定には介入しません）。
- `Notification` はターン中の入力待ちだけでなく、**ターン完了後にプロンプト待ちでアイドルになったとき**にも発火します。この通知は `Stop`（`ended`）の**後**に届くため、そのまま `waiting` を記録すると完了済みセッションの `✓ done` が `◆ waiting` に巻き戻ってしまいます。そこで記録済み phase が `ended` のときは `Notification` → `waiting` を**書き換えず維持**します。真の（ターン中の）入力待ちは直前に必ず `UserPromptSubmit` → `running` を挟むため記録済み phase は `running` であり、このガードで取りこぼすことはありません（usagi のモデルでも「ターン完了」は `done` であって `waiting` ではありません）。Codex は `Notification` フックを持たないためこの巻き戻りは起きません。
- `running` を駆動するのは `UserPromptSubmit` だけではありません。**ターン中のツール呼び出し（`PreToolUse` / `PostToolUse`）も `running` に割り当て**ます。これは `◆ waiting` への貼り付きを解消するためです。ユーザーが質問に答えたり許可を承認したりして Agent が作業を再開しても、新たな `UserPromptSubmit` は発火しないので、`waiting` のままでは「実際は稼働中なのに `◆ waiting`」になってしまいます。再開後の最初のツール呼び出しで `PreToolUse` / `PostToolUse` が発火し、セッションを `▶ running` へ引き戻します。これらのフックは**ターンの途中でしか発火しない**ため、アイドル中のセッションを誤って `running` にすることはありません。
- 割り当てを見送ったフック: `SubagentStop`（サブエージェントの終了は本体ターンの終了ではない。本体は作業を続けており、`Task` ツールの `PostToolUse` が `running` を保つ）、`PreCompact` / `PostCompact`（コンパクションは上記の `SessionStart` ガードで処理され、再開後のツール呼び出しが改めて `running` を主張する）。
- `--settings` は**ユーザー自身の設定に追加マージ**されるため、既存の Claude 設定を壊しません。
- **Codex** も同じ仕組みで phase を報告します。上表のうち Codex が持つイベントは `SessionStart` / `UserPromptSubmit` / `PreToolUse` / `PostToolUse` / `PermissionRequest` / `Stop` で、`ready` / `running` / `waiting` / `ended` の割り当ては Claude と同一です（Codex には `Notification` / `SessionEnd` イベントが無く、`ended` は `Stop` が担います）。`SessionStart` の `source`（`startup` / `resume` / `clear` / `compact`）も Claude と同じ値なので、コンパクションガードもそのまま機能します。Codex のフックは**信頼されていない command フック**として扱われ既定では実行前に承認を求めるため、usagi は `--dangerously-bypass-hook-trust` を付けて起動します（フックが実行するのは usagi 自身のみ）。対話起動は `--sandbox workspace-write --ask-for-approval on-request` も付け、worktree 内の自動実行を許しつつ、サンドボックス外へのエスカレーションだけ許可待ちにします。usagi が注入する MCP サーバは `default_tools_approval_mode = "approve"` にし、MCP tool 呼び出しごとの確認は省きます。
- フックを持たない Agent（`gemini` など）はこの仕組みの対象外で、入力待ちは従来のターミナルベルで推定します。
- フック・MCP サーバが呼び戻す `usagi` は、`$PATH` 上の名前ではなく **usagi 自身の実行ファイルの絶対パス**
  （`std::env::current_exe()` で解決）を埋め込みます。これにより、インストール済みでも `cargo run` のように
  ビルド成果物（`target/debug/usagi`）を直接起動した場合でも、`usagi mcp` / `usagi agent-phase` が
  `command not found` にならず解決できます（パスが取得できない場合のみ素の名前 `usagi` にフォールバック）。

### worktree への閉じ込め（メインリポジトリ保護）

セッション worktree は**メインリポジトリの内側**（`<repo>/.usagi/sessions/<name>/`）に置かれるため、
リポジトリルートや別セッションの worktree がディスク上で 1 つ上の階層に並びます。Agent が `<repo>/src/...`
を編集したり親リポジトリへ `cd` したりすると、意図したセッションとは別のツリーを触ってしまいます。usagi は
これを 2 段階で防ぎます。

| 段 | 仕組み | 対象 | 効果 |
|---|---|---|---|
| ソフト | 「作業はこの worktree 配下だけで完結させ、親のメインリポジトリには触れない」旨を Agent に伝える。system prompt を持つ CLI は system prompt（`--append-system-prompt` ／ Codex の `developer_instructions`）で、持たない CLI は**開始プロンプト**（`-i` ／ headless の `-p`）の先頭にこの指示を置き、キュー済みプロンプトはその後ろに続ける | 全 CLI（Claude / Codex は system prompt、Gemini / Antigravity は開始プロンプト） | Agent に意図を伝える指示。強制力はない |
| ハード | `PreToolUse` フックに `usagi guard-workspace` を差し込み、ツール呼び出しを**拒否**する。判定は Agent の `cwd` によって 2 モードに分岐する（下記） | Claude | 指示を破った変更を実際にブロックする |

ハード側（`guard-workspace`）は `PreToolUse` の payload（stdin の JSON）から `cwd` を読み、それが
`.usagi/sessions/<name>/` 配下か（＝セッションの worktree か、pre-commit フックの命名規則免除と同じ判定基準）で
モードを選びます。

- **session モード**（`cwd` がセッション worktree の中）: payload の `tool_input.file_path`（ツールが触れようとする
  パス）を worktree 基準で正規化（`.` / `..` を解決）し、worktree の外に出る場合だけ拒否します。worktree 内・
  パスを持たないツール（`Bash` / `Grep` など）・解釈できない payload は素通しします。セッションは自分の worktree の
  中を自由に編集できます。
- **root モード**（`cwd` がワークスペースルート＝ `.usagi/sessions/` 配下でない。コーディネータの行）: root 行は
  リポジトリを一切変更しないため、閉じ込め（cwd == repo ルートなので「外」判定が働かない）に代えてより強く拒否
  します。
  - **ファイル書き込み系ツール**（`Edit` / `Write` / `MultiEdit` / `NotebookEdit`）を**パスに依らず**すべて拒否。
  - `Bash` のうち**リポジトリを変更する git**（`commit` / `add` / `push` / `merge` / `rebase` / `checkout -b` /
    `worktree add` など）を拒否。判定は読み取り系 git（`status` / `log` / `diff` / `show` など）の**許可リスト**で行い、
    それ以外の git サブコマンドは変更系とみなして拒否する（未知・曖昧な git を素通しさせない安全側）。git を含まない
    コマンドは素通しする。
  - 変更は root 行では行わず、セッションの worktree に委譲します。

拒否時は Claude の `PreToolUse` 契約どおり `permissionDecision: "deny"` を stdout に返します（理由も添える）。
許可する場合は何も出力せず、Claude の通常の許可フローに委ねます。

- `guard-workspace` は状態報告の `agent-phase` と同じ `PreToolUse` 配列に並べて差し込みます。Claude は同一イベントの
  フックをすべて実行し、いずれかが拒否すればツールはブロックされるため、状態報告と保護が両立します。

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
  復旧時にローカル設定の `env`（`op://...`）を解決する場合は、root row と各 session worktree が同じ workspace root に
  属していれば解決結果を共有し、同じ 1Password reference に対する `op read` を重複実行しません。
- 復旧は**ベストエフォート**です。スナップショットが無いセッションや、spawn に失敗したペインは黙って飛ばし、
  画面の起動を妨げません。設定を OFF にすると保存も復旧も行わず、常にまっさらな状態で起動します。

### 復帰フォーカス（いた場所の復元）

ペインの復旧が「どのペインが開いていたか」を呼び戻すのに対し、**復帰フォーカス**は「終了時にユーザーが
*どこにいたか*」を呼び戻します。同じ [`restore_panes_enabled`](05-settings.md#設定項目) で一括制御します
（両者で 1 つの「セッション状態を復元する」機能）。

- **保存**: 終了が確定した時（quit 確認モーダルの承認、即時 Ctrl-C、`:quit`）に、カーソルがあったセッションと
  その**エンゲージメント段階**（切替 / 在席 / 没入）をワークスペース別のスナップショットに記録します（保存
  フォーマットは [data/01-global.md#resume-focus復帰フォーカススナップショット](data/01-global.md#resume-focus復帰フォーカススナップショット)）。
  没入中の終了（既定の `prefix` 方式なら `Ctrl-O q`、`alt` 方式なら `Alt-q`）は確認モーダルへ抜ける際に在席へ降格するため、降格前に「没入だった」ことを記録しておきます。
- **復旧**: 起動時、ペインの復旧が済んだ後に読み出し、記録された段階へ戻します。切替ならカーソルをそのセッションへ、
  在席ならそのセッションを在席に、没入ならイベントループの初回パスで自動的に attach します（このときペインは
  既に復旧済みなので生きており、attach できます）。
- 復旧は**ベストエフォート**です。記録されたセッションが既に消えている（`session remove` された）場合は何も
  復元せず、既定の切替で起動します。設定を OFF にすると保存も復旧も行いません。

## キュー済みプロンプトの自動起動

コーディネータ役のエージェントが MCP `session_delegate_issue`（または `session_prompt` の queue チャネル）で
issue を新しいセッションへ委譲すると、そのプロンプトは[起動時キュー](03-commands/03-mcp.md#session_prompt-の挙動)
（`~/.usagi/agent-prompts/`）に積まれます。**キュー済みプロンプトの自動起動**は、ホーム画面がこのキューを検知したら、
対象セッションの agent ペインを**バックグラウンドで自動 spawn** して着手させる機能で、設定
[`autostart_queued_prompts`](05-settings.md#設定項目)（既定 ON）で切り替えます。人がそのセッションのペインを開く
までエージェントが走り出さない、という自律オーケストレーションのギャップを埋めます。

- **spawn の仕組み**は[ペインの復旧](#ペインの復旧)を流用します。ライブペインを持たないセッションに対し、
  記録された agent ペインではなく**キュー済みプロンプトを最初のメッセージ**として渡して agent CLI を起動します
  （対象 worktree に前回の会話が残っていれば復旧と同様に続きから再開）。attach しないため画面のフォーカスは
  奪いませんが、監視スレッドが拾うので左ペインのバッジ（`▶ running` / `◆ waiting` / `✓ done`）は動きます。
  ローカル設定の `env`（`op://...`）解決は復旧と同じく workspace root 単位で結果を共有します。
  **セッションに `agent`（CLI / モデル）の指定があればここで適用されます**——`session_delegate_issue(agent_cli, model)` で
  委譲したセッションは、この自動 spawn で**指定 CLI・指定モデル**で起動します（無指定ならワークスペースの実効 `agent_cli`
  と各 CLI の既定モデル）。これがセッション単位のモデル指定が最も効く経路です。
- **検知の契機**は 2 つです。(1) **起動時**: TUI が起動していない間にキューされたプロンプト（例: 別プロセスの
  エージェントが委譲したもの）を、次回起動時にペインの復旧・復帰フォーカスを済ませた後で拾って自動 spawn します。
  (2) **稼働中**: イベントループが定期的にキューを走査し、TUI 稼働中に委譲されたプロンプトを人の操作なしで拾います
  （キューが空の間はディレクトリ一覧の確認だけで済ませます）。
- 既にライブペインを持つセッションは対象外です。その場合キュー済みプロンプトは従来どおり、そのセッションの agent
  ペインを**次にフレッシュ起動したとき**に消費されます（[`session_prompt` の挙動](03-commands/03-mcp.md#session_prompt-の挙動)）。
- 起動は**ベストエフォート**です。プロンプトの取り出しは 1 回限り（one-shot）で、spawn に失敗した場合はキューへ
  積み直して後続の契機・人の操作に委ねます。設定を OFF にすると自動 spawn を一切行わず、上記の「次のフレッシュ
  起動時に消費」へ戻ります。
