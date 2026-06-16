# 4. オーケストレーション（セッション・worktree 管理）

> [ドキュメント目次](README.md) ｜ ← 前へ [3. コマンドリファレンス](03-commands/README.md) ｜ 次へ → [5. 設定](05-settings.md)

`usagi` の中核は、**複数の作業を worktree ベースの「セッション」として束ね、複数リポジトリ構成でも
一括でオーケストレーションする**ことです。本書はその概念モデルとライフサイクル、関連コマンドの役割を
まとめます。各コマンドの一覧は [3. コマンドリファレンス](03-commands/README.md)、永続化されるデータは
[data/02-workspace.md](data/02-workspace.md) を参照してください。

## 目次

- [用語](#用語)
- [なぜ worktree を 1 か所に集約するのか](#なぜ-worktree-を-1-か所に集約するのか)
- [セッションの構築（再帰走査と複数リポジトリ対応）](#セッションの構築再帰走査と複数リポジトリ対応)
  - [セッション名の指定（モーダル）](#セッション名の指定モーダル)
- [セッションのライフサイクル](#セッションのライフサイクル)
- [アクティブなワークスペースと AI 連携](#アクティブなワークスペースと-ai-連携)
- [関連コマンドの役割分担](#関連コマンドの役割分担)
- [実装状況](#実装状況)

## 用語

| 用語 | 意味 |
|---|---|
| ワークスペース | usagi に登録したプロジェクトのルートディレクトリ。git リポジトリでなくてもよい（複数リポジトリのルートでも可）。グローバルレジストリ `workspaces.json` に登録される |
| セッション | 1 つの作業単位。`session create <name>` でワークスペースルート配下に作られる worktree 群（＋コピー）の集合。名前 `<name>` で識別する |
| worktree | git の作業ツリー。各 git リポジトリにつき 1 つ、セッション用ブランチをチェックアウトして作られる |
| アクティブな worktree | `session switch` で選択中の作業対象。以降の `ai` / `terminal` / `diff` などの実行カレントディレクトリになる |

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
- **git 管理外のファイル・ディレクトリ** → 同じ相対パス `.usagi/sessions/<name>/<相対パス>/` へコピーする。

これにより、単一リポジトリだけでなく、ルートが git でない複数リポジトリ構成（モノレポ的な
ディレクトリツリー）にも対応できます。

#### 新ブランチの基点（local / remote）

新しい `<name>` ブランチを**どの基点から切るか**は、各リポジトリのローカル設定
`default_branch_source`（[05-settings.md](05-settings.md#ローカル設定プロジェクト単位の上書き)）で選べます。

- `local` → そのリポジトリのローカル既定ブランチ（例 `main`）。
- `remote` → リモート追従の既定ブランチ（例 `origin/main`）。**既定**。`origin/<既定>` が無ければローカル
  既定ブランチ → それも無ければ現在の HEAD にフォールバックします。

設定は**リポジトリ単位**です。複数リポジトリ構成では `session create` 実行時に各リポジトリの
`<repo>/.usagi/settings.json` をそれぞれ参照し、リポジトリごとに異なる基点で worktree を切れます。基点解決は
`infrastructure/git.rs` の `resolve_base_ref`、適用は `usecase/session` の `create` / `build_dir` が担います。

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

### セッション名の指定（モーダル）

`session create <name>` は引数でセッション名を渡せますが、**`session create` を名前なしで実行した場合は、
セッション名を入力するモーダルを画面中央に表示して名前を尋ねます**。

- `Enter` で入力を確定し、その名前でセッションを作成。`Esc` でキャンセル。
- 空文字や既存セッションと重複する名前はバリデーションし、モーダル内にエラーを表示して確定させません。
- モーダルは既存のモーダル基盤（`src/presentation/tui/widgets/` の `boxed` / `render_modal`、テキスト
  入力フィールド）を再利用し、ディレクトリ選択モーダル（[design/03-new.md](design/03-new.md#ディレクトリ選択モーダル)）と
  同じく中央寄せ・枠付きボックスで描画します。

## セッションのライフサイクル

セッションは「作成 → 作業 → 統合 → 破棄」の流れで完結します。

```text
  session create <name>     session switch <name> / ai / terminal     usagi sync
        │                              │                              │
        ▼                              ▼                              ▼
   [セッション作成] ───────────▶ [作業（worktree 上）] ◀──── [main の最新を取り込む]
        │                              │
        │                              ▼
        │                       usagi finish (--pr)
        │                              │
        │                              ▼
        └────────────────────▶ [main へ統合 → worktree 削除]
                                       │
                                       ▼
                                 usagi clean
                              [古いセッションの整理]
```

| 段階 | コマンド | 役割 |
|---|---|---|
| 作成 | `session create [<name>]` | ルートを再帰走査して `.usagi/sessions/<name>/` 配下に worktree 群を構築（名前省略時はモーダルで名前を尋ねる） |
| 一覧 | `session list` / `usagi list` | セッション一覧・各セッションの ahead/behind を俯瞰 |
| 切替 | `session switch <name>` | アクティブなセッションを切り替え（以降のコマンドの対象） |
| 同期 | `usagi sync` | origin の既定ブランチの最新をセッションへ取り込む |
| 統合 | `usagi finish` / `submit` | 変更を main へ統合し、worktree を削除（`--pr` で PR 作成） |
| 破棄 | `session remove <name>` / `usagi clean` | 不要になったセッションの worktree・ブランチ・コピーを削除 |

`session` のサブコマンドは短縮形を受け付けます（`create`=`c`/`new`、`list`=`ls`、`remove`=`rm`）。例: `session ls` / `session rm <name>`。

`session remove` / `clean` は未コミット変更がある場合に警告し、安全に削除します。

### state.json との同期（孤児セッションの掃除）

`session create` / `session remove` の実行時に、`.usagi/sessions/` 配下のディレクトリと `state.json` の記録を照合します。**`state.json` に記録のないディレクトリ**（中断された作成・手で編集された `state.json`・クラッシュなどで取り残されたもの）は「孤児」とみなし、**未コミット変更の有無にかかわらず強制削除**して同期を取ります（worktree の登録解除・セッションブランチの削除・コピーしたファイルの除去）。

- これにより、作成時は同名の取り残しディレクトリが新規セッションの作成を妨げません。
- 記録済みセッション本体の削除には引き続き未コミット変更のガード（`--force` 必須）が効きます。掃除されるのは **記録のない** ディレクトリだけです。
- セッションディレクトリ直下の単なるファイルは対象外です。

## アクティブなワークスペースと AI 連携

- `session switch` で選択したセッションが「アクティブ」になり、ホーム画面で視覚的に強調表示されます。
- 以降の `ai` / `terminal` / `diff` などは、アクティブな worktree をカレントディレクトリとして実行します。
- `ai <prompt>` は設定の Agent CLI（`claude` / `gemini`）を起動し、アクティブな worktree 配下の
  ファイルをコンテキストとして AI に指示を渡します。Agent CLI の選択は設定で行います
  （[5. 設定](05-settings.md)）。
- `usagi context` は AI に読み込ませるプロジェクト概要を生成し、`ai` や外部エージェントの入力に使えます。

## 関連コマンドの役割分担

| 関心事 | コマンド | 参照 |
|---|---|---|
| セッションの作成・一覧・削除 | `session` | [issue 003](../issues/003-session.md) |
| アクティブセッションの切替 | `session switch` | [issue 004](../issues/004-space.md) |
| AI への指示・対話 | `ai` | [issue 005](../issues/005-ai.md) |
| 対話ターミナル | `terminal` | [issue 006](../issues/006-terminal.md) |
| 埋め込みターミナルで Agent CLI 起動 | `agent` | [issue 026](../issues/026-agent.md) |
| 入力待ち検知と通知 | `terminal` / `agent`（監視） | [issue 028](../issues/028-agent-wait-notify.md) |
| main の同期 | `usagi sync` | [issue 009](../issues/009-sync.md) |
| 統合・破棄 | `usagi finish` / `clean` | [issue 010](../issues/010-finish.md) / [014](../issues/014-clean.md) |
| 俯瞰 | `usagi list` | [issue 011](../issues/011-list.md) |
| 差分閲覧 | `diff` | [issue 012](../issues/012-diff.md) |
| Issue 連携 | gh Issue → セッション | [issue 020](../issues/020-gh-issue.md) |

## 実装状況

- ✅ **worktree の集約場所**（`.usagi/sessions/<name>/`）と、`usagi status` による状態同期・表示は実装済み。
- ✅ **セッション作成**（`session create <name>` / `session create`）：ルートを再帰走査して git は worktree 構築・
  非 git はコピー（`usecase/session`、`infrastructure/git.rs` の `add_worktree`）。名前省略時は
  切替モードの左ペイン内インライン入力で作成（[design/05-home.md](design/05-home.md#切替switch)）。単一リポジトリ構成では
  作成後に `state.json` を再同期し worktree 一覧へ反映。
- ✅ **複数リポジトリ構成での state.json 集約表現**（`sessions` / `SessionRecord`）。ルートが git でなくてもセッションを追跡。
- ✅ **`session list`**：`state.json` の `sessions` を一覧表示（件数 + 各セッション名 + worktree 数）。
- ✅ **`session remove <name> [--force]`**：各リポジトリの worktree・ブランチを削除しコピーを掃除、`state.json` から除去。未コミット変更があれば削除せず警告し、`--force` で破棄。名前を省略すると一覧モーダルを開き、Space で選択/解除して Enter で複数セッションを一括削除。
- ✅ **`session switch <name>`**：アクティブセッションを切り替え＆在席へ（引数なしで切替モードを開く）。
- ✅ **`terminal`**：在席（選択中）の worktree（未選択時はワークスペースルート）で対話シェルを右ペインに埋め込み起動し没入へ（疑似ターミナル portable-pty + vt100）。没入中は **`Ctrl-O` だけが予約キー**で、切替モードへズームアウトして別セッションへ切り替え、もう一度 `Ctrl-O` で統括まで戻る（シェルは存続）（[issue 006](../issues/006-terminal.md)）。
- ✅ **`agent`**：`terminal` と同じ埋め込みシェルを起動し、実効設定の Agent CLI（既定 `claude`）を自動入力（実質 `terminal` → `claude`）。対応する Agent CLI には usagi の issue MCP サーバ（`usagi mcp`）を組み込んで起動し、エージェントが起動直後から issue を操作できる（Claude は `--mcp-config` で注入、Gemini は現状素のまま）。あわせて Claude にはセッション専用 worktree 内で起動している旨をシステムプロンプトとして注入し（`--append-system-prompt`）、エージェントが冗長な worktree 作成をせず作業ディレクトリでそのまま着手できるようにする。([issue 026](../issues/026-agent.md))。さらに **ローカル LLM が有効**（`local_llm.enabled`）なら、issue サーバと並べて `usagi-llm` MCP サーバ（`usagi llm-mcp`）も `--mcp-config` に組み込み、軽量タスクをローカル LLM へ委譲してクラウド Agent のトークン消費を抑えるよう促す（[3.4 ローカル LLM MCP サーバ](03-commands/04-llm-mcp.md)）。
- ✅ **入力待ち検知と通知**：常駐中（ターミナルプール）の各セッションのターミナルベルを監視スレッドが監視し、
  アタッチ中でないセッションが入力待ちになると左ペインに `◆` マーカー＋デスクトップ通知で知らせる。判定ロジックは
  純粋な `infrastructure/session_monitor.rs`（`SessionMonitor`）、監視・通知は `home/terminal_pool.rs` の
  `TerminalPool` に統合。([issue 028](../issues/028-agent-wait-notify.md))。
- 🚧 **同期・統合・AI 連携**（`ai` / `sync` / `finish` / `list` / `clean` / `diff`）は
  usagi.ai から移植予定です。詳細は各 issue を参照してください。

### 埋め込みターミナルの永続化

`terminal` / `agent` で開いた埋め込みシェルは、worktree パスをキーに **ターミナルプール**
（`presentation/tui/home/terminal_pool.rs` の `TerminalPool`）が保持します。プールはホーム画面を開いて
いる間ずっと生き続けるため、次の性質が成り立ちます。

- **離れても終了しない**：没入中に `Ctrl-O` で切替モードへズームアウトしても、シェルはプールに残った
  ままです。裏では出力スレッドが画面グリッドを更新し続けるので、`claude` などの長時間
  プロセスもそのまま進みます。
- **セッションをまたいで生存**：没入中に `Ctrl-O` で切替モードへズームアウトして別 worktree のターミナルへ
  切り替えても、元のシェルはプールに残ります。切り替え先に既存シェルがあれば再アタッチ、無ければその場で spawn します
  （`agent` 起動コマンドの自動入力は初回 spawn 時のみ）。
- **切り替え先の状態をそのまま表示**：切り替えはペインを開いたコマンド（`terminal` / `agent`）の
  agent フラグを引き継ぎません。既存シェルがあればその状態（`claude` 画面など）に再アタッチし、無ければ
  素のシェルを spawn します。そのため「セッション 1 はアイドル、セッション 2 は agent」のように、
  切り替え先ごとに本来の状態が見えます。
- **切替から新規作成**：切替モードで `c` を押すと左ペイン内のインライン名入力（`session create` と同じ）が開き、
  作成したセッションへ入ります。
- **破棄のタイミング**：シェル側で `exit` した場合、またはホーム画面を離れた（プールが drop された）場合に
  終了します。後者は開いていた全シェルをまとめて閉じます。

切り替え・作成ループ自体は純粋な `event.rs`（切替モードが選んだ行へカーソルを
動かして再ルート、`c` は左ペイン内のインライン名入力を経て新規セッションへ再ルート）にあり、テスト可能です。
PTY 本体とプールは I/O 専用のためカバレッジ対象外です。
キー操作の詳細は [3.2 TUI 内コマンド](03-commands/02-tui.md) と
[design/05-home.md](design/05-home.md#没入のキー操作attached--terminal--agent-実行中) を参照してください。
