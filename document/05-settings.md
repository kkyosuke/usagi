# 5. 設定

> [ドキュメント目次](README.md) ｜ ← 前へ [4. オーケストレーション](04-orchestration.md) ｜ 次へ → [6. 開発規約](06-conventions.md)

`usagi` の設定の全体像をまとめます。本書は「どんな設定があり、どこに保存され、どう変えるか」という
機能視点のガイドです。保存フォーマットの詳細は [data/01-global.md](data/01-global.md)、設定画面の
見た目・操作は [design/04-config.md](design/04-config.md) を参照してください。

## 目次

- [設定の保存場所](#設定の保存場所)
- [設定項目](#設定項目)
- [ローカル設定（プロジェクト単位の上書き）](#ローカル設定プロジェクト単位の上書き)
- [設定の変更方法](#設定の変更方法)
- [環境変数](#環境変数)
- [設定が効く場面](#設定が効く場面)

## 設定の保存場所

アプリ全体の設定は、グローバルなデータディレクトリ配下の `settings.json` に保存されます。

```
~/.usagi/settings.json        # USAGI_HOME が設定されていればそのディレクトリ配下
```

- 解決順は ① 環境変数 `USAGI_HOME` → ② `~/.usagi`（`infrastructure/storage.rs` の `data_dir()`）。
- アトミック書き込み（`*.tmp` → `rename`）で保存され、書き込み途中のクラッシュでも壊れません。
- ファイルが無い初回起動時は、すべて既定値として扱われます。

## 設定項目

| 項目 | JSON キー | 型 | 既定値 | 選択肢 / 意味 |
|---|---|---|---|---|
| テーマ | `theme` | enum | `system` | `light` / `dark` / `system`（OS 追従）の UI カラーテーマ |
| 既定ワークスペース | `default_workspace` | string? | `null` | 既定で開くワークスペース名。未設定なら `null` |
| クローン先ベース | `workspace_root` | string? | `null`（→ `~/git`） | 新規プロジェクトのクローン先ベースディレクトリ。未設定時は `~/git` にフォールバック |
| デスクトップ通知 | `notifications_enabled` | bool | `true` | バックグラウンドの `agent` が入力待ち・完了になった時のデスクトップ通知の ON/OFF |
| ペイン復旧 | `restore_panes_enabled` | bool | `true` | 起動時に各セッションの前回開いていたペイン（agent / terminal）をバックグラウンドで復旧し、終了時にいたセッションとエンゲージメント段階（切替 / 在席 / 没入）へ復帰する。agent は会話の続きから再開する（[4. オーケストレーション#ペインの復旧](04-orchestration.md#ペインの復旧)） |
| Agent CLI | `agent_cli` | enum | `claude` | 起動する AI エージェント CLI（`claude` / `codex` / `codex_fugu` / `gemini`）。`codex_fugu` は Codex 互換 CLI で `codex-fugu` を起動する |
| セッションアクション UI | `session_action_ui` | enum | `menu` | ホーム画面の[在席](design/home/02-layout.md#在席focus)で右ペインに出すアクション UI のスタイル。`menu`（選べるリスト）/ `prompt`（セッションスコープのコマンドライン） |
| サイドバー | `sidebar` | enum | `full` | ホーム画面の左セッション一覧を開く初期状態。`full`（全幅の一覧）/ `rail`（幅 5 桁に畳んだレール）。実行時は `Ctrl-B` で随時切り替えられる（[サイドバーの開閉](design/home/03-sidebar.md#サイドバーの開閉ctrl-b)） |
| 端末キー方式 | `key_scheme` | enum | `prefix` | 埋め込み端末（[没入](design/home/02-layout.md#没入attached)）がナビゲーション用に予約するキーの方式。`prefix`（`Ctrl-O` リーダー：`Ctrl-O` の次キーで操作。`Ctrl-O` 以外の Ctrl キーはシェル/エージェントへ流れる）/ `alt`（`Alt` 単打：bare Ctrl キーを一切奪わない。macOS は端末の Option=Meta 設定が前提） |
| マスコットの動き | `mascot_animation_enabled` | bool | `true` | ホーム画面サイドバーの[マスコットのうさぎ](design/home/02-layout.md#レイアウト)が操作に反応するかどうか。`true` で、切替 / 在席では操作のたびにまばたきし、没入では作業中の手をぴくぴく動かす。`false` にすると一切動かず静止画になる（うさぎ自体は表示される）。再描画はもともと起きる操作に乗せるだけでアイドル時のタイマーは持たない |
| 端末スクロールバック | `terminal_scrollback_lines` | usize | `2000` | 埋め込み端末ペインが保持するスクロールバック行数。**ライブなペインごとに 1 つ**確保されるため、セッション・ペインを多数開いたときの TUI メモリの主因。深い履歴が欲しければ上げ、メモリを抑えたければ下げる（上限 `50000`） |
| ローカル LLM 有効化 | `local_llm.enabled` | bool | `false` | 有効にすると `agent` 起動時に [ローカル LLM MCP サーバ](03-commands/04-llm-mcp.md)（`usagi-llm`）を wire し、軽量タスクをローカル LLM に委譲できる |
| ローカル LLM モデル | `local_llm.model` | string | `qwen2.5-coder:7b` | 委譲先の Ollama モデル名（`qwen2.5-coder:7b` / `:3b` / `:1.5b` / `qwen2.5:7b`） |
| 1Password MCP 有効化 | `op_mcp.enabled` | bool | `false` | 有効にすると `agent` 起動時に [1Password MCP サーバ](03-commands/05-op-mcp.md)（`usagi-op`）を wire し、エージェントが `op` 経由で secret を読み取れる。1Password サービスアカウントトークン本体は OS のシークレットストアに保存する |
| 同梱スキル機能 | `skill_features` | map<string, bool> | `{}` | usagi が各セッションに配布する[同梱スキル](04-orchestration.md#スキルの配布)を**機能（feature）単位**で ON/OFF する。キーは機能 ID（現状 `pull-request`：PR 作成・更新・修正の 3 スキルをまとめたもの）、値が有効・無効。既定値（ON）と同じ機能はマップに残さない（未記載 = 既定）。`usagi-session` は usagi 固有の常時 ON スキルで、この設定の対象外 |

> **同梱スキル機能（`skill_features`）**: usagi はビルド時に埋め込んだ Claude Code スキルを、起動する
> エージェントへ配布します（[スキルの配布](04-orchestration.md#スキルの配布)）。このうち**機能ごとにまとめた
> グループ**を ON/OFF できます。`pull-request` 機能（`usagi-pr-create` / `usagi-pr-update` /
> `usagi-pr-fix`）が現状の対象で、OFF にするとそのセッションの worktree にこれらのスキルを symlink しません。
> 既定はすべて ON（同梱スキルはオプトアウト）。`usagi-session` は機能に属さず常に配布されます。

> ローカル LLM は **オプトイン**（既定 `false`）です。資材は Config 画面で **2 段階**に導入します:
> まず `Local LLM` 行の Install アクション（`Space` / `Enter` でモーダルを開き sudo パスワードを入力 →
> `ollama` ランタイムを導入）、次に `Local LLM Model` 行のモデル選択モーダル（一覧から選び、未導入のモデルは
> その場で `ollama pull`）。いずれも**バックグラウンドで進み、導入中も usagi の他機能を操作できます**（進行は
> 全画面共通の[ローディングうさぎ](design/04-config.md#インストール中のローディングうさぎ)で表示）。
> `usagi doctor --fix` はランタイムと既定モデルをまとめて導入します。詳細は
> [Config 画面のローカル LLM 導入](design/04-config.md) / [3.4 ローカル LLM MCP サーバ](03-commands/04-llm-mcp.md)。

> **セキュリティ注記**:
> - ランタイム導入は ollama 公式の `curl -fsSL https://ollama.com/install.sh | sh` を **sudo 権限で** 実行します。
>   HTTPS で取得しますが、取得スクリプトの内容に対する usagi 側のチェックサム/署名検証はありません（上流の手順に準拠）。
>   ollama.com / CDN / DNS が侵害された場合は任意コードが実行され得る点に留意してください。
> - `local_llm.model` は上表の allowlist の値のみ有効です。`settings.json` を手編集して allowlist 外の値を入れた場合、
>   ロード時に既定（`qwen2.5-coder:7b`）へ戻されます（model 名はエージェント起動コマンドに展開されるため）。

> **1Password トークン**: 1Password サービスアカウントトークンは `settings.json` には保存せず、OS 固有の
> シークレットストア（macOS Keychain / Linux Secret Service）に保存します。登録は `usagi op login`、削除は
> `usagi op logout`、状態確認は `usagi op status` で行います。`login` はトークンをシークレットストアに保存したうえで
> `op_mcp.enabled` を `true` にし、`logout` はトークンを削除して `false` に戻します。
>
> `usagi op-mcp` は起動時にシークレットストアからトークンを読み取り、`op` サブプロセスへ環境変数
> `OP_SERVICE_ACCOUNT_TOKEN` として渡します（**エージェントの起動コマンド行やプロセス一覧には出ません**）。
> `usagi config`（表示）はトークン値を表示せず、`op_mcp.enabled` だけを示します。実際にトークンが保存されているかは
> `usagi op status` で確認します。

> すべての項目はフォーマットバージョン `version: 1` とともに `settings.json` に格納されます。
> 完全な JSON 例は [data/01-global.md](data/01-global.md#settingsjson) を参照してください。

## ローカル設定（プロジェクト単位の上書き）

グローバル設定のうち **プロジェクトごとに変えたい項目だけ** を、各リポジトリの
`<repo>/.usagi/settings.json` で上書きできます（ローカル設定）。コミットされず、マシンごとに保持されます。

| 項目 | JSON キー | 型 | 未設定時 |
|---|---|---|---|
| Agent CLI | `agent_cli` | enum? | グローバル設定にフォールバック |
| デスクトップ通知 | `notifications_enabled` | bool? | グローバル設定にフォールバック |
| ペイン復旧 | `restore_panes_enabled` | bool? | グローバル設定にフォールバック |
| デフォルトブランチ | `default_branch` | string? | リポジトリの検出済み既定ブランチ（auto） |
| デフォルトブランチ基点 | `default_branch_source` | enum? | 既定（`remote`） |
| ローカル LLM 有効化 | `local_llm_enabled` | bool? | グローバル設定（`local_llm.enabled`）にフォールバック |
| 同梱スキル機能 | `skill_features` | map<string, bool> | 機能 ID 単位で上書き。未記載の機能はグローバル設定にフォールバック |

> **デフォルトブランチ（`default_branch`）**: `session create` でセッションを作るとき、各 git リポジトリの
> worktree を切る新ブランチを**どのブランチから**切るかを選びます。未設定（`null` = auto）ならリポジトリの
> 検出済み既定ブランチ（`origin/HEAD` → 現在のブランチ → `main` の順で解決）を使い、`develop` のように
> 値を指定するとそのブランチを基点にします。Config 画面では対象リポジトリの**実在ブランチ**を検出して
> 選択肢に並べます（`auto` ＋ 各ブランチ名）。

> **デフォルトブランチ基点（`default_branch_source`）**: 上で選んだブランチを**ローカル形・リモート形の
> どちらで**基点にするかを選びます。選択肢は `local`（ローカルのブランチ。例 `develop`）と `remote`（リモート
> 追従のブランチ。例 `origin/develop`）。グローバル設定に対応項目はなく、未設定時は `remote` として扱います
> （`origin/<ブランチ>` が無ければローカルのブランチ → それも無ければ現在の HEAD にフォールバック）。
> いずれもワークスペースのローカル設定（`<workspace>/.usagi/settings.json`）に保存され、ホーム画面の
> `config` から編集します。

- 全フィールドが任意（`Option`）で、`null` は「グローバル設定に従う」を意味します。テーマ（`theme`）や
  クローン先（`workspace_root`）のようにプロジェクト単位で変える意味の薄い項目は対象外です。
- **実効設定 = グローバル設定にローカルの上書きを適用した結果**。解決は `domain/settings.rs` の
  `Settings::with_local`、ユースケースは `usecase/settings.rs` の `effective(storage, repo_root)` が担います。
- 読み書きロジック・永続化に加え、編集 UI も実装済みです。ホーム画面のコマンドモードで `config` を実行すると
  設定画面が**ワークスペーススコープ**で開き、「Agent CLI」「Notifications」「Restore Panes」「Default Branch」
  「Branch Source」と、固定項目の下に並ぶ**同梱スキル機能**（`PR Skills` など）を編集できます。Agent CLI /
  Notifications / Restore Panes と各スキル機能は **「グローバルに従う / ローカルで上書き（On/Off）」**、Default
  Branch は **`auto`（検出済み既定）／ リポジトリの各ブランチ**、Branch Source は **`local` / `remote`** を
  切り替えられます。詳細は [design/04-config.md](design/04-config.md) を参照。
- JSON 例・フィールド詳細は [data/02-workspace.md](data/02-workspace.md#settingsjson-プロジェクト固有の設定上書きローカル設定) を参照。

## 設定の変更方法

### 設定画面（Config）

設定画面は **開いた場所でスコープが分かれます**。

- `usagi hop` の起動画面で `Config`（`c`）を選ぶ → **グローバルスコープ**。アプリ全体の設定
  （`~/.usagi/settings.json`）を編集します。
- ホーム画面のコマンドモードで `config` を実行する → **ワークスペーススコープ**。起動中のワークスペースの
  ローカル設定（`<workspace>/.usagi/settings.json`）だけを編集します。

どちらのスコープも操作は共通です。

- 各項目は `< 値 >` の左右セレクタ。`↑↓` で項目移動、`←→` で値の切り替え。
- 変更はメモリ上に保持され、未保存の項目はラベル左の黄色 `●` と黄色の値で明示されます。
- 末尾の **Save ボタン**で `Enter` を押すとそのスコープの `settings.json` へ保存します（変更があるときだけ有効）。

操作の詳細・レイアウトは [design/04-config.md](design/04-config.md) を参照してください。

> グローバルスコープで編集できるのは Theme / Default Workspace / Notifications / Restore Panes /
> Agent CLI / Session Action UI / Terminal Keys / Local LLM 系の各項目（`key_scheme` は
> 「Session Action UI」と「Local LLM」の間の **Terminal Keys** 行）に加え、固定項目の下に並ぶ**同梱スキル機能**
> （`PR Skills` など）。ワークスペーススコープで編集できるのは Agent CLI / Notifications / Restore Panes /
> Default Branch / Branch Source と、同じく**同梱スキル機能**です。
> `workspace_root` は `settings.json` に保存されますが、設定画面では編集せず、`usagi config --edit` で変更します。

### CLI

CLI からも設定を確認・編集できます（[3. コマンドリファレンス](03-commands/README.md)）。ただし `config` は通常の導線ではなく、起動画面の Config に揃えるため `usagi --help` には表示しない上級者向けコマンドです。

- `usagi config` — 現在のグローバル設定を `key  value` 形式で一覧表示（同梱スキル機能は `skill:<機能 ID>  true/false` の行で表示）。
- `usagi config --edit` — 設定ファイルを `$EDITOR`（→ `$VISUAL` → OS 既定）で開いて編集。保存後に
  再パースで形式（JSON 構文・必須 `version`・各フィールドの型）を検証し、不正なら編集前の内容へ
  巻き戻します。

## 環境変数

| 環境変数 | 役割 |
|---|---|
| `USAGI_HOME` | グローバルデータディレクトリ（`workspaces.json` / `settings.json` の置き場）を上書きする。未設定なら `~/.usagi` |
| `USAGI_TRACE` | 操作トレース（`logs/trace-YYYY-MM-DD.jsonl`）の記録を有効化する。空でも `0` でもない値で ON、未設定なら OFF（[data/01-global.md#logs操作トレース](data/01-global.md#logs操作トレース)） |
| `NO_COLOR` | 値が**空でなければ**色出力を抑制する（[no-color.org](https://no-color.org/)。CLI・TUI 両方に効く）。`CLICOLOR_FORCE` が色を強制している（空でも `0` でもない値）ときは無視される |
| `CLICOLOR_FORCE` | 空でも `0` でもない値なら色出力を強制し、`NO_COLOR` より優先する |

## 設定が効く場面

| 設定 | 効く場面 |
|---|---|
| `theme` | TUI 全体の配色 |
| `default_workspace` | 起動時に既定で開くワークスペースの選択 |
| `workspace_root` | 新規プロジェクト画面（Clone）の Location 既定値（[design/03-new.md](design/03-new.md)） |
| `notifications_enabled` | バックグラウンドの `agent` が入力待ち・完了になった時のデスクトップ通知の表示可否 |
| `restore_panes_enabled` | 起動時に各セッションのペイン（agent / terminal）を復旧し、終了時にいたセッション・エンゲージメント段階へ復帰するかどうか（[4. オーケストレーション#ペインの復旧](04-orchestration.md#ペインの復旧)） |
| `agent_cli` | `agent` コマンドが起動する AI エージェント CLI の選択（[4. オーケストレーション](04-orchestration.md)） |
| `session_action_ui` | ホーム画面の[在席](design/home/02-layout.md#在席focus)で右ペインに出すアクション UI（`menu` / `prompt`）の選択 |
| `sidebar` | ホーム画面の左セッション一覧を開く初期状態（`full` / `rail`）。実行時は `Ctrl-B` で切り替え（[サイドバーの開閉](design/home/03-sidebar.md#サイドバーの開閉ctrl-b)） |
| `key_scheme` | 埋め込み端末（[没入](design/home/02-layout.md#没入attached)）がナビゲーション用に予約するキー方式（`prefix` / `alt`）の選択 |
| `terminal_scrollback_lines` | 埋め込み端末ペインが保持するスクロールバック行数。ライブなペインごとに確保されるため、多数のセッションを開いたときのメモリ使用量を左右する |
| `local_llm.enabled` / `local_llm.model` | 有効時、`agent` 起動コマンドに `usagi-llm` MCP サーバを追加し、軽量タスクをローカル LLM に委譲する（[3.4 ローカル LLM MCP サーバ](03-commands/04-llm-mcp.md)） |
| `op_mcp.enabled` | 有効時、`agent` 起動コマンドに `usagi-op` MCP サーバを追加し、エージェントが `op` 経由で secret を読み取れるようにする（[3.5 1Password MCP サーバ](03-commands/05-op-mcp.md)） |
| `skill_features` | `session create` 時に、機能が有効な[同梱スキル](04-orchestration.md#スキルの配布)だけを各 worktree の `.claude/skills/` へ symlink する。無効な機能のスキルは配布しない（`usagi-session` は常時配布） |

> ホーム画面の `config` で `session_action_ui` や `key_scheme` を変更すると、設定画面を閉じて
> ホームに戻った時点で実効設定を読み直し、[在席](design/home/02-layout.md#在席focus)の右ペインや
> [没入](design/home/02-layout.md#没入attached)のキー処理に反映します（ホーム画面を開き直す必要はありません）。

> 設定の永続化は `usecase/settings.rs`（`load` / `save` / 各 `set_*`）と
> `infrastructure/storage.rs`（`Storage`）に実装されています。
